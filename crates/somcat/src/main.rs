//! `somcat` — a minimal terminal image viewer for Som. Speaks Som's own
//! rich-content protocol (`terminal::rich_content_transport`), with no
//! dependency on `crossterm` or any other console-mode abstraction library.
//!
//! Usage: `somcat <file>` or `somcat --srp <file>` (the flag is an explicit
//! synonym for the default, for anyone who'd rather not rely on an implicit
//! default).
mod raw_mode;

use image::ImageDecoder as _;
use image::codecs::gif::GifDecoder;
use std::io::Read as _;
use terminal::kitty_graphics_placeholder;

/// Chunk size for a single APC string's payload — large enough to be
/// efficient, small enough that no real terminal's escape-sequence parser
/// chokes on a single control string.
const CHUNK_SIZE: usize = 4096;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let explicit_srp = args.first().map(String::as_str) == Some("--srp");
    let path_arg = if explicit_srp { args.get(1) } else { args.first() };
    let Some(path) = path_arg else {
        eprintln!("usage: somcat <file>  (or: somcat --srp <file>)");
        std::process::exit(2);
    };

    // Raw mode is needed here (not just `enable_output_vt_processing()`)
    // because `stream_file` reads Som's `CSI 16 t` cell-size reply off
    // stdin — see `query_cell_size_px`'s doc comment for why the terminal's
    // own cell metrics (not Som's) must be the source of the placeholder
    // grid's row/column count.
    let raw_guard = raw_mode::enable();
    let result = stream_file(path);
    drop(raw_guard);
    if let Err(err) = result {
        eprintln!("somcat: failed to stream {path}: {err}");
        std::process::exit(1);
    }
}

/// Reads just enough of a GIF file to describe it — natural pixel
/// dimensions (via `ImageDecoder::dimensions`, which reads the fixed-size
/// logical screen descriptor without touching any frame's compressed image
/// data) and whether it's animated (more than one frame).
///
/// Scans the raw file bytes directly for a second GIF Image Descriptor
/// block (`0x2C`, the per-frame marker every GIF frame's data starts with)
/// after the fixed logical screen descriptor + optional global color table
/// — finding one proves a second frame exists without decoding either
/// frame's pixels. Not a general-purpose GIF parser (doesn't walk extension
/// blocks precisely byte-for-byte) — a plain byte scan for `0x2C` after the
/// header is good enough for "more than one frame," since `0x2C` cannot
/// legitimately appear as a sub-block length/data byte in the handful of
/// extension block shapes real encoders emit before the first frame.
fn gif_metadata(bytes: &[u8]) -> Result<(u32, u32, bool), String> {
    let decoder = GifDecoder::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let (width, height) = decoder.dimensions();

    let frame_marker_count = bytes.iter().filter(|&&b| b == 0x2C).count();
    let is_animated = frame_marker_count > 1;

    Ok((width, height, is_animated))
}

/// Reads a static (JPEG/PNG) image's pixel dimensions from its header —
/// `image::image_dimensions` only parses the fixed-size header block for
/// whichever format it detects, it doesn't decode any pixel data. Always
/// `is_animated = false`: neither format this function handles has a
/// multi-frame story SRP wires up (a PNG with an `acTL`/`fdAT`
/// animation extension would still just show its static fallback frame).
fn static_image_metadata(bytes: &[u8]) -> Result<(u32, u32, bool), String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let (width, height) = reader.into_dimensions().map_err(|e| e.to_string())?;
    Ok((width, height, false))
}

/// Sends `CSI 16 t` ("report cell size in pixels") and waits briefly for
/// the terminal's `CSI 6 ; height ; width t` reply. Real terminals that
/// implement this (Som included — see `alacritty_terminal::Term::
/// cell_size_pixels`, answered through `Terminal::process_event`'s
/// `TextAreaSizeRequest` arm) answer within milliseconds; not answering at
/// all within this timeout means the terminal doesn't support the query,
/// same fallback-on-silence assumption `somcat`'s old Kitty capability
/// query used.
///
/// This is why `main()` puts stdin in raw mode for the whole process
/// lifetime rather than only around this call: reading the reply requires
/// `ENABLE_VIRTUAL_TERMINAL_INPUT` (see `raw_mode`'s doc comment), and
/// there's no interactive session afterward whose console mode would need
/// to stay untouched.
fn query_cell_size_px() -> Option<(u32, u32)> {
    print!("\x1b[16t");
    std::io::Write::flush(&mut std::io::stdout()).ok()?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    if buf.last() == Some(&b't') || buf.len() >= 64 {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = tx.send(buf);
    });

    let buf = rx.recv_timeout(std::time::Duration::from_millis(800)).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // Expected shape: `ESC [ 6 ; <height> ; <width> t` — find our own
    // reply even if earlier unrelated bytes (a stray keypress, another
    // query's answer) happen to share the buffer.
    let start = text.find("\x1b[6;")?;
    let rest = &text[start + 4..];
    let end = rest.find('t')?;
    let mut parts = rest[..end].split(';');
    let height: u32 = parts.next()?.parse().ok()?;
    let width: u32 = parts.next()?.parse().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// Sends `CSI 18 t` ("report text area size in characters") and waits for
/// the terminal's `CSI 8 ; lines ; cols t` reply — same request/response
/// shape as [`query_cell_size_px`], just asking for the grid's own
/// dimensions instead of a single cell's pixel size.
///
/// This is why a placeholder grid can't be sized from `width_px`/`cell_width`
/// alone: an image's pixel dimensions are physical file pixels, while
/// `cell_width`/`cell_height` (from `CSI 16 t`) are GPUI's logical (DIP)
/// pixels — unrelated units once a monitor's DPI scale isn't 1.0. Dividing
/// one by the other can produce a `columns` count wider than the terminal
/// actually has, which the real terminal then silently wraps mid-grid,
/// scrambling every cell's row/column coordinates. Clamping against the
/// terminal's own reported character grid size sidesteps the unit mismatch
/// entirely, regardless of what it turns out to be.
fn query_cell_count() -> Option<(u32, u32)> {
    print!("\x1b[18t");
    std::io::Write::flush(&mut std::io::stdout()).ok()?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    if buf.last() == Some(&b't') || buf.len() >= 64 {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = tx.send(buf);
    });

    let buf = rx.recv_timeout(std::time::Duration::from_millis(800)).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // Expected shape: `ESC [ 8 ; <lines> ; <cols> t`.
    let start = text.find("\x1b[8;")?;
    let rest = &text[start + 4..];
    let end = rest.find('t')?;
    let mut parts = rest[..end].split(';');
    let lines: u32 = parts.next()?.parse().ok()?;
    let cols: u32 = parts.next()?.parse().ok()?;
    if cols == 0 || lines == 0 {
        return None;
    }
    Some((cols, lines))
}

/// Foreground carries session_id, underline color carries file_id — same
/// split Kitty's own encoding uses for (image_id, placement_id), mirrored
/// from `crates/terminal/src/terminal.rs`'s private `id_to_rgb` (kept as a
/// small local duplicate rather than a cross-crate `pub` export — this is
/// the only other call site, and the encoding is a stable, documented part
/// of the wire format, not an implementation detail worth coupling crates
/// over).
fn id_to_rgb(id: u32) -> (u8, u8, u8) {
    (((id >> 16) & 0xFF) as u8, ((id >> 8) & 0xFF) as u8, (id & 0xFF) as u8)
}

/// Prints the Unicode-placeholder grid for a `(session_id, file_id)`
/// placement directly to this process's own stdout — i.e. through the
/// SAME real PTY channel the shell's own prompt uses, not injected into
/// Som's terminal model out-of-band. This is why the cursor lands in the
/// right place with no further hackery: from the shell's perspective, this
/// is just ordinary text a child process printed, so its own model of
/// "where is the cursor" stays correct, and its next prompt naturally
/// prints below it.
fn print_placeholder_grid(session_id: u32, file_id: u32, width_px: u32, height_px: u32) -> Result<(), String> {
    let Some((cell_width, cell_height)) = query_cell_size_px() else {
        // No reply — fall back to not printing anything rather than
        // guessing cell metrics; the image is still cached and paintable
        // once Som resolves the placement some other way (or a future
        // client-side default), but a wrong footprint here would be worse
        // than none.
        return Ok(());
    };

    let mut columns = width_px.div_ceil(cell_width).max(1).min(297);
    let mut rows = height_px.div_ceil(cell_height).max(1).min(297);

    // `columns`/`rows` above assume the image's pixel dimensions and the
    // terminal cell's pixel dimensions share the same unit — true at DPI
    // scale 1.0, false on a scaled (e.g. 4K) display where `cell_width`
    // came back in GPUI's logical pixels while `width_px` is the image
    // file's physical pixel count. When that mismatch makes the grid wider
    // than the terminal actually is, the real terminal wraps it mid-row,
    // scrambling every placeholder cell's decoded (row, column). Query the
    // terminal's own character grid size and scale the whole placement
    // down (preserving aspect ratio) to fit both axes, rather than
    // trusting the pixel-based math alone. Height is clamped to
    // `terminal_rows - 1`, not `terminal_rows`, so a placement always
    // leaves at least one real row free below it for the shell's next
    // prompt — a placement that filled the screen edge-to-edge would push
    // that prompt out of view entirely until the user scrolled, which
    // defeats the whole point of printing it inline.
    if let Some((terminal_columns, terminal_rows)) = query_cell_count() {
        if columns > terminal_columns {
            let scale = terminal_columns as f64 / columns as f64;
            columns = terminal_columns.max(1);
            rows = ((rows as f64 * scale).floor() as u32).max(1);
        }
        let max_rows = terminal_rows.saturating_sub(1).max(1);
        if rows > max_rows {
            let scale = max_rows as f64 / rows as f64;
            rows = max_rows;
            columns = ((columns as f64 * scale).floor() as u32).max(1);
        }
    }

    // Hard `\r\n` between rows: the terminal's own soft-wrap reflow always
    // re-wraps to ITS current width, not the image's — it can't hold a
    // placement at a narrower width while the window is wider, which would
    // stretch the image past its real aspect ratio. Som instead actively
    // re-derives and rewrites this grid's cells in place on every resize
    // (see `Terminal::resync_rich_content_placements`), so staying with
    // explicit row boundaries here keeps the on-screen shape predictable
    // between resizes rather than relying on wrap behavior this protocol
    // doesn't want.
    let mut text = String::new();
    let (sr, sg, sb) = id_to_rgb(session_id);
    let (fr, fg, fb) = id_to_rgb(file_id);
    text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
    for row in 0..rows {
        for column in 0..columns {
            if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                text.extend(cell);
            }
        }
        if row + 1 < rows {
            text.push_str("\r\n");
        }
    }
    text.push_str("\x1b[0m\r\n");
    write_raw_stdout(text.as_bytes())
}

/// Streams `path` to Som's own binary rich-content protocol
/// (`terminal::rich_content_transport`) — the file's raw bytes go over the
/// wire base91-encoded (see that module's doc comment for why), chunk by
/// chunk, with no re-encoding at all: the receiving side
/// (`terminal::rich_content_cache`/`rich_content_gif_player`/
/// `rich_content_static_image_player`) decodes the file format itself. GIF
/// decodes progressively as chunks arrive; JPEG/PNG (no progressive-prefix
/// decode story in the `image` crate) wait for the whole file to land.
///
/// Content type is inferred from the file extension: `.gif`, `.jpg`/
/// `.jpeg`, `.png` (audio/markdown/video are reserved `ContentType`
/// variants, not wired up to any extension yet).
///
/// Once the transfer completes, THIS process prints the Unicode-placeholder
/// grid itself (see `print_placeholder_grid`) — deliberately not Som,
/// which used to inject this text into its own terminal model out-of-band
/// after the fact. That out-of-band write never reached the real PTY, so
/// the actual shell (PowerShell, etc.) never learned the cursor had moved:
/// its own line editor kept redrawing from its last-known (stale) cursor
/// position, visibly fighting Som's cursor placement. Printing through
/// this process's own stdout is ordinary child-process output as far as
/// the shell and ConPTY are concerned, so there's nothing to reconcile.
fn stream_file(path: &str) -> Result<(), String> {
    use terminal::rich_content_transport::{Chunk, ContentMetadata, ContentType, build_envelope, split_into_chunks};

    let extension = std::path::Path::new(path).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
    let content_type = match extension.as_deref() {
        Some("gif") => ContentType::Gif,
        Some("jpg" | "jpeg") => ContentType::Jpeg,
        Some("png") => ContentType::Png,
        Some(other) => {
            return Err(format!("unrecognized extension .{other} — only .gif/.jpg/.jpeg/.png are supported so far"));
        },
        None => return Err("file has no extension, can't infer content type".to_string()),
    };

    let bytes = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
    let total_size = bytes.len() as u64;

    let (width_px, height_px, is_animated) =
        if content_type == ContentType::Gif { gif_metadata(&bytes)? } else { static_image_metadata(&bytes)? };
    // Every pixel this protocol sends is decoded to RGBA before reaching
    // the wire (see `rich_content_gif_player`/`gpui::RenderImage`'s own
    // frame buffers) — 32 bits per pixel regardless of the source GIF's
    // own (typically <=8-bit indexed/palette) on-disk color depth.
    let metadata = ContentMetadata::Image { width_px, height_px, color_bits: 32, is_animated };

    // session_id and file_id both derived from the current time so two
    // separate `somcat` invocations rarely collide on the receiving side's
    // `(session_id, file_id)` cache key. Masked to 24 bits — this process
    // encodes each id into a placeholder cell's RGB color (`id_to_rgb`
    // above), which only has 24 bits to work with; an unmasked id here
    // would silently lose its high byte on the round trip through the
    // grid, making the id painted next to the image never match the id
    // this process actually sent.
    let now_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u32).unwrap_or(1);
    let session_id = (now_ms & 0xFF_FFFF).max(1);
    let file_id = (now_ms.wrapping_mul(2_654_435_761) & 0xFF_FFFF).max(1); // Knuth multiplicative hash, cheap decorrelation from session_id.

    let pieces = split_into_chunks(&bytes, CHUNK_SIZE);
    let mut offset = 0u64;
    for payload in pieces {
        let payload_len = payload.len() as u64;
        let chunk = Chunk { content_type, session_id, file_id, chunk_offset: offset, total_size, metadata, payload };
        // `build_envelope` already produces the complete envelope
        // (marker + header + base91-encoded payload) — this just wraps it
        // in the APC start/end sequence (`ESC _` ... `ESC \`).
        let envelope = build_envelope(&chunk);
        let mut apc = Vec::with_capacity(2 + envelope.len() + 2);
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&envelope);
        apc.extend_from_slice(&[0x1B, b'\\']);
        write_raw_stdout(&apc)?;
        offset += payload_len;
    }

    print_placeholder_grid(session_id, file_id, width_px, height_px)
}

/// Writes `bytes` to stdout bypassing `std::io::Stdout` entirely — see this
/// function's own platform-specific implementations for why.
///
/// # Why not `std::io::stdout().write_all()`/`.flush()`
///
/// Found the hard way (a live headless `#[gpui::test]` benchmark hung
/// indefinitely, reproducing consistently through a real ConPTY but NEVER
/// through a plain OS pipe redirect in isolation — the exact difference
/// that took real instrumentation, not guessing, to pin down):
/// `std::io::Stdout` wraps its underlying handle in a `LineWriter`, which
/// only flushes its internal userspace buffer up to the last `\n` byte it
/// has seen — see the standard library's own `io::stdio::Stdout` doc
/// comment ("FIXME: this should be LineWriter or BufWriter depending on the
/// state of stdout"). This protocol's binary envelopes essentially never
/// contain a real `\n` (0x0A) byte, so `write_all` alone leaves most of
/// what was "written" sitting in that userspace buffer, never actually
/// reaching the OS pipe underneath. Calling `.flush()` afterward to force
/// it out is what actually hung — confirmed directly: `write_all` itself
/// reliably returned `Ok`, but the very next `.flush()` call never returned
/// at all, on the very first (113-byte) chunk, every time.
#[cfg(windows)]
fn write_raw_stdout(bytes: &[u8]) -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.map_err(|e| e.to_string())?;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let mut written = 0u32;
        unsafe { WriteFile(handle, Some(&bytes[offset..]), Some(&mut written), None) }
            .map_err(|e| format!("WriteFile: {e}"))?;
        if written == 0 {
            return Err("WriteFile wrote 0 bytes without erroring — treating as a stalled pipe".to_string());
        }
        offset += written as usize;
    }
    Ok(())
}

#[cfg(unix)]
fn write_raw_stdout(bytes: &[u8]) -> Result<(), String> {
    // Unix's `Stdout` has no `LineWriter`-buffering surprise the way
    // Windows' does for a non-console (pipe/pty) target — `write_all`
    // alone is sufficient here.
    use std::io::Write as _;
    std::io::stdout().write_all(bytes).map_err(|e| e.to_string())
}
