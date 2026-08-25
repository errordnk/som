//! `somcat` — a minimal terminal image/audio viewer for Som. Speaks Som's
//! own rich-content protocol (`terminal::rich_content_transport`), with no
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

/// Reads just enough of an MP3/FLAC file to describe its PCM shape AND
/// its real total duration — `symphonia`'s probe + format reader parses
/// container/stream headers without decoding any audio frames.
/// Decoding and playback both happen on Som's side (it's the only
/// process guaranteed to be local to the user's speakers, even when this
/// process is running on a remote SSH host) — `somcat` only needs enough
/// to fill `ContentMetadata::Audio` accurately before the first chunk
/// goes out, same "metadata travels on every chunk" pattern used for
/// images.
///
/// Duration comes from `CodecParameters::n_frames`/`time_base`, which
/// `symphonia`'s format readers populate straight from the file's own
/// header (an MP3's Xing/VBRI VBR tag, or a size/bitrate-based estimate;
/// a FLAC's STREAMINFO block) — NOT by decoding the file. This is what
/// lets Som show an accurate duration/seek-bar length from just the
/// first chunk of a multi-gigabyte file, before the rest has streamed in
/// at all (see `SRP_PROTOCOL.md`'s progressive-audio section). `0` if
/// the format reader couldn't determine it, same "unknown" convention
/// the rest of this protocol's metadata fields use.
fn audio_metadata(path: &str) -> Result<(u32, u8, u8, u32), String> {
    use symphonia::core::codecs::CODEC_TYPE_NULL;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).map_err(|e| format!("opening {path}: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &Default::default(), &Default::default())
        .map_err(|e| format!("probing {path}: {e}"))?;

    let track = probed
        .format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| format!("{path}: no decodable audio track found"))?;

    let params = &track.codec_params;
    let sample_rate = params.sample_rate.ok_or_else(|| format!("{path}: unknown sample rate"))?;
    let channels = params.channels.map(|c| c.count() as u8).ok_or_else(|| format!("{path}: unknown channel count"))?;
    let bits_per_sample = params.bits_per_sample.map(|b| b as u8).unwrap_or(16);
    let duration_ms = match (params.n_frames, params.time_base) {
        (Some(n_frames), Some(time_base)) => {
            let time = time_base.calc_time(n_frames);
            (time.seconds.saturating_mul(1000) as u32).saturating_add((time.frac * 1000.0) as u32)
        },
        _ => 0,
    };

    Ok((sample_rate, channels, bits_per_sample, duration_ms))
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

    let columns = width_px.div_ceil(cell_width).max(1).min(297);
    let rows = height_px.div_ceil(cell_height).max(1).min(297);
    print_placeholder_grid_with_cell_dims(session_id, file_id, columns, rows)
}

/// Prints a placeholder grid of an EXPLICIT `columns`x`rows` cell footprint
/// — used for audio, which has no pixel dimensions to derive a footprint
/// from at all (unlike images/GIF). Som paints its own fixed-size
/// play/pause/seek-bar widget into whatever footprint this placeholder
/// grid reserves, the same way it paints decoded image pixels into an
/// image's own reserved footprint — see `paint_rich_content_placements`'s
/// audio branch in `terminal_element.rs`.
const AUDIO_WIDGET_COLUMNS: u32 = 40;
const AUDIO_WIDGET_ROWS: u32 = 1;

fn print_audio_placeholder_grid(session_id: u32, file_id: u32) -> Result<(), String> {
    print_placeholder_grid_with_cell_dims(session_id, file_id, AUDIO_WIDGET_COLUMNS, AUDIO_WIDGET_ROWS)
}

fn print_placeholder_grid_with_cell_dims(
    session_id: u32,
    file_id: u32,
    mut columns: u32,
    mut rows: u32,
) -> Result<(), String> {
    // `columns`/`rows` above (for the image caller) assume the image's
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

/// Session/file id pair identifying one SRP transfer on the wire — see
/// [`new_ids`]'s doc comment for how these are derived and why they're
/// masked to 24 bits.
type SrpIds = (u32, u32);

/// Derives a fresh `(session_id, file_id)` pair for a new SRP transfer.
/// Both are time-derived so two separate `somcat` invocations rarely
/// collide on the receiving side's `(session_id, file_id)` cache key.
/// Masked to 24 bits — this process encodes each id into a placeholder
/// cell's RGB color (`id_to_rgb` above) for image placements, which only
/// has 24 bits to work with; an unmasked id here would silently lose its
/// high byte on the round trip through the grid. Audio has no placeholder
/// grid to round-trip through, but reuses the same id shape for
/// consistency and so the receiving-side cache key format never depends
/// on content type.
fn new_ids() -> SrpIds {
    let now_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u32).unwrap_or(1);
    let session_id = (now_ms & 0xFF_FFFF).max(1);
    let file_id = (now_ms.wrapping_mul(2_654_435_761) & 0xFF_FFFF).max(1); // Knuth multiplicative hash, cheap decorrelation from session_id.
    (session_id, file_id)
}

/// Streams `bytes` to Som's own binary rich-content protocol
/// (`terminal::rich_content_transport`) — the file's raw bytes go over the
/// wire base91-encoded (see that module's doc comment for why), chunk by
/// chunk, with no re-encoding at all. Content-type-agnostic: the caller
/// picks `content_type`/`metadata`, this function only knows how to chop
/// bytes into envelopes and write them out.
fn stream_bytes(
    bytes: &[u8],
    content_type: terminal::rich_content_transport::ContentType,
    metadata: terminal::rich_content_transport::ContentMetadata,
    ids: SrpIds,
) -> Result<(), String> {
    let total_size = bytes.len() as u64;
    send_range_chunks(bytes, content_type, metadata, ids, total_size, 0, bytes.len() as u64)
}

/// Sends `bytes[offset as usize .. (offset + len) as usize]` as one or
/// more chunk envelopes at their real file offsets — the shared
/// implementation behind both [`stream_bytes`] (the whole file, offset
/// 0) and a range-request response (an arbitrary sub-range, see
/// [`spawn_audio_query_responder`]). `total_size` is the WHOLE file's
/// size (not `bytes.len()`), since a range response still needs to
/// declare the file's real total size in every chunk header, same as
/// the initial sequential stream does.
fn send_range_chunks(
    bytes: &[u8],
    content_type: terminal::rich_content_transport::ContentType,
    metadata: terminal::rich_content_transport::ContentMetadata,
    (session_id, file_id): SrpIds,
    total_size: u64,
    range_offset: u64,
    range_len: u64,
) -> Result<(), String> {
    use terminal::rich_content_transport::{Chunk, build_envelope, split_into_chunks};

    let start = range_offset as usize;
    let end = (range_offset + range_len).min(bytes.len() as u64) as usize;
    let slice = bytes.get(start..end).ok_or_else(|| format!("range [{start}, {end}) out of bounds"))?;

    let pieces = split_into_chunks(slice, CHUNK_SIZE);
    let mut offset = range_offset;
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
    Ok(())
}

/// Spawns the background thread that services Som's byte-range queries
/// (`rich_content_transport::Query`/`QUERY_MARKER`) for the rest of this
/// process's lifetime — see this module's doc comment on
/// `STDOUT_WRITE_LOCK` for why concurrent writes need synchronization,
/// and `rich_content_transport::Query`'s own doc comment for why a
/// range-request's ANSWER is just ordinary chunk envelopes, not a new
/// reply shape.
///
/// Reads stdin one byte at a time looking for `ESC _ Q ... ESC \`
/// (mirrors `query_cell_size_px`'s own byte-at-a-time stdin read, but
/// long-lived instead of a single blocking read-with-timeout). Must only
/// be started AFTER any other stdin reader this process still needs has
/// already finished (`stream_file`'s audio branch prints the
/// placeholder grid — which reads Som's own `CSI 16 t`/`CSI 18 t`
/// cell-size replies off stdin — BEFORE calling this, specifically so
/// the two never read the same stdin concurrently, which would be
/// inherently racy).
///
/// `bytes` is the SAME in-memory file contents `stream_file` already
/// read via `std::fs::read` before starting to stream — sharing it
/// (via `Arc`) rather than opening a second file handle avoids any
/// file-position contention with a second `Read`/`Seek` user, and
/// `somcat` already loaded the whole file into memory before this point
/// regardless.
///
/// Returns a `(stop_flag, JoinHandle)` pair. The flag is checked between
/// stdin reads, but a thread parked in a blocking `read()` call won't
/// observe it until its next byte arrives — `stream_file` sets the flag
/// once the main sequential loop finishes but deliberately does NOT
/// join the handle: `somcat`'s process exits right after the audio
/// path's caller returns regardless, which tears this thread down along
/// with it, and there is no further stdin use afterward this needs to
/// be sequenced against.
fn spawn_audio_query_responder(
    bytes: std::sync::Arc<Vec<u8>>,
    content_type: terminal::rich_content_transport::ContentType,
    metadata: terminal::rich_content_transport::ContentMetadata,
    ids: SrpIds,
    total_size: u64,
) -> (std::sync::Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<()>) {
    use std::sync::atomic::{AtomicBool, Ordering};

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop_for_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if stop_for_thread.load(Ordering::Relaxed) {
                return;
            }
            match std::io::Read::read(&mut stdin, &mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    // APC terminator `ESC \` — check whether the tail of
                    // `buf` contains a complete `ESC _ Q ... ESC \`
                    // envelope, same start/end markers `stream_bytes`
                    // itself wraps every chunk in.
                    if buf.len() >= 2 && buf[buf.len() - 2] == 0x1B && buf[buf.len() - 1] == b'\\' {
                        if let Some(start) = find_apc_start(&buf) {
                            let inner = &buf[start + 2..buf.len() - 2];
                            if inner.first().copied() == Some(terminal::rich_content_transport::QUERY_MARKER)
                                && let Ok(query) = terminal::rich_content_transport::parse_query_envelope(inner)
                                && query.session_id == ids.0
                                && query.file_id == ids.1
                            {
                                let _ = send_range_chunks(
                                    &bytes,
                                    content_type,
                                    metadata,
                                    ids,
                                    total_size,
                                    query.offset,
                                    query.len,
                                );
                            }
                        }
                        buf.clear();
                    } else if buf.len() > 8192 {
                        // No plausible envelope this large — drop
                        // whatever's accumulated rather than growing
                        // `buf` forever on unrelated stdin noise.
                        buf.clear();
                    }
                },
                _ => return,
            }
        }
    });
    (stop, handle)
}

/// Finds the start of the LAST `ESC _` (APC start) in `buf`, if any —
/// used by [`spawn_audio_query_responder`] to locate where a just-
/// completed envelope (ending at `buf`'s own tail) began, without
/// needing a full state machine for a byte stream that's overwhelmingly
/// just one envelope at a time in practice.
fn find_apc_start(buf: &[u8]) -> Option<usize> {
    buf.windows(2).rposition(|w| w == [0x1B, b'_'])
}

/// Streams `path` to Som over SRP, then prints a placeholder grid
/// reserving this placement's on-screen footprint — for images/GIF sized
/// from the file's own pixel dimensions (`print_placeholder_grid`), for
/// audio a fixed cell footprint (`print_audio_placeholder_grid`) since
/// audio has no pixel dimensions to derive one from. Either way, Som
/// decodes the cached bytes and paints into that reserved footprint
/// itself — a decoded image frame for images/GIF, an inline play/pause/
/// seek-bar widget for audio — `somcat`'s job ends once the bytes are on
/// the wire and the footprint is reserved.
///
/// Content type is inferred from the file extension: `.gif`, `.jpg`/
/// `.jpeg`, `.png`, `.mp3`, `.flac` (markdown/video are reserved
/// `ContentType` variants, not wired up to any extension yet).
///
/// For images: THIS process prints the Unicode-placeholder grid itself
/// — deliberately not Som, which used to inject this text into its own
/// terminal model out-of-band after the fact. That out-of-band write
/// never reached the real PTY, so the actual shell (PowerShell, etc.)
/// never learned the cursor had moved: its own line editor kept
/// redrawing from its last-known (stale) cursor position, visibly
/// fighting Som's cursor placement. Printing through this process's own
/// stdout is ordinary child-process output as far as the shell and
/// ConPTY are concerned, so there's nothing to reconcile.
///
/// For audio: decoding and playback both happen on Som's side, not
/// here. `somcat` (this process) can be running on a remote SSH host
/// while Som — and the user's actual speakers — are local; playback has
/// to happen wherever Som runs, so this process's only audio-specific
/// work is probing header metadata (`audio_metadata`) to fill
/// `ContentMetadata::Audio` accurately before the first chunk goes out.
fn stream_file(path: &str) -> Result<(), String> {
    use terminal::rich_content_transport::{ContentMetadata, ContentType};

    let extension = std::path::Path::new(path).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
    let content_type = match extension.as_deref() {
        Some("gif") => ContentType::Gif,
        Some("jpg" | "jpeg") => ContentType::Jpeg,
        Some("png") => ContentType::Png,
        Some("mp3" | "flac") => ContentType::Audio,
        Some(other) => {
            return Err(format!(
                "unrecognized extension .{other} — only .gif/.jpg/.jpeg/.png/.mp3/.flac are supported so far"
            ));
        },
        None => return Err("file has no extension, can't infer content type".to_string()),
    };

    let bytes = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
    let ids = new_ids();

    if content_type == ContentType::Audio {
        let (sample_rate, channels, bits_per_sample, duration_ms) = audio_metadata(path)?;
        let metadata = ContentMetadata::Audio { sample_rate, channels, bits_per_sample, duration_ms };
        let (session_id, file_id) = ids;

        // Placeholder grid FIRST, streaming SECOND — the reverse order
        // from every other content type. `print_audio_placeholder_grid`
        // reads Som's `CSI 16 t`/`CSI 18 t` cell-size replies off this
        // process's own stdin (`query_cell_size_px`/`query_cell_count`),
        // and so does `spawn_audio_query_responder`'s background thread
        // once it starts — two things reading the same stdin
        // concurrently is inherently racy (whichever thread's read call
        // gets a given byte first wins), so the query responder must not
        // start until AFTER the grid's own queries have already
        // completed. The grid's footprint is a fixed cell size (see
        // `print_audio_placeholder_grid`), not derived from decoded
        // audio content, so printing it before any bytes have streamed
        // is correct, not just convenient.
        print_audio_placeholder_grid(session_id, file_id)?;

        let total_size = bytes.len() as u64;
        let shared_bytes = std::sync::Arc::new(bytes);
        let (stop, handle) =
            spawn_audio_query_responder(shared_bytes.clone(), content_type, metadata, ids, total_size);
        let result = stream_bytes(&shared_bytes, content_type, metadata, ids);
        // The responder thread blocks on a stdin read that may never
        // return on its own (no more queries arriving) — `stop` is
        // checked between reads, but a thread parked in `read()` won't
        // observe it until its next byte arrives or the process exits.
        // Not joining here is deliberate: `main()` returns (and the
        // process exits) right after this function does for the audio
        // path, which tears the thread down along with it; there is no
        // further stdin use after this point for `somcat` to protect
        // against racing with.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        drop(handle);
        return result;
    }

    let (width_px, height_px, is_animated) =
        if content_type == ContentType::Gif { gif_metadata(&bytes)? } else { static_image_metadata(&bytes)? };
    // Every pixel this protocol sends is decoded to RGBA before reaching
    // the wire (see `rich_content_gif_player`/`gpui::RenderImage`'s own
    // frame buffers) — 32 bits per pixel regardless of the source GIF's
    // own (typically <=8-bit indexed/palette) on-disk color depth.
    let metadata = ContentMetadata::Image { width_px, height_px, color_bits: 32, is_animated };
    stream_bytes(&bytes, content_type, metadata, ids)?;

    let (session_id, file_id) = ids;
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
/// Serializes every `write_raw_stdout` call — needed once audio streams
/// gained a second writer (the background query-reader thread's range-
/// response chunks, see `spawn_audio_query_responder`) that runs
/// CONCURRENTLY with the main thread's own sequential chunk-sending
/// loop. Without this, two envelopes' bytes could interleave on the
/// wire (one thread's `WriteFile`/`write_all` call landing partway
/// through another's), producing bytes neither `parse_envelope` nor
/// `parse_query_envelope` could ever make sense of — every other content
/// type (images/GIF) only ever has one writer (the main thread), so this
/// was never needed before audio's background thread existed.
static STDOUT_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(windows)]
fn write_raw_stdout(bytes: &[u8]) -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    let _guard = STDOUT_WRITE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
    let _guard = STDOUT_WRITE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::io::stdout().write_all(bytes).map_err(|e| e.to_string())
}
