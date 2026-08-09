//! `somcat` — a minimal terminal image viewer that speaks the Kitty
//! terminal graphics protocol directly, with no dependency on `crossterm`
//! or any other console-mode abstraction library.
//!
//! Written specifically after discovering (KITTY_GRAPHICS_PLAN.md Stage 7,
//! see `project_kitty_graphics_protocol` memory) that `yazi` (which relies
//! on `crossterm` for Windows console raw mode) fails to receive Som's
//! Kitty graphics query responses on Windows, because `crossterm`'s
//! `enable_raw_mode()` never sets `ENABLE_VIRTUAL_TERMINAL_INPUT` — without
//! that flag, the Windows console translates/discards raw VT byte
//! sequences instead of delivering them to a reading process's stdin. This
//! tool sets that flag itself, so it can serve as a genuinely reliable
//! reference client for testing Som's Kitty Graphics Protocol
//! implementation end-to-end, on Windows as well as Unix — independent of
//! whatever bugs/limitations third-party terminal libraries may have.
//!
//! Usage: `somcat <image-path> [<image-path> ...]`
//!
//! For each image: queries the terminal's Kitty graphics support (`a=q`),
//! waits briefly for a response, then transmits and displays the image
//! (`a=T`) at the current cursor position, sized to fit within a
//! reasonable terminal-cell footprint. Falls back to printing a plain
//! message (not garbage escape codes) if the terminal never answers the
//! capability query — the same graceful-degradation behavior real Kitty
//! graphics clients (`icat`, `yazi`) are expected to have, and that
//! `crates/terminal/src/kitty_graphics.rs`'s doc comments already describe
//! from Som's receiving side.
mod raw_mode;

use base64::Engine as _;
use image::AnimationDecoder as _;
use image::codecs::gif::GifDecoder;
use std::io::{Read, Write};
use std::time::Duration;

/// Kitty's own recommended chunk size for base64 payloads split across
/// multiple APC strings — large enough to be efficient, small enough that
/// no real terminal's escape-sequence parser chokes on a single control
/// string. See <https://sw.kovidgoyal.net/kitty/graphics-protocol/#the-transmission-medium>.
const CHUNK_SIZE: usize = 4096;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--stream <file>` is a completely separate code path from the
    // Kitty-based flow below — it speaks Som's own binary rich-content
    // protocol (`terminal::rich_content_transport`), not Kitty's, has no
    // capability query/negotiation step (unlike Kitty, this protocol
    // isn't optional-with-fallback: it's Som-specific, so there's no
    // third-party terminal to gracefully degrade for), and sends the
    // source file's raw bytes unmodified rather than re-encoding frames
    // as PNG. See `stream_file`'s own doc comment for the full picture.
    if args.first().map(String::as_str) == Some("--stream") {
        let Some(path) = args.get(1) else {
            eprintln!("usage: somcat --stream <file>");
            std::process::exit(2);
        };
        // No `raw_mode::enable()` here, unlike the Kitty path below —
        // that's only needed for `query_kitty_support`'s stdin read of
        // the capability-query response (see `raw_mode`'s own doc
        // comment: it exists specifically to make raw escape bytes reach
        // this process's stdin on Windows). Som's own rich-content
        // protocol has no query/negotiation step at all (see
        // `stream_file`'s doc comment for why — it's Som-specific, not
        // an optional-with-fallback external spec), so this path never
        // reads stdin and doesn't need its console mode touched.
        //
        // `enable_output_vt_processing()` (output side only, no stdin
        // handle touched, no RAII guard) IS still needed though — found
        // the hard way (a live headless bench test hung indefinitely on
        // the very first `stdout.write_all` of a binary envelope):
        // without `ENABLE_VIRTUAL_TERMINAL_PROCESSING` set on the output
        // handle, ConPTY's own internal VT parser apparently does not
        // reliably accept a raw binary (non-UTF-8-safe) byte stream the
        // same way it does once that mode is explicitly requested.
        raw_mode::enable_output_vt_processing();
        if let Err(err) = stream_file(path) {
            eprintln!("somcat --stream: failed to stream {path}: {err}");
            std::process::exit(1);
        }
        return;
    }

    let paths: Vec<String> = args;
    if paths.is_empty() {
        eprintln!("usage: somcat <image-path> [<image-path> ...]");
        std::process::exit(2);
    }

    let raw_guard = raw_mode::enable();
    let supported = query_kitty_support();
    if !supported {
        // Restore the terminal before printing anything further — no
        // point leaving it in raw mode just to print a fallback message.
        drop(raw_guard);
        eprintln!(
            "somcat: this terminal did not answer the Kitty graphics protocol capability query \
             (a=q) within the timeout — falling back to plain output instead of risking garbage \
             escape codes on screen."
        );
        for path in &paths {
            println!("[image: {path}] (Kitty graphics protocol not supported by this terminal)");
        }
        return;
    }

    let mut exit_code = 0;
    // Each image gets its own id, starting from a random-ish base (the
    // low bits of the current time) rather than a fixed `1` — a fixed id
    // means every separate `somcat` invocation (and every extra path
    // within one invocation) collides with whatever image the receiving
    // terminal already stored under that same id: `ImageStore::insert_image`
    // (on the Som side) documents this exact case as "replacing an
    // existing image", so a second `somcat` call doesn't add a new
    // scrollback entry, it silently OVERWRITES the pixel data any
    // earlier, still-on-screen placement now points at. Confirmed as a
    // real, visible bug: running `somcat` twice in the same session
    // showed the FIRST image's on-screen placement suddenly display the
    // SECOND image's content instead of staying put and scrolling into
    // history normally.
    let mut next_image_id = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(1))
        & 0x7FFF_FFFF;
    if next_image_id == 0 {
        next_image_id = 1;
    }
    for path in &paths {
        let id = next_image_id;
        next_image_id = next_image_id.wrapping_add(1).max(1);
        if let Err(err) = display_image(path, id) {
            eprintln!("somcat: failed to display {path}: {err}");
            exit_code = 1;
        }
        // A blank line of vertical room after each image so multiple
        // images in one invocation don't visually overlap — the terminal
        // has already advanced its cursor past the image's cell footprint
        // by the point this runs (see display_image's placement sizing).
        println!();
    }

    drop(raw_guard);
    std::process::exit(exit_code);
}

/// Sends `a=q` and waits briefly for the terminal's response. Real Kitty
/// terminals answer within milliseconds; not answering at all within this
/// timeout is the terminal's own way of saying "I don't support this",
/// same assumption `KITTY_GRAPHICS_PLAN.md`'s degradation-check tests make
/// on the receiving side.
fn query_kitty_support() -> bool {
    print!("\x1b_Gi=1,a=q,t=d,f=24;AAAA\x1b\\");
    std::io::stdout().flush().ok();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        let mut stdin = std::io::stdin();
        let marker = b"\x1b_Gi=1;OK\x1b\\";
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    if buf.windows(marker.len()).any(|w| w == marker) || buf.len() >= 256 {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = tx.send(buf);
    });

    match rx.recv_timeout(Duration::from_millis(800)) {
        Ok(buf) => String::from_utf8_lossy(&buf).contains("\x1b_Gi=1;OK"),
        Err(_) => false,
    }
}

// A reasonable default cell footprint: cap at 40 columns / 20 rows so a
// single image never floods an entire terminal window regardless of its
// native pixel dimensions — Kitty's own c=/r= keys let the sender request
// exactly this kind of display-size override independent of the source
// image's real resolution.
const DISPLAY_COLUMNS: u32 = 40;
const DISPLAY_ROWS: u32 = 20;

/// Animated frames are downscaled to fit within this many pixels on their
/// longer side before encoding — a terminal cell grid can only ever show a
/// handful of hundred pixels at most (`DISPLAY_COLUMNS`/`DISPLAY_ROWS`
/// cells), so transmitting a source GIF's full resolution (a typical giphy
/// download is 400-500px square) for EVERY frame wastes bandwidth for no
/// visible benefit — worse, over Som's som-tmux relay (a low-bandwidth
/// named pipe, see `crates/som_tmux/src/relay.rs`'s own doc comments on
/// shared-channel contention) a multi-second-per-frame transmission makes
/// the animation command (`a=a`) arrive so late it looks like animation
/// doesn't work at all, when really it just hasn't finished uploading yet.
const MAX_FRAME_DIMENSION: u32 = 256;

fn display_image(path: &str, id: u32) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;

    // GIF is the only animated format worth special-casing here — it's the
    // one format callers of this tool actually hand somcat (see
    // `KITTY_GRAPHICS_PLAN.md`'s animation bullet); everything else
    // (including a single-frame GIF, or a WebP that happens to be
    // animated) goes through the existing static PNG path unchanged.
    if image::guess_format(&bytes).ok() == Some(image::ImageFormat::Gif) {
        let frames = decode_gif_frames(&bytes)?;
        if frames.len() > 1 {
            return transmit_and_animate(id, frames);
        }
        // A single-frame "animated" GIF (or a decode that only yielded one
        // usable frame) doesn't need the a=f/a=a machinery at all.
        let (buffer, _delay) = frames.into_iter().next().ok_or("GIF had no decodable frames")?;
        return transmit_static_rgba(id, buffer);
    }

    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    transmit_static_rgba(id, img.to_rgba8())
}

fn transmit_static_rgba(id: u32, rgba: image::RgbaImage) -> Result<(), String> {
    let (width, height) = rgba.dimensions();
    let png_bytes = encode_png(&rgba, width, height)?;
    let base64_payload = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    transmit_and_place(id, &base64_payload, DISPLAY_COLUMNS, DISPLAY_ROWS)
}

/// Decodes every frame of a GIF via `image`'s standard animation path
/// (`GifDecoder` + `AnimationDecoder::into_frames`), pairing each frame's
/// pixels with its own display duration, and downscaling each frame to
/// `MAX_FRAME_DIMENSION` (see that constant's doc comment). A frame that
/// individually fails to decode is skipped (logged to stderr) rather than
/// aborting the whole animation — mirrors `gpui::elements::img`'s same
/// tolerance for partially-corrupt GIFs.
fn decode_gif_frames(bytes: &[u8]) -> Result<Vec<(image::RgbaImage, Duration)>, String> {
    let decoder = GifDecoder::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut frames = Vec::new();
    for frame in decoder.into_frames() {
        match frame {
            Ok(frame) => {
                let delay = Duration::from(frame.delay());
                let buffer = downscale(frame.into_buffer());
                frames.push((buffer, delay));
            },
            Err(err) => eprintln!("somcat: skipping unreadable GIF frame: {err}"),
        }
    }
    Ok(frames)
}

fn downscale(rgba: image::RgbaImage) -> image::RgbaImage {
    let (width, height) = rgba.dimensions();
    if width <= MAX_FRAME_DIMENSION && height <= MAX_FRAME_DIMENSION {
        return rgba;
    }
    let scale = MAX_FRAME_DIMENSION as f32 / width.max(height) as f32;
    let new_width = ((width as f32 * scale).round() as u32).max(1);
    let new_height = ((height as f32 * scale).round() as u32).max(1);
    image::imageops::resize(&rgba, new_width, new_height, image::imageops::FilterType::Triangle)
}

/// Transmits an animated image's frames: the first frame as a normal
/// `a=T` (so it also gets placed and displays immediately, same as a
/// static image), that root frame's own gap set separately via
/// `a=a,r=1,z=<gap>` (the ONLY way to do so — `a=T`'s own `z=` key means
/// z-index, not gap), every subsequent frame as `a=f` carrying its own
/// delay (`z=`, milliseconds), then `a=a,s=3` to start normal (looping)
/// playback — see `crates/terminal/src/kitty_graphics.rs`'s
/// `FrameCommand`/`AnimationCommand` for the receiving side, and the
/// official spec's "Animation frames" section
/// (<https://sw.kovidgoyal.net/kitty/graphics-protocol/#animation-frames>)
/// for exactly why `s=3` (not `s=1`, which actually means STOP) is what
/// starts looping playback. Every frame (base and `a=f`) is sent
/// PNG-encoded, not raw RGBA — raw pixels for a multi-frame animation add
/// up fast (a single 256x256 raw RGBA frame is 256KB before even
/// base64-inflating it), while PNG's compression keeps a real GIF's
/// per-frame transmission small enough that the whole animation actually
/// finishes uploading in a reasonable time.
fn transmit_and_animate(id: u32, frames: Vec<(image::RgbaImage, Duration)>) -> Result<(), String> {
    let mut frames = frames.into_iter();
    let (first_buffer, first_delay) = frames.next().ok_or("no frames to animate")?;
    let (width, height) = first_buffer.dimensions();

    transmit_png_frame(id, &first_buffer, DISPLAY_COLUMNS, DISPLAY_ROWS, true)?;

    let root_gap_ms = first_delay.as_millis().max(1) as u32;
    print!("\x1b_Ga=a,i={id},r=1,z={root_gap_ms}\x1b\\");

    for (buffer, delay) in frames {
        if buffer.dimensions() != (width, height) {
            // Kitty's a=f has no way to change an animation's canvas size
            // mid-stream — a GIF frame with a different size than the
            // first would need compositing onto the logical screen size
            // first (full GIF disposal-method support), which this
            // reference client deliberately doesn't implement. Skipping
            // rather than sending a malformed frame.
            eprintln!("somcat: skipping GIF frame with mismatched size {:?} (expected {width}x{height})", buffer.dimensions());
            continue;
        }
        let gap_ms = delay.as_millis().max(1) as u32;
        transmit_frame(id, &buffer, gap_ms)?;
    }

    // s=3: run normally, looping back to the first frame once the last is
    // reached — NOT s=1, which per spec stops (rather than starts)
    // playback.
    print!("\x1b_Ga=a,i={id},s=3\x1b\\");
    std::io::stdout().flush().map_err(|e| e.to_string())?;

    // Cursor was already advanced past the placement's footprint by the
    // initial transmit_png_frame call above.
    Ok(())
}

fn encode_png(rgba: &image::RgbaImage, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(rgba.clone())
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode failed for {width}x{height} image: {e}"))?;
    Ok(bytes)
}

/// Transmits the base64 PNG payload (chunked if needed, per Kitty's `m=`
/// continuation flag) and immediately places it at the current cursor
/// position with the given cell footprint.
fn transmit_and_place(id: u32, base64_payload: &str, columns: u32, rows: u32) -> Result<(), String> {
    send_chunked(&format!("a=T,f=100,i={id}"), base64_payload)?;

    print!("\x1b_Ga=p,i={id},p=1,c={columns},r={rows}\x1b\\");
    // Move the cursor past the placement's cell footprint so subsequent
    // output (the blank line in `main`, or a later image) doesn't
    // overlap it — Kitty placements don't reserve grid space on their
    // own, they're painted independent of the text layer.
    for _ in 0..rows {
        println!();
    }
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// Like [`transmit_and_place`] but for the FIRST frame of an animation —
/// PNG-encoded like every other frame in the sequence (see
/// `transmit_and_animate`'s doc comment for why raw RGBA isn't used here),
/// and, when `display_immediately` is true, sized/placed identically to a
/// static image so the animation's first frame appears exactly like any
/// other somcat image while the rest of its frames stream in behind it.
fn transmit_png_frame(
    id: u32,
    rgba: &image::RgbaImage,
    columns: u32,
    rows: u32,
    display_immediately: bool,
) -> Result<(), String> {
    let (width, height) = rgba.dimensions();
    let png_bytes = encode_png(rgba, width, height)?;
    let base64_payload = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    let action = if display_immediately { "T" } else { "t" };
    send_chunked(&format!("a={action},f=100,i={id}"), &base64_payload)?;

    if display_immediately {
        print!("\x1b_Ga=p,i={id},p=1,c={columns},r={rows}\x1b\\");
        for _ in 0..rows {
            println!();
        }
    }
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// Appends one more animation frame (`a=f`) to an already-transmitted
/// image, with its own display duration. PNG-encoded — see
/// `transmit_and_animate`'s doc comment for why. Flushes explicitly (like
/// every other `transmit_*` function) — without it, stdout's own
/// block-buffering could hold each frame's bytes in this process's
/// userspace buffer indefinitely instead of actually writing them to the
/// PTY, since nothing else in the per-frame loop this is called from ever
/// flushes on its behalf.
fn transmit_frame(id: u32, rgba: &image::RgbaImage, gap_ms: u32) -> Result<(), String> {
    let (width, height) = rgba.dimensions();
    let png_bytes = encode_png(rgba, width, height)?;
    let base64_payload = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    send_chunked(&format!("a=f,i={id},z={gap_ms}"), &base64_payload)?;
    std::io::stdout().flush().map_err(|e| e.to_string())
}

/// Shared chunking helper: writes `first_control_data;<payload>`, splitting
/// the base64 payload across multiple APC strings (`m=1` continuations) per
/// Kitty's own recommended chunk size when it's too large for one. Always
/// sends at least one APC string, even for an empty payload.
fn send_chunked(first_control_data: &str, base64_payload: &str) -> Result<(), String> {
    let bytes = base64_payload.as_bytes();
    let mut offset = 0;
    let mut first = true;

    loop {
        let end = (offset + CHUNK_SIZE).min(bytes.len());
        let chunk = &bytes[offset..end];
        let more = end < bytes.len();
        let more_flag = if more { "1" } else { "0" };

        if first {
            print!(
                "\x1b_G{first_control_data},m={more_flag};{}\x1b\\",
                std::str::from_utf8(chunk).unwrap()
            );
            first = false;
        } else {
            print!("\x1b_Gm={more_flag};{}\x1b\\", std::str::from_utf8(chunk).unwrap());
        }
        // Flushed after EVERY chunk (not just once at the end of the whole
        // transmission) — without this, stdout's own block-buffering could
        // hold many chunks in this process's userspace buffer indefinitely
        // instead of actually writing them to the PTY, since nothing else
        // in this loop flushes on its behalf.
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        offset = end;
        if !more {
            break;
        }
    }
    Ok(())
}

/// Streams `path` to Som's own binary rich-content protocol
/// (`terminal::rich_content_transport`) instead of Kitty's — the file's
/// raw bytes go over the wire unmodified, chunk by chunk, with no
/// per-frame re-encoding (no PNG, no base64): the receiving side
/// (`terminal::rich_content_cache`/`rich_content_gif_player`) decodes the
/// file format itself, progressively, directly off the growing cache file
/// on disk. This is a dramatically simpler client-side path than the
/// Kitty animation flow above (`transmit_and_animate`/`decode_gif_frames`)
/// precisely because none of that re-encoding work happens here anymore —
/// see `rich_content_transport`'s module doc comment for the full
/// rationale behind the whole progressive-copy design.
///
/// Content type is inferred from the file extension; only GIF is
/// recognized right now (audio/markdown/video are reserved
/// `ContentType` variants, not wired up to any extension yet).
fn stream_file(path: &str) -> Result<(), String> {
    use terminal::rich_content_transport::{Chunk, ContentType, build_envelope, split_into_chunks};

    let content_type = match std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("gif") => ContentType::Gif,
        Some(other) => return Err(format!("unrecognized extension .{other} — only .gif is supported so far")),
        None => return Err("file has no extension, can't infer content type".to_string()),
    };

    let bytes = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
    let total_size = bytes.len() as u64;

    // Same per-invocation-unique id principle as the Kitty path's
    // `next_image_id` above (see that variable's own doc comment for the
    // real, once-shipped collision bug this pattern exists to avoid) —
    // session_id and file_id both derived from the current time so two
    // separate `somcat --stream` invocations never collide on the
    // receiving side's `(session_id, file_id)` cache key.
    let now_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u32).unwrap_or(1);
    let session_id = now_ms.max(1);
    let file_id = now_ms.wrapping_mul(2_654_435_761).max(1); // Knuth multiplicative hash, cheap decorrelation from session_id.

    let pieces = split_into_chunks(&bytes, CHUNK_SIZE);
    let mut offset = 0u64;
    for (payload, trailing_byte) in pieces {
        let payload_len = payload.len() as u64;
        let chunk = Chunk { content_type, session_id, file_id, chunk_offset: offset, total_size, payload, trailing_byte };
        // `build_envelope` already produces the complete envelope
        // (marker + version + ... + payload) — this just wraps it in the
        // APC start/end sequence (`ESC _` ... `ESC \`).
        let envelope = build_envelope(&chunk);
        let mut apc = Vec::with_capacity(2 + envelope.len() + 2);
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&envelope);
        apc.extend_from_slice(&[0x1B, b'\\']);
        write_raw_stdout(&apc)?;
        offset += payload_len + if chunk.trailing_byte.is_some() { 1 } else { 0 };
    }
    Ok(())
}

/// Writes `bytes` to stdout bypassing `std::io::Stdout` entirely — see
/// this function's own platform-specific implementations for why.
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
/// comment ("FIXME: this should be LineWriter or BufWriter depending on
/// the state of stdout"). This protocol's binary envelopes essentially
/// never contain a real `\n` (0x0A) byte, so `write_all` alone leaves most
/// of what was "written" sitting in that userspace buffer, never actually
/// reaching the OS pipe underneath. Calling `.flush()` afterward to force
/// it out is what actually hung — confirmed directly: `write_all` itself
/// reliably returned `Ok`, but the very next `.flush()` call never
/// returned at all, on the very first (113-byte) chunk, every time. Kitty's
/// own `send_chunked`/`print!`-based transmission (`crates/somcat`'s
/// existing Kitty path above) never hits this because it's base64 text
/// terminated by `println!()` calls between images — real `\n` bytes are
/// common in that path, so `LineWriter` keeps flushing on its own without
/// ever needing the explicit call this function exists to avoid.
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
    std::io::stdout().write_all(bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measures the actual wire size (post-PNG, post-base64) somcat sends
    /// for the repo's `giphy.gif` fixture — the real number that matters
    /// for how long a live Som window's PTY pipe/ConPTY/paint pipeline has
    /// to move before an animation finishes arriving. Written while
    /// investigating why a single 47-frame 500x500 GIF took ~10s to fully
    /// animate live even after fixing the `paint()` force-redraw
    /// starvation bug — this establishes whether the remaining time is
    /// dominated by payload SIZE (this test) or by per-command/per-chunk
    /// processing overhead (see `bench_somcat_*` in
    /// `crates/terminal/src/terminal.rs`, which measure the latter
    /// end-to-end and stay in the 5-9s range with no competing real
    /// window).
    #[test]
    fn measure_giphy_gif_wire_payload_size() {
        let giphy_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../giphy.gif");
        let bytes = std::fs::read(&giphy_path)
            .unwrap_or_else(|e| panic!("reading {giphy_path:?}: {e}"));
        let frames = decode_gif_frames(&bytes).expect("giphy.gif must decode");

        let mut total_png = 0usize;
        let mut total_base64 = 0usize;
        for (buffer, _delay) in &frames {
            let (w, h) = buffer.dimensions();
            let png_bytes = encode_png(buffer, w, h).expect("PNG encode must succeed");
            total_png += png_bytes.len();
            // Base64 expands every 3 raw bytes into 4 encoded bytes.
            total_base64 += png_bytes.len().div_ceil(3) * 4;
        }

        eprintln!(
            "MEASURE: {} frames, {}x{} (post-downscale), total PNG={:.1}KB, total base64 (wire)={:.1}KB, avg/frame={:.1}KB base64",
            frames.len(),
            frames.first().map(|(b, _)| b.width()).unwrap_or(0),
            frames.first().map(|(b, _)| b.height()).unwrap_or(0),
            total_png as f64 / 1024.0,
            total_base64 as f64 / 1024.0,
            total_base64 as f64 / frames.len().max(1) as f64 / 1024.0,
        );
    }
}
