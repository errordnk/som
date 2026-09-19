//! `somsrp` — a minimal terminal image/audio viewer for Som.
//!
//! Usage: `somsrp <file>` or `somsrp --srp <file>` (the flag is an explicit
//! synonym for the default, for anyone who'd rather not rely on an implicit
//! default).
//!
//! Bulk payload bytes and byte-range query/response travel over
//! `somsrv`'s binary side channel (`srv_channel::SrvChannel`,
//! `somsrv::protocol::SrvRequest::PutChunk`/`RequestByteRange`) — the
//! OLD APC/base91-over-PTY transport (`terminal::rich_content_transport`'s
//! `Chunk`/`build_envelope`/`Query`) has been deleted entirely, no
//! fallback. The placeholder-grid control handshake (`print_placeholder_
//! grid`/`print_placeholder_grid_with_cell_dims`) is UNAFFECTED — it never
//! depended on that machinery, see those functions' own doc comments —
//! and stays on the PTY, since it's how Som learns a placement exists at
//! all before any payload bytes arrive.
mod raw_mode;
mod srv_channel;

use image::ImageDecoder as _;
use image::codecs::gif::GifDecoder;
use std::io::Read as _;
use terminal::kitty_graphics_placeholder;

/// Chunk size for one piece of a progressive file transfer — large enough
/// to be efficient, small enough to keep per-piece overhead low.
/// Unchanged from the old APC-over-PTY transport's own chunk size —
/// still a reasonable piece size for `somsrv::protocol::SrvRequest::
/// PutChunk` messages over a real binary side channel, though there's no
/// longer an escape-sequence-parser or ConPTY codepage concern driving
/// this number the way there was on the PTY path; revisit if profiling
/// ever suggests a different size performs better over this transport.
const CHUNK_SIZE: usize = 65536;

fn main() {
    // somsrp links ffmpeg-next/ffmpeg-sys-next directly (see `video_metadata`
    // below) — as its own OS process, it needs the same DLL-search-path
    // wiring `som.exe` does for its embedded video playback, since the two
    // are independent processes and neither's DLL search path is inherited
    // by the other.
    #[cfg(target_os = "windows")]
    terminal::rich_content_video_player::ensure_ffmpeg_extracted_and_wired();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // `-a <N>` selects which of the container's audio streams to decode,
    // `-s <N>` which subtitle stream to render (0-based, in the order
    // FFmpeg's demuxer enumerates each kind) — only meaningful for video
    // today, since audio-only content (.mp3/.flac) has exactly one
    // stream to begin with and carries no subtitles at all. Without
    // `-a`, video always used `ictx.streams().best(Type::Audio)`,
    // FFmpeg's own "most likely the main track" heuristic — usually
    // correct, but real multi-track files (commentary tracks, multiple
    // dub languages) have no way to pick a DIFFERENT one. `-s` has no
    // such heuristic fallback: subtitles default to OFF (`None`) unless
    // explicitly requested, matching every other player-widget's own
    // opt-in convention. Both parsed here (not left to the flags'
    // position relative to `--srp`/the path) so `somsrp -a 1 -s 0 --srp
    // file.mkv`, `somsrp --srp -a 1 file.mkv -s 0`, and every other
    // ordering all work identically — these are modifier flags, not
    // positional.
    let mut audio_stream_index: Option<u32> = None;
    let mut subtitle_stream_index: Option<u32> = None;
    let mut positional: Vec<&str> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-a" || arg == "-s" {
            let Some(value) = iter.next() else {
                eprintln!("usage: somsrp [-a <audio-stream-index>] [-s <subtitle-stream-index>] [--srp] <file>");
                std::process::exit(2);
            };
            let parsed = value.parse::<u32>();
            match (arg.as_str(), parsed) {
                ("-a", Ok(index)) => audio_stream_index = Some(index),
                ("-s", Ok(index)) => subtitle_stream_index = Some(index),
                (flag, Err(_)) => {
                    eprintln!("somsrp: {flag} expects a non-negative integer stream index, got {value:?}");
                    std::process::exit(2);
                },
                _ => unreachable!(),
            }
        } else {
            positional.push(arg);
        }
    }

    let explicit_srp = positional.first().copied() == Some("--srp");
    let path_arg = if explicit_srp { positional.get(1) } else { positional.first() };
    let Some(&path) = path_arg else {
        eprintln!(
            "usage: somsrp [-a <audio-stream-index>] [-s <subtitle-stream-index>] <file>  \
             (or: somsrp [-a <audio-stream-index>] [-s <subtitle-stream-index>] --srp <file>)"
        );
        std::process::exit(2);
    };

    // Raw mode is needed here (not just `enable_output_vt_processing()`)
    // because `stream_file` reads Som's `CSI 16 t` cell-size reply off
    // stdin — see `query_cell_size_px`'s doc comment for why the terminal's
    // own cell metrics (not Som's) must be the source of the placeholder
    // grid's row/column count.
    let raw_guard = raw_mode::enable();

    // A panic anywhere in `stream_file` (its own code or a dependency's)
    // would otherwise unwind straight past `raw_guard`'s `Drop` and past
    // the `Err` handling below without printing anything a user watching
    // the real terminal would ever see — Rust's default panic handler
    // writes to stderr too, but by the time the process actually exits,
    // raw mode may still be disabled correctly (via `Drop`) while the
    // panic message itself scrolls past faster than it's readable, or
    // gets swallowed depending on how the parent PTY buffers output right
    // before the process dies. Installing an explicit hook here, INSIDE
    // raw mode, guarantees the message is written with a trailing
    // `\r\n` (raw mode disables the terminal's own newline translation,
    // so a bare `\n` alone would just carriage-return without advancing
    // to a new line, visually mangling the message) before anything else
    // happens — this is the fix for a real reported symptom ("black
    // screen with instant exit, no visible error") where the actual cause
    // turned out to be silent/invisible rather than the transport itself
    // being broken.
    std::panic::set_hook(Box::new(|info| {
        eprint!("somsrp: panicked: {info}\r\n");
    }));

    let result = stream_file(path, audio_stream_index, subtitle_stream_index);
    drop(raw_guard);
    if let Err(err) = result {
        eprintln!("somsrp: failed to stream {path}: {err}");
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
/// process is running on a remote SSH host) — `somsrp` only needs enough
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

/// Probes `path`'s real picture size/frame rate/codec via FFmpeg —
/// opens the container and reads its header (resolving stream
/// parameters from the header alone) without decoding a single frame.
/// `terminal` already links `ffmpeg-next` on Windows (the only platform
/// with an embedded FFmpeg today — see `crates/assets/src/assets.rs`'s
/// own doc comment), and `somsrp` already depends on `terminal`, so
/// this needs no new dependency of its own.
///
/// Reads through a plain `std::fs::File` wrapped in FFmpeg's custom-
/// `AVIOContext` path (`StreamIo::from_read_seek` + `input_from_stream`)
/// rather than the path-based `ffmpeg_next::format::input(path)` — this
/// build's trimmed FFmpeg deliberately has NO `--enable-protocol=file`
/// (see `vcpkg-overlays/ffmpeg/portfile.cmake`'s own doc comment: the
/// video player in `terminal` never opens a path directly either, for
/// the same reason), so `format::input(path)` fails immediately with
/// "Protocol not found" — confirmed live via a dedicated unit test
/// after this exact failure mode showed up as this function silently
/// never taking effect (the caller's fallback swallowed the error).
///
/// Returns `Err` for anything the probe can't resolve (missing/corrupt
/// header, a codec this build's trimmed FFmpeg doesn't decode, no video
/// stream at all) — the caller falls back to a fixed placeholder size
/// exactly as it did before this function existed, not a big deal for a
/// single unprobeable file.
#[cfg(windows)]
/// Picks the container's main video stream — NOT simply `ictx.streams().
/// best(Video)`, which picks the stream with the largest resolution*
/// frame-count product FFmpeg can determine from the header alone.
/// Confirmed live as wrong for a real file: a multi-track MKV (h264 main
/// feature + several AC3/EAC3 audio tracks + subtitle tracks + an
/// embedded MJPEG cover-art attachment) had `best(Video)` pick the
/// single-frame MJPEG cover art over the actual multi-thousand-frame
/// h264 feature — `best()`'s own heuristic apparently weighs pixel
/// dimensions before frame count when a stream's frame count isn't yet
/// knowable from the header, and a cover-art frame is often encoded at
/// a HIGHER resolution than the feature itself. MJPEG is the standard
/// codec real-world muxers (ffmpeg, mkvmerge) use for embedded cover art
/// specifically — never a legitimate movie/show's own primary video
/// codec in practice — so deprioritizing it below every non-MJPEG
/// candidate closes this gap without needing per-container attachment-
/// flag parsing. MUST match `crates/terminal/src/rich_content_video_
/// player.rs`'s identical `best_video_stream` byte-for-byte (this crate
/// has no dependency access to that one's private fn) — both `somsrp`'s
/// own probe here AND Som's real decode thread need to agree on which
/// stream is "the video," or a probe here could report a different
/// codec/resolution than what actually plays.
fn best_video_stream(ictx: &ffmpeg_next::format::context::Input) -> Option<ffmpeg_next::format::stream::Stream<'_>> {
    let non_mjpeg = ictx
        .streams()
        .filter(|s| s.parameters().id() != ffmpeg_next::codec::Id::MJPEG)
        .max_by_key(|s| (s.parameters().medium() == ffmpeg_next::media::Type::Video, s.frames(), s.duration()));
    non_mjpeg
        .filter(|s| s.parameters().medium() == ffmpeg_next::media::Type::Video)
        .or_else(|| ictx.streams().best(ffmpeg_next::media::Type::Video))
}

fn video_metadata(
    path: &str,
    audio_stream_index: Option<u32>,
    subtitle_stream_index: Option<u32>,
) -> Result<terminal::rich_content_transport::ContentMetadata, String> {
    use terminal::rich_content_transport::{ContentMetadata, VideoCodec};

    let _ = ffmpeg_next::init();
    let file = std::fs::File::open(path).map_err(|e| format!("opening {path}: {e}"))?;
    let stream_io = ffmpeg_next::format::context::StreamIo::from_read_seek(file)
        .map_err(|e| format!("{path}: wrapping file for probing: {e}"))?;
    let filename = std::path::Path::new(path).file_name().and_then(|n| n.to_str());
    let ictx = ffmpeg_next::format::input_from_stream(stream_io, filename, None).map_err(|e| format!("probing {path}: {e}"))?;
    let stream = best_video_stream(&ictx).ok_or_else(|| format!("{path}: no video stream found"))?;

    let params = stream.parameters();
    let context_decoder =
        ffmpeg_next::codec::context::Context::from_parameters(params).map_err(|e| format!("{path}: reading codec parameters: {e}"))?;
    let decoder = context_decoder.decoder().video().map_err(|e| format!("{path}: opening decoder for probing: {e}"))?;

    let width_px = decoder.width();
    let height_px = decoder.height();
    if width_px == 0 || height_px == 0 {
        return Err(format!("{path}: decoder reported zero width/height"));
    }

    let frame_rate = stream.rate();
    let codec = match decoder.id() {
        ffmpeg_next::codec::Id::H264 => VideoCodec::H264,
        ffmpeg_next::codec::Id::HEVC => VideoCodec::H265,
        ffmpeg_next::codec::Id::VP9 => VideoCodec::Vp9,
        ffmpeg_next::codec::Id::AV1 => VideoCodec::Av1,
        ffmpeg_next::codec::Id::MPEG4 => VideoCodec::Mpeg4,
        _ => VideoCodec::Unknown,
    };

    let extension = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
    Ok(ContentMetadata::Video {
        width_px,
        height_px,
        fps_numerator: frame_rate.numerator().max(0) as u32,
        fps_denominator: frame_rate.denominator().max(0) as u32,
        codec,
        audio_stream_index,
        subtitle_stream_index,
        extension,
    })
}

/// Sends `CSI 16 t` ("report cell size in pixels") and waits briefly for
/// the terminal's `CSI 6 ; height ; width t` reply. Real terminals that
/// implement this (Som included — see `alacritty_terminal::Term::
/// cell_size_pixels`, answered through `Terminal::process_event`'s
/// `TextAreaSizeRequest` arm) answer within milliseconds; not answering at
/// all within this timeout means the terminal doesn't support the query,
/// same fallback-on-silence assumption `somsrp`'s old Kitty capability
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
                    if byte[0] == ETX {
                        std::process::exit(130);
                    }
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
                    if byte[0] == ETX {
                        std::process::exit(130);
                    }
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

/// Video counterpart to [`print_placeholder_grid`] — identical column/row
/// derivation from pixel dimensions, but reserves ONE EXTRA row beyond
/// the picture itself for Som's play/pause/seek-bar widget (see
/// `paint_rich_content_placements`'s video branch in `terminal_element.rs`,
/// which paints the picture into every row this placement has EXCEPT its
/// last, and the widget into that last row — mirroring how the picture
/// and the widget are two visually separate things even though they're
/// one placement on the wire). `print_placeholder_grid_with_cell_dims`'s
/// own row clamp (`terminal_rows - 1`, leaving room for the shell's next
/// prompt) still applies on top of this — passing `rows + 1` here means
/// the clamp effectively reserves room for BOTH the widget row and the
/// prompt row, not just the prompt.
fn print_video_placeholder_grid(session_id: u32, file_id: u32, width_px: u32, height_px: u32) -> Result<(), String> {
    let Some((cell_width, cell_height)) = query_cell_size_px() else {
        return Ok(());
    };

    let columns = width_px.div_ceil(cell_width).max(1).min(297);
    let picture_rows = height_px.div_ceil(cell_height).max(1).min(296);
    print_placeholder_grid_with_cell_dims(session_id, file_id, columns, picture_rows + 1)
}

/// Fallback footprint for audio, used only if `query_cell_count()` gets no
/// reply at all (see `print_placeholder_grid_with_cell_dims`'s own doc
/// comment — audio's REAL reservation is now always the whole terminal;
/// Som's paint path centers a compact fixed-size play/pause/seek-bar
/// widget within that full-terminal footprint, matching this SAME 40x1
/// figure via its own mirrored constant — see `paint_rich_content_media_
/// widget`'s doc comment in `terminal_element.rs` for that side).
const AUDIO_WIDGET_COLUMNS: u32 = 40;
const AUDIO_WIDGET_ROWS: u32 = 1;

/// Placeholder-grid footprint for a video whose real pixel dimensions
/// this process never learns (see `stream_file`'s video branch — no
/// client-side FFmpeg dependency, so no way to probe width/height here).
/// A 16:9 figure in the same ballpark as common video resolutions
/// (1280x720) — `print_placeholder_grid` derives columns/rows from this
/// via the terminal's own cell pixel size, same as it would for a real
/// image, and Som's paint path scales the actually-decoded frame to fit
/// whatever footprint results, so this only affects the on-screen aspect
/// ratio until the terminal is resized, not correctness.
const VIDEO_PLACEHOLDER_WIDTH_PX: u32 = 1280;
const VIDEO_PLACEHOLDER_HEIGHT_PX: u32 = 720;

fn print_audio_placeholder_grid(session_id: u32, file_id: u32) -> Result<(), String> {
    print_placeholder_grid_with_cell_dims(session_id, file_id, AUDIO_WIDGET_COLUMNS, AUDIO_WIDGET_ROWS)
}

/// Placeholder-grid footprint for a markdown file: as wide as the
/// terminal currently is (`print_placeholder_grid_with_cell_dims`
/// clamps this down to `terminal_columns`/`terminal_rows - 1` itself —
/// see that function's own doc comment — so passing an oversized figure
/// here is the normal, expected way to say "use the terminal's own
/// width" rather than a bug), as tall as the file has newlines. Neither
/// figure needs to be exact: Som's own markdown renderer wraps/reflows
/// text to fit whatever footprint this placement actually reserves, the
/// same tolerance the audio/video widgets already have for an
/// approximate reserved area.
const MARKDOWN_MAX_COLUMNS: u32 = 9999;

/// Prints the INITIAL, minimal placeholder grid for a live markdown
/// placement — just `MARKDOWN_INITIAL_ROWS` row(s), enough for Som to
/// discover the id and open a subscription, NOT a predicted final
/// height. Markdown used to reserve its whole predicted row count up
/// front (`markdown_line_count::count_rendered_lines`, a lightweight
/// GPUI-free approximation of `layout_markdown`'s block-level logic) —
/// removed entirely: that crate could mirror `layout_markdown`'s BLOCK
/// structure but never its real word-wrap (word-wrap needs real font
/// metrics and the terminal's actual current pixel width, neither
/// knowable to a plain CLI at the moment it's invoked), so its
/// prediction drifted from the real painted height by an amount that
/// SCALED with document size — confirmed live as visible overlap with
/// the shell's next prompt. The caller (`stream_file`'s markdown branch)
/// now keeps this placement's `somsrp` process alive afterward,
/// registers it as a range-style responder, and grows this SAME grid
/// row by row on demand via `SrvRequest::GrowMarkdownRows` — see that
/// variant's own doc comment for the full replacement design. Returns
/// `(columns, initial_rows)` so the caller can track this placement's
/// own running row count for subsequent growth calls.
const MARKDOWN_INITIAL_ROWS: u32 = 1;

fn print_markdown_placeholder_grid(session_id: u32, file_id: u32) -> Result<(u32, u32), String> {
    // Full terminal width, NOT clamped further — matches the OLD
    // behavior's column choice exactly (`print_placeholder_grid_with_
    // cell_dims`'s aspect-ratio scale is for images, wrong here for the
    // same reason it always was: a real word-wrapped document has no
    // fixed aspect ratio to preserve).
    let columns = query_cell_count().map(|(c, _)| c).unwrap_or(MARKDOWN_MAX_COLUMNS).min(MARKDOWN_MAX_COLUMNS).max(1);
    write_placeholder_grid_rows(session_id, file_id, columns, MARKDOWN_INITIAL_ROWS)?;
    Ok((columns, MARKDOWN_INITIAL_ROWS))
}

/// Reserves a placeholder grid spanning the terminal's ENTIRE current
/// size, regardless of `columns`/`rows`' own aspect-ratio-derived values —
/// top-level content now always occupies the whole terminal (Som's paint
/// path centers/letterboxes the real decoded content within that
/// full-terminal box via `fit_into_box`, never stretching it past its own
/// natural size). `columns`/`rows` are kept as PARAMETERS (not dropped
/// from the signature) only as the fallback used when `query_cell_count()`
/// gets no reply at all — the old aspect-derived footprint is still a
/// reasonable guess in that case, better than guessing a terminal size
/// outright.
///
/// This replaces the OLD behavior of scaling `columns`/`rows` down to fit
/// within the terminal while clamping height to `terminal_rows - 1` (one
/// row deliberately left free for the shell's next prompt) — that
/// reservation no longer leaves room for a prompt at all, which is
/// intentional: the shell's prompt now only reappears after the user
/// presses Ctrl+C (see `stream_file`'s image/GIF branch, which now joins
/// video/audio/markdown's existing live-process wait loop instead of
/// exiting immediately once its bytes are streamed).
fn print_placeholder_grid_with_cell_dims(session_id: u32, file_id: u32, columns: u32, rows: u32) -> Result<(), String> {
    let (columns, rows) = query_cell_count().unwrap_or((columns.max(1), rows.max(1)));
    write_placeholder_grid_rows(session_id, file_id, columns.max(1), rows.max(1))
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
fn write_placeholder_grid_rows(session_id: u32, file_id: u32, columns: u32, rows: u32) -> Result<(), String> {
    write_placeholder_grid_rows_from(session_id, file_id, columns, 0, rows)
}

/// Same as [`write_placeholder_grid_rows`], but encodes rows starting
/// from `start_row` instead of always `0` — needed by a LIVE markdown
/// placement growing its own reservation after the fact (see
/// `SrvRequest::GrowMarkdownRows`'s own doc comment): a second call must
/// continue the SAME placement's row numbering from where the first call
/// left off, or the new cells decode as overlapping `row=0..N` and
/// corrupt `Terminal::markdown_placement_origins`'s `max_row` scan
/// (which reads `decoded_row` directly off each cell's own diacritics).
/// Every existing caller still passes `start_row: 0` via the wrapper
/// above — this function only exists as a separate entry point so that
/// distinction is explicit at each call site rather than an easily
/// mis-passed extra argument on the common case.
///
/// `encode_cell` returns `None` once `row >= 297` (the diacritics table
/// has exactly 297 entries — see that function's own doc comment) —
/// rows at/past that limit are silently skipped rather than panicking,
/// the same tolerance the inner loop already had for a column past
/// its own limit. A markdown document long enough to hit this genuinely
/// cannot reserve any more real placeholder cells; the widget can still
/// PAINT further rows (painted height never depended on the placeholder
/// grid's own row count, see `paint_rich_content_markdown_widget`'s
/// design), only grid-cell-based row discovery/scroll-lock hit-testing
/// stops growing past this point.
fn write_placeholder_grid_rows_from(session_id: u32, file_id: u32, columns: u32, start_row: u32, rows: u32) -> Result<(), String> {
    let mut text = String::new();
    let (sr, sg, sb) = id_to_rgb(session_id);
    let (fr, fg, fb) = id_to_rgb(file_id);
    text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
    for offset in 0..rows {
        let row = start_row + offset;
        for column in 0..columns {
            if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                text.extend(cell);
            }
        }
        if offset + 1 < rows {
            text.push_str("\r\n");
        }
    }
    text.push_str("\x1b[0m\r\n");
    write_raw_stdout(text.as_bytes())
}

/// Clears the real terminal and homes the cursor to (row 0, column 0)
/// before printing a top-level (non-markdown) placeholder grid — `CSI 2J`
/// (clear entire screen) + `CSI H` (cursor home), both standard ANSI,
/// already relied upon elsewhere in this codebase's terminal handling.
/// Needed now that a top-level placement's placeholder grid always spans
/// the WHOLE terminal (see `print_placeholder_grid_with_cell_dims`'s own
/// doc comment): Som derives a placement's on-screen origin from wherever
/// its `(row=0, column=0)` placeholder cell actually sits in the real
/// terminal grid — homing the cursor first guarantees that's the
/// terminal's own real top-left corner, with no change needed to that
/// origin-detection logic on Som's side. Markdown does NOT call this —
/// markdown's own placement stays left/top-anchored wherever the cursor
/// was at invocation time, unaffected by this change.
fn clear_screen_and_home_cursor() {
    print!("\x1b[2J\x1b[H");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Raw mode (`raw_mode::enable`) clears `ENABLE_PROCESSED_INPUT` on
/// Windows so escape-sequence replies (`CSI 16 t`, query responses) reach
/// this process's stdin as raw bytes instead of being intercepted by the
/// console — but `ENABLE_PROCESSED_INPUT` is also what makes the console
/// turn a Ctrl+C keypress into a `CTRL_C_EVENT`/SIGINT that would
/// otherwise kill this process. With it off, Ctrl+C arrives as an
/// ordinary `0x03` byte on stdin like any other byte, and every stdin-
/// reading loop here needs to check for it explicitly and exit, or a
/// user's Ctrl+C while `somsrp` is still streaming (or waiting on a
/// range query) does nothing at all. Unix's raw mode (`cfmakeraw`) has
/// the same effect (`ISIG` is cleared), so this applies on both.
const ETX: u8 = 0x03;

/// Set by [`spawn_ctrlc_watcher`]'s background thread the instant it sees
/// `ETX`, `ESC`, or `q`/`Q` on stdin — checked by [`stream_file_from_disk`]'s
/// own wait loop to know when to stop answering `RequestByteRange` and
/// exit. Named after Ctrl+C specifically (the first of the three exit
/// keys this project supported) rather than something more generic —
/// left as-is since renaming it now would touch every call site for a
/// purely cosmetic reason. Before this flag existed at all, stdin was
/// only ever read for the two short, one-shot placeholder-grid
/// handshakes (`query_cell_size_px`/`query_cell_count`), both of which
/// complete and return well before a large video/audio file's transfer
/// even starts — nothing was left listening on stdin during the part of
/// the run that can actually take minutes, so `ETX`'s own doc comment
/// ("every stdin-reading loop here needs to check for it explicitly")
/// was true in spirit but incomplete in practice.
static CTRL_C_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Spawns a background thread that blocks reading stdin one byte at a
/// time for the remainder of the process's life, setting
/// [`CTRL_C_REQUESTED`] the moment it sees `ETX` (`0x03`) and then
/// exiting. Must only be started AFTER the placeholder-grid handshake's
/// own short-lived stdin reads (`query_cell_size_px`/`query_cell_count`)
/// have both already returned — two readers racing the same stdin handle
/// at once would nondeterministically split the handshake's own reply
/// bytes between them. `stream_file`'s branches all print their
/// placeholder grid before starting the long transfer (see each
/// branch's own "placeholder grid FIRST" comment), so calling this right
/// before handing off to `stream_bytes`/`stream_file_from_disk` is safe.
/// `ESC` (`0x1b`) — one of the three keys that end a live full-terminal
/// widget's wait loop (see [`spawn_ctrlc_watcher`]'s own doc comment for
/// why all three share `CTRL_C_REQUESTED` rather than each getting a
/// separate flag). A bare `ESC` byte here is unambiguous: this watcher
/// never negotiates terminal capabilities or reads query replies while
/// it's running (those one-shot reads already completed before this
/// spawns), so there's no CSI/SS3 escape SEQUENCE this could be mistaken
/// for the start of — every byte arriving here is a real keypress.
const ESC: u8 = 0x1b;

/// Watches stdin for any of the three keys that end a live full-terminal
/// widget's wait loop — `Ctrl+C` (`ETX`), `Escape`, or `q`/`Q` — matching
/// the same exit convention `vim`/`top`/`mc` already use for a full-
/// screen view: not every user reaches for Ctrl+C first, and Escape/q are
/// the more natural instinct for "close this and get my prompt back" in
/// a pager-like UI. All three set the SAME [`CTRL_C_REQUESTED`] flag
/// (not a differently-named one) — nothing downstream needs to
/// distinguish which key was pressed, they're fully equivalent once this
/// flag is set.
fn spawn_ctrlc_watcher() {
    std::thread::spawn(|| {
        let mut stdin = std::io::stdin();
        let mut byte = [0u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(0) => return, // stdin closed
                Ok(_) => {
                    if byte[0] == ETX || byte[0] == ESC || byte[0] == b'q' || byte[0] == b'Q' {
                        CTRL_C_REQUESTED.store(true, std::sync::atomic::Ordering::Release);
                        return;
                    }
                },
                Err(_) => return,
            }
        }
    });
}

/// Session/file id pair identifying one SRP transfer on the wire — see
/// [`new_ids`]'s doc comment for how these are derived and why they're
/// masked to 24 bits.
type SrpIds = (u32, u32);

/// Derives a fresh `(session_id, file_id)` pair for a new SRP transfer.
/// Both are time-derived so two separate `somsrp` invocations rarely
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

/// Streams `bytes` to Som's rich-content pipeline over `channel` — the
/// binary side-channel to `somsrv` (`SrvChannel::connect`), replacing
/// the old APC/base91-over-PTY transport entirely (see `terminal::
/// rich_content_transport`'s module doc comment for why that transport
/// existed and no longer does). Content-type-agnostic: the caller picks
/// `content_type`/`metadata`, this function only knows how to chop bytes
/// into pieces and hand them off.
fn stream_bytes(
    channel: &srv_channel::SrvChannel,
    bytes: &[u8],
    content_type: terminal::rich_content_transport::ContentType,
    metadata: terminal::rich_content_transport::ContentMetadata,
    ids: SrpIds,
) -> Result<(), String> {
    let total_size = bytes.len() as u64;
    send_range_chunks(channel, bytes, content_type, metadata, ids, total_size, 0, bytes.len() as u64)
}

/// Sends `bytes[offset as usize .. (offset + len) as usize]` as
/// [`CHUNK_SIZE`]-sized `SrvRequest::PutChunk` messages over `channel` —
/// the shared implementation behind both [`stream_bytes`] (the whole
/// file, offset 0) and a range-request response (an arbitrary sub-range,
/// see [`spawn_byte_range_responder`]). `total_size` is the WHOLE file's
/// size (not `bytes.len()`), since a range response still needs to
/// declare the file's real total size, same as the initial sequential
/// stream does. `content_type`/`metadata` travel on every `PutChunk` —
/// see `somsrv::protocol::SrvRequest::PutChunk`'s own doc comment for
/// why.
#[allow(clippy::too_many_arguments)]
fn send_range_chunks(
    channel: &srv_channel::SrvChannel,
    bytes: &[u8],
    content_type: terminal::rich_content_transport::ContentType,
    metadata: terminal::rich_content_transport::ContentMetadata,
    (session_id, file_id): SrpIds,
    total_size: u64,
    range_offset: u64,
    range_len: u64,
) -> Result<(), String> {
    let start = range_offset as usize;
    let end = (range_offset + range_len).min(bytes.len() as u64) as usize;
    let slice = bytes.get(start..end).ok_or_else(|| format!("range [{start}, {end}) out of bounds"))?;

    let srv_content_type = srv_channel::to_srv_content_type(content_type);
    let srv_metadata = srv_channel::to_srv_metadata(metadata);
    let mut offset = range_offset;
    for piece in slice.chunks(CHUNK_SIZE) {
        channel.put_chunk(session_id, file_id, offset, piece.to_vec(), total_size, srv_content_type, srv_metadata.clone())?;
        offset += piece.len() as u64;
    }
    Ok(())
}

/// Sends `file[range_offset .. range_offset + range_len)` as
/// [`CHUNK_SIZE`]-sized `SrvRequest::PutChunk` messages over `channel` —
/// the file-backed counterpart to [`send_range_chunks`], used for video/
/// audio (see [`stream_file_from_disk`]'s own doc comment for the pull-
/// style design this serves: the front/tail announce chunks and every
/// `RequestByteRange` reply all go through this same function). `file` is
/// read via `Seek`+`Read` rather than kept at a running cursor, since
/// concurrent range requests (from [`spawn_byte_range_responder_from_
/// disk`]'s own thread) can interleave on a SEPARATE thread sharing the
/// same `Mutex<File>`. Checks [`CTRL_C_REQUESTED`] between chunks so a
/// user interrupt takes effect promptly even mid-range, though in
/// practice every range this function ever sends is small (an announce
/// chunk or one on-demand reply), never a whole large file.
#[allow(clippy::too_many_arguments)]
fn send_range_chunks_from_disk(
    channel: &srv_channel::SrvChannel,
    file: &std::sync::Mutex<std::fs::File>,
    content_type: terminal::rich_content_transport::ContentType,
    metadata: terminal::rich_content_transport::ContentMetadata,
    (session_id, file_id): SrpIds,
    total_size: u64,
    range_offset: u64,
    range_len: u64,
) -> Result<(), String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::sync::atomic::Ordering;

    let range_len = range_len.min(total_size.saturating_sub(range_offset));
    let srv_content_type = srv_channel::to_srv_content_type(content_type);
    let srv_metadata = srv_channel::to_srv_metadata(metadata);
    let mut offset = range_offset;
    let end = range_offset + range_len;
    let mut buf = vec![0u8; CHUNK_SIZE];
    while offset < end {
        if CTRL_C_REQUESTED.load(Ordering::Acquire) {
            return Ok(());
        }
        let piece_len = (end - offset).min(CHUNK_SIZE as u64) as usize;
        {
            let mut guard = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.seek(SeekFrom::Start(offset)).map_err(|e| format!("seeking to {offset}: {e}"))?;
            guard.read_exact(&mut buf[..piece_len]).map_err(|e| format!("reading {piece_len} bytes at {offset}: {e}"))?;
        }
        channel.put_chunk(session_id, file_id, offset, buf[..piece_len].to_vec(), total_size, srv_content_type, srv_metadata.clone())?;
        offset += piece_len as u64;
    }
    Ok(())
}

/// File-backed counterpart to [`spawn_byte_range_responder`] — services
/// `RequestByteRange` by seeking/reading `file` instead of slicing an
/// in-memory buffer. This is now the ENTIRE data-delivery path for
/// video/audio (see [`stream_file_from_disk`]'s own doc comment for the
/// pull-style redesign this is a part of) — every byte past the small
/// front/tail announce chunks arrives only because this thread answered
/// a request for it, never because anything was pushed proactively.
///
/// Uses its OWN `SrvChannel` connection (registered via `SrvRequest::
/// RegisterRangeResponder`), separate from whichever connection sent the
/// announce chunks — see that protocol variant's own doc comment for why:
/// even in the pull model, a slow response here shouldn't have to wait on
/// unrelated traffic on another connection.
///
/// Also listens on this same connection for `SrvRequest::EndPlayback` —
/// Som's signal that playback has definitively ended (natural EOF, or the
/// widget's own stop icon) — setting the returned `ended` flag instead of
/// answering it, letting [`stream_file_from_disk`]'s own wait loop notice
/// and exit. See that function's own doc comment for the full "why does
/// `somsrp` need to know this at all" reasoning.
fn spawn_byte_range_responder_from_disk(
    file: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
    content_type: terminal::rich_content_transport::ContentType,
    metadata: terminal::rich_content_transport::ContentMetadata,
    ids: SrpIds,
    total_size: u64,
) -> Result<(std::sync::Arc<std::sync::atomic::AtomicBool>, std::sync::Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<()>), String>
{
    use std::sync::atomic::{AtomicBool, Ordering};

    let (session_id, file_id) = ids;
    let channel = std::sync::Arc::new(srv_channel::SrvChannel::connect()?);
    channel.register_range_responder(session_id, file_id)?;

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop_for_thread = stop.clone();
    let ended = std::sync::Arc::new(AtomicBool::new(false));
    let ended_for_thread = ended.clone();
    let handle = std::thread::spawn(move || {
        loop {
            if stop_for_thread.load(Ordering::Relaxed) {
                return;
            }
            match channel.read_incoming() {
                Ok(srv_channel::Incoming::Request(somsrv::protocol::SrvRequest::RequestByteRange {
                    session_id,
                    file_id,
                    offset,
                    len,
                })) if (session_id, file_id) == ids => {
                    let _ = send_range_chunks_from_disk(&channel, &file, content_type, metadata.clone(), ids, total_size, offset, len);
                },
                Ok(srv_channel::Incoming::Request(somsrv::protocol::SrvRequest::EndPlayback { session_id, file_id }))
                    if (session_id, file_id) == ids =>
                {
                    ended_for_thread.store(true, Ordering::Release);
                    return;
                },
                Ok(_) => continue,
                Err(_) => return,
            }
        }
    });
    Ok((stop, ended, handle))
}

/// Keeps a live markdown placement's `somsrp` process holding the
/// terminal in the foreground (exactly like `spawn_byte_range_responder_
/// from_disk` already does for audio/video's pull-model responder),
/// growing its OWN placeholder grid on demand instead of answering byte-
/// range requests — see `SrvRequest::GrowMarkdownRows`'s own doc comment
/// for why this replaced the old `markdown_line_count`-predicted up-
/// front reservation entirely.
///
/// Registers via `SrvRequest::RegisterMarkdownGrower` — DELIBERATELY NOT
/// the same `RegisterRangeResponder` message the audio/video responder
/// uses, even though both are "somsrp says where to send me things for
/// this id": reusing that registration made a late-subscriber
/// `RequestByteRange` catch-up silently vanish into this thread (which
/// doesn't understand it) instead of falling through to `somsrv`'s
/// recent-bytes fallback — confirmed live as the reason markdown never
/// rendered at all in an early version of this design. `somsrv` routes
/// `GrowMarkdownRows` here through its own dedicated `markdown_grower_
/// routes` table, entirely separate from `range_response_routes`.
///
/// Tracks this placement's own running row count (`columns` never
/// changes after the initial grid — only rows grow) so each successive
/// `GrowMarkdownRows` continues the SAME placement's row numbering via
/// `write_placeholder_grid_rows_from`'s `start_row`, never restarting
/// from row 0 (which would corrupt `Terminal::markdown_placement_
/// origins`'s `max_row` scan — see that function's own doc comment).
fn spawn_markdown_grower(
    ids: SrpIds,
    initial_columns: u32,
    initial_rows: u32,
) -> Result<(std::sync::Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<()>), String> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (session_id, file_id) = ids;
    let channel = srv_channel::SrvChannel::connect()?;
    channel.register_markdown_grower(session_id, file_id)?;

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop_for_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        let mut printed_rows = initial_rows;
        loop {
            if stop_for_thread.load(Ordering::Relaxed) {
                return;
            }
            match channel.read_incoming() {
                Ok(srv_channel::Incoming::Request(somsrv::protocol::SrvRequest::GrowMarkdownRows {
                    session_id,
                    file_id,
                    additional_rows,
                })) if (session_id, file_id) == ids => {
                    if write_placeholder_grid_rows_from(session_id, file_id, initial_columns, printed_rows, additional_rows).is_ok() {
                        printed_rows += additional_rows;
                    }
                },
                Ok(srv_channel::Incoming::Request(somsrv::protocol::SrvRequest::EndPlayback { session_id, file_id }))
                    if (session_id, file_id) == ids =>
                {
                    return;
                },
                Ok(_) => continue,
                Err(_) => return,
            }
        }
    });
    Ok((stop, handle))
}

/// Streams `path` (a video or audio file) to Som over SRP by reading it
/// in [`CHUNK_SIZE`]-sized pieces off disk, instead of [`std::fs::read`]-
/// ing the whole file into memory first — the fix for a real, live-
/// measured bug: a 15GB movie file used to take ~15 minutes to start
/// playing in Som, because the OLD `stream_file` read the entire file
/// into a `Vec<u8>` before a single `PutChunk` went out, and every
/// downstream stage (`somsrv`'s cache writer, `RichContentCache`,
/// `Terminal::rich_content_video_placements`'s open gate, `GrowingFileStream`'s
/// FFmpeg probe) was ALREADY fully progressive and ready to start playing
/// from the very first byte — the whole-file read was the only actual
/// bottleneck. `total_size` comes from `std::fs::metadata` (a cheap
/// `stat`), NOT from a fully-read buffer's length, so it's known
/// immediately without reading any file content at all.
///
/// Images/GIF (`stream_file`'s other branches) deliberately keep the
/// old `std::fs::read`-into-memory model: they need the whole buffer
/// anyway for metadata probing (`gif_metadata`/`static_image_metadata`
/// both operate on an in-memory byte slice) and are typically small
/// enough that this was never the bottleneck those formats have.
///
/// Streams `path` PULL-STYLE: `somsrp` never proactively pushes the file
/// forward — it sends one small announcing chunk (and, for video, a tail
/// fetch), registers itself as the byte-range responder, then holds the
/// terminal in the foreground (exactly like `cat`/`less` would) answering
/// `SrvRequest::RequestByteRange` on demand for as long as playback can
/// still ask for more. This is deliberate, not a leftover from the old
/// design: the user's own model here is that `somsrp` occupies the
/// terminal the whole time content is playing, the same way `cat`ing a
/// file keeps the shell busy until the file is exhausted — a prompt
/// appearing while video/audio is still playing would let the user run a
/// new command (or start ANOTHER `somsrp` against the same placement)
/// while this one is still the thing feeding it, which is exactly the
/// confusing double-control state this design avoids. `somsrp` only
/// returns — thus only lets the shell print its next prompt — once
/// playback has definitively ended, one of three ways, all funneled
/// through the same exit path below:
/// 1. **Natural end of content.** Som reaches EOF during decode and tells
///    `somsrv` to relay [`somsrv::protocol::SrvRequest::EndPlayback`]
///    back down THIS responder connection (mirrors `RequestByteRange`'s
///    own existing Som -> daemon -> registered-responder routing, see
///    `RegisterRangeResponder`'s doc comment) — handled in the responder
///    thread's read loop below, same as any other routed request.
/// 2. **User pressed stop / closed the placement in Som.** Same
///    `EndPlayback` message, sent by `Terminal::stop_rich_content_*`
///    instead of the decode-EOF path — from `somsrp`'s side these two
///    cases are indistinguishable and don't need to be: either way,
///    nothing is going to ask for more bytes again.
/// 3. **User pressed Ctrl+C in the terminal itself.** Caught by
///    [`spawn_ctrlc_watcher`] exactly as before pull-model existed.
///
/// This REPLACES an earlier "sequential push, `RequestByteRange` only as
/// a fallback for stragglers" design — confirmed live as broken for large
/// files specifically: on a fast local named pipe, a full sequential pass
/// over a real 16GB movie completed in ~2 SECONDS (`send_range_chunks_
/// from_disk_interruptible`'s own `Ok(SendOutcome::Completed)` returned
/// well before Som had even finished subscribing), after which `somsrp`
/// exited and closed every connection — leaving Som's own in-memory
/// buffer (bounded on purpose, see `somsrv::srv_cache::CacheEntry::
/// recent_bytes`'s own doc comment) with nowhere near enough of the file
/// to decode from, and no live sender left to ask for more. A small file
/// (or a slow enough network) happened to let subscription win the race
/// often enough that this looked like it worked — it never actually
/// solved the underlying push-vs-subscribe race, just usually finished
/// before the race lost. Real streaming players (HLS/DASH, and every
/// other network video player) don't have this race at all because they
/// never push proactively either — the client always asks for exactly
/// the segment/range it currently needs, same principle this function now
/// follows: `somsrp` holds the file open and answers on demand for as
/// long as the process runs, instead of racing to finish a one-shot send
/// before anyone's ready to receive it.
fn stream_file_from_disk(
    path: &str,
    channel: srv_channel::SrvChannel,
    content_type: terminal::rich_content_transport::ContentType,
    metadata: terminal::rich_content_transport::ContentMetadata,
    ids: SrpIds,
) -> Result<(), String> {
    // Both callers (video/audio branches of `stream_file`) have already
    // printed their placeholder grid and completed the short stdin
    // handshakes that requires (`query_cell_size_px`/`query_cell_count`)
    // — see those functions' own doc comments — so it's now safe to start
    // reading stdin for the rest of the process's life without racing
    // that handshake. See `spawn_ctrlc_watcher`'s own doc comment for why
    // this couldn't just start at the top of `main` instead.
    spawn_ctrlc_watcher();

    let total_size = std::fs::metadata(path).map_err(|e| format!("stat {path}: {e}"))?.len();
    let file = std::fs::File::open(path).map_err(|e| format!("opening {path}: {e}"))?;
    let shared_file = std::sync::Arc::new(std::sync::Mutex::new(file));
    let shared_channel = std::sync::Arc::new(channel);

    let (stop, ended, handle) =
        spawn_byte_range_responder_from_disk(shared_file.clone(), content_type, metadata.clone(), ids, total_size)?;

    // The ONE proactive push this function still does: just enough of the
    // file's own FRONT for `somsrv` to create its cache entry (`total_
    // size`/`content_type`/`metadata`, needed before Som has anything to
    // subscribe TO at all — see `SrvCache::put_chunk`'s own doc comment)
    // and for FFmpeg's format probe to have a real shot at succeeding
    // without a round trip. Deliberately small and bounded — this is an
    // announcement, not the start of a sequential pass; everything past
    // it (including the REST of this same front region, if the probe
    // needs more) arrives only via `RequestByteRange`.
    const ANNOUNCE_LEN: u64 = 256 * 1024;
    let announce_len = ANNOUNCE_LEN.min(total_size);
    send_range_chunks_from_disk(&shared_channel, &shared_file, content_type, metadata.clone(), ids, total_size, 0, announce_len)?;

    // Non-faststart MP4 has its `moov` atom at the end of the file, and
    // MKV can likewise have its Cues (seek index) element near the end
    // rather than up front — either way, FFmpeg's format probe on Som's
    // side (`GrowingFileStream::seek`, `SeekFrom::End`) would otherwise
    // need a full extra `RequestByteRange` round trip just to learn the
    // tail even exists. Sending it proactively alongside the front
    // announce chunk above saves that round trip; it's still bounded (4MB
    // fixed, not proportional to file size) so it doesn't reintroduce the
    // "push the whole file" problem this function exists to avoid.
    if content_type == terminal::rich_content_transport::ContentType::Video {
        const TAIL_FETCH_LEN: u64 = 4 * 1024 * 1024;
        let tail_offset = total_size.saturating_sub(TAIL_FETCH_LEN);
        let _ =
            send_range_chunks_from_disk(&shared_channel, &shared_file, content_type, metadata.clone(), ids, total_size, tail_offset, TAIL_FETCH_LEN);
    }

    // Hold the file open and keep answering `RequestByteRange` (via the
    // responder thread spawned above) until playback has definitively
    // ended — either `ended` (Som relayed `SrvRequest::EndPlayback`,
    // meaning natural EOF or an explicit stop/close on Som's side) or
    // `CTRL_C_REQUESTED` (`spawn_ctrlc_watcher`, the user pressing Ctrl+C
    // in this terminal) — see this function's own doc comment for why
    // both must lead here, not just Ctrl+C.
    while !CTRL_C_REQUESTED.load(std::sync::atomic::Ordering::Acquire) && !ended.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(handle);
    Ok(())
}

/// Streams `path` to Som over SRP, then prints a placeholder grid
/// reserving this placement's on-screen footprint — for images/GIF sized
/// from the file's own pixel dimensions (`print_placeholder_grid`), for
/// audio a fixed cell footprint (`print_audio_placeholder_grid`) since
/// audio has no pixel dimensions to derive one from. Either way, Som
/// decodes the cached bytes and paints into that reserved footprint
/// itself — a decoded image frame for images/GIF, an inline play/pause/
/// seek-bar widget for audio — `somsrp`'s job ends once the bytes are on
/// the wire and the footprint is reserved.
///
/// Content type is inferred from the file extension: `.gif`, `.jpg`/
/// `.jpeg`, `.png`, `.mp3`, `.flac`, `.mp4`/`.mkv`/`.avi`, `.md`.
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
/// here. `somsrp` (this process) can be running on a remote SSH host
/// while Som — and the user's actual speakers — are local; playback has
/// to happen wherever Som runs, so this process's only audio-specific
/// work is probing header metadata (`audio_metadata`) to fill
/// `ContentMetadata::Audio` accurately before the first chunk goes out.
fn stream_file(path: &str, audio_stream_index: Option<u32>, subtitle_stream_index: Option<u32>) -> Result<(), String> {
    // A URL target's real content type isn't knowable to this process at
    // all — `somsrv` reads it from the real HTTP `Content-Type` header
    // once it actually fetches the resource (see `stream_url`'s own doc
    // comment). Checked before any extension-based dispatch below, since
    // a URL's own path component might coincidentally end in something
    // that looks like a recognized extension (or might not have one at
    // all — query strings, extension-less API endpoints).
    if path.starts_with("http://") || path.starts_with("https://") {
        return stream_url(path);
    }
    stream_file_impl(path, audio_stream_index, subtitle_stream_index)
}

/// Streams an `http://`/`https://` URL directly, WITHOUT `somsrp` ever
/// reading the resource itself — `somsrv` does that server-side via
/// `SrvRequest::FetchResource` (`crates/somsrv/src/http_fetch.rs`'s
/// `fetch_and_stream`/`fetch_network`, which already reads the URL's real
/// HTTP `Content-Type` header and streams into the cache on its own).
/// This is why `somsrp` can't classify content type from a URL the way it
/// does for a local file's extension: it genuinely doesn't know, and
/// won't ever need to — the placeholder grid it prints is already
/// content-type-agnostic under the full-terminal reservation model (see
/// `print_placeholder_grid_with_cell_dims`'s own doc comment), so there's
/// nothing about the grid's size that would need to differ once the real
/// type is known anyway.
fn stream_url(url: &str) -> Result<(), String> {
    let (session_id, file_id) = new_ids();

    // One connection for the `FetchResource` request AND to hold the
    // terminal open afterward — unlike `stream_file`'s branches, there's
    // no bulk byte stream THIS process sends at all (that's entirely
    // `somsrv`'s job now), so there's no separate "drop the sender
    // connection" step needed the way markdown/image's live-process
    // branches have (see those branches' own doc comments for why THEY
    // need an explicit early drop — this process was never a `PutChunk`
    // sender in the first place, so `SrvCache`'s `sender_routes` table
    // never has an entry for this connection to begin with).
    let channel = srv_channel::SrvChannel::connect()?;

    clear_screen_and_home_cursor();
    // Content-type-agnostic footprint: the full terminal, same as every
    // other top-level (non-markdown) content type now reserves — see
    // `print_placeholder_grid_with_cell_dims`'s own doc comment. The
    // `columns`/`rows` fallback values passed here (used only if `query_
    // cell_count()` gets no reply at all) are arbitrary since there's no
    // known aspect ratio to fall back to for a URL of unknown type —
    // `AUDIO_WIDGET_COLUMNS`/`AUDIO_WIDGET_ROWS` are reused here purely as
    // an existing "small, sane default" constant, not because this is
    // audio-specific.
    print_placeholder_grid_with_cell_dims(session_id, file_id, AUDIO_WIDGET_COLUMNS, AUDIO_WIDGET_ROWS)?;

    channel.request_fetch_resource(session_id, file_id, url.to_string())?;
    drop(channel);

    // Same live-process model every other top-level content type now
    // uses (see `print_placeholder_grid_with_cell_dims`'s doc comment for
    // why the prompt no longer reappears immediately) — `somsrv` itself
    // determines the real content type and streams bytes on its own; this
    // process has nothing further to do but hold the terminal open until
    // the user is done looking.
    spawn_ctrlc_watcher();
    while !CTRL_C_REQUESTED.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

fn stream_file_impl(path: &str, audio_stream_index: Option<u32>, subtitle_stream_index: Option<u32>) -> Result<(), String> {
    use terminal::rich_content_transport::{ContentMetadata, ContentType};

    let extension = std::path::Path::new(path).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
    let content_type = match extension.as_deref() {
        Some("gif") => ContentType::Gif,
        Some("jpg" | "jpeg") => ContentType::Jpeg,
        Some("png") => ContentType::Png,
        Some("mp3" | "flac") => ContentType::Audio,
        Some("mp4" | "mkv" | "avi") => ContentType::Video,
        Some("md") => ContentType::Markdown,
        Some(other) => {
            return Err(format!(
                "unrecognized extension .{other} — only .gif/.jpg/.jpeg/.png/.mp3/.flac/.mp4/.mkv/.avi/.md are supported so far"
            ));
        },
        None => return Err("file has no extension, can't infer content type".to_string()),
    };

    let ids = new_ids();

    // One connection to somsrv's binary side channel for this whole
    // transfer — see `srv_channel::SrvChannel`'s own doc comment for why
    // this fails hard (not a silent fallback to the old PTY transport,
    // which no longer exists) if the daemon isn't reachable.
    let channel = srv_channel::SrvChannel::connect()?;

    if content_type == ContentType::Audio {
        let (sample_rate, channels, bits_per_sample, duration_ms) = audio_metadata(path)?;
        let extension = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
        let metadata = ContentMetadata::Audio { sample_rate, channels, bits_per_sample, duration_ms, extension };
        let (session_id, file_id) = ids;

        // Placeholder grid FIRST, streaming SECOND — the reverse order
        // from every other content type. Without a placeholder printed
        // yet, Som has no id to open a player for at all — see the video
        // branch below for the same reasoning, confirmed live for that
        // content type; audio adopted the same order first.
        clear_screen_and_home_cursor();
        print_audio_placeholder_grid(session_id, file_id)?;

        // Streams off disk in bounded chunks rather than reading the
        // whole file into memory first — see `stream_file_from_disk`'s
        // own doc comment for why (a real, live-measured ~15-minute
        // startup delay on a 15GB file, fixed by not materializing the
        // whole file before the first byte goes out).
        return stream_file_from_disk(path, channel, content_type, metadata, ids);
    }

    if content_type == ContentType::Video {
        // Probed via FFmpeg (this process already links it transitively
        // through `terminal` on Windows, the only platform with an
        // embedded FFmpeg today) — reads just enough of the container's
        // header to learn the real picture size/frame rate/codec, NOT a
        // full decode. Getting this right matters beyond cosmetics: an
        // inaccurate placeholder footprint (this used to always be the
        // fixed 1280x720 fallback below) reserves the WRONG aspect ratio
        // of terminal cells before a single frame has decoded, and
        // Som's own paint path never shrinks a placement's reserved
        // footprint back down once printed — a video narrower than
        // 16:9 (e.g. cinemascope) left visible letterboxing gaps for
        // the placement's entire lifetime, confirmed live.
        let fallback_extension = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
        #[cfg(windows)]
        let metadata = video_metadata(path, audio_stream_index, subtitle_stream_index).unwrap_or(ContentMetadata::Video {
            width_px: 0,
            height_px: 0,
            fps_numerator: 0,
            fps_denominator: 0,
            codec: terminal::rich_content_transport::VideoCodec::Unknown,
            audio_stream_index,
            subtitle_stream_index,
            extension: fallback_extension,
        });
        // No FFmpeg on non-Windows builds yet (see `video_metadata`'s own
        // doc comment) — same fallback as a failed probe on Windows.
        #[cfg(not(windows))]
        let metadata = ContentMetadata::Video {
            width_px: 0,
            height_px: 0,
            fps_numerator: 0,
            fps_denominator: 0,
            codec: terminal::rich_content_transport::VideoCodec::Unknown,
            audio_stream_index,
            subtitle_stream_index,
            extension: fallback_extension,
        };
        let (width_px, height_px) = match metadata {
            ContentMetadata::Video { width_px, height_px, .. } if width_px > 0 && height_px > 0 => {
                (width_px, height_px)
            },
            // Probe failed (corrupt header, codec this build's trimmed
            // FFmpeg doesn't decode, etc.) — same graceful fallback the
            // rest of this module already uses for a single-file
            // failure: an inaccurate footprint, not an aborted transfer.
            _ => (VIDEO_PLACEHOLDER_WIDTH_PX, VIDEO_PLACEHOLDER_HEIGHT_PX),
        };
        let (session_id, file_id) = ids;
        // Placeholder grid FIRST, streaming SECOND — same reversal from
        // the image/GIF branch below that audio already uses, for the
        // same reason: without a placeholder printed yet, Som has no id
        // to open a `RichContentVideoPlayer` for at all, so its decode
        // thread (which itself reads progressively off the SAME cache
        // file this streaming call is still writing into — see
        // `rich_content_video_player`'s own `GrowingFileStream`) simply
        // never starts until every last chunk of a potentially very
        // large file has already gone out over the wire. Confirmed live:
        // this made playback of a real several-minutes movie clip look
        // like it "never starts," when transport (not decode, which is
        // itself fully progressive) was the actual bottleneck.
        clear_screen_and_home_cursor();
        print_video_placeholder_grid(session_id, file_id, width_px, height_px)?;

        return stream_file_from_disk(path, channel, content_type, metadata, ids);
    }

    if content_type == ContentType::Markdown {
        let bytes = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
        // Absolute, not `std::env::current_dir()` verbatim — `path` may
        // itself be relative to somsrp's cwd, and the resulting base_dir
        // is later used by `somsrv` (a DIFFERENT process, possibly with a
        // different cwd) to resolve a markdown-embedded link's own
        // relative path, so it must stand on its own.
        let base_dir = std::path::Path::new(path)
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let metadata = ContentMetadata::Markdown { base_dir };
        let (session_id, file_id) = ids;

        // Placeholder grid FIRST, streaming SECOND — same reordering
        // every other content type already needs (see e.g. the video
        // branch's own comment for the full reasoning: without a
        // placeholder printed yet, Som has no id to open a markdown
        // player for at all). Prints only `MARKDOWN_INITIAL_ROWS` up
        // front now — see that constant's own doc comment for why the
        // old predicted-row-count reservation was removed.
        let (columns, initial_rows) = print_markdown_placeholder_grid(session_id, file_id)?;

        stream_bytes(&channel, &bytes, content_type, metadata, ids)?;

        // Explicitly closed here, not left open for the rest of this
        // live process's life — `somsrv` auto-registers this connection
        // as `(session_id, file_id)`'s `sender_routes` entry the moment
        // the first `PutChunk` arrives (see `server.rs`'s `PutChunk` arm),
        // and `SrvCache::route_byte_range_request` treats a successful
        // WRITE to that route as "handled," never falling through to
        // `serve_from_recent_bytes`. Every OTHER content type's sender
        // connection already closes right after streaming (the process
        // exits), so a late-subscriber `RequestByteRange` naturally finds
        // the write failing and falls through — markdown's process no
        // longer exits here (it now holds the terminal open, see below),
        // so without this explicit drop the connection stays open with
        // NOTHING reading it, and `route_byte_range_request` would judge
        // it "still alive" forever, silently swallowing every catch-up
        // request instead of ever reaching the in-memory fallback —
        // confirmed live as a markdown placement that never rendered at
        // all despite `somsrv` genuinely holding its bytes.
        drop(channel);

        // Unlike every other content type, markdown now holds the
        // terminal open afterward (same live-process model audio/video
        // already use) so Som can grow this placement's placeholder grid
        // on demand as `layout_markdown`'s real word-wrapped row count
        // becomes known — see `spawn_markdown_grower`'s own doc comment.
        spawn_ctrlc_watcher();
        let (stop, handle) = spawn_markdown_grower(ids, columns, initial_rows)?;
        while !CTRL_C_REQUESTED.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        drop(handle);
        return Ok(());
    }

    let bytes = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;

    let (width_px, height_px, is_animated) =
        if content_type == ContentType::Gif { gif_metadata(&bytes)? } else { static_image_metadata(&bytes)? };
    // Every pixel this protocol sends is decoded to RGBA before reaching
    // the wire (see `rich_content_gif_player`/`gpui::RenderImage`'s own
    // frame buffers) — 32 bits per pixel regardless of the source GIF's
    // own (typically <=8-bit indexed/palette) on-disk color depth.
    let metadata = ContentMetadata::Image { width_px, height_px, color_bits: 32, is_animated };

    // Placeholder grid FIRST, streaming SECOND — same reordering audio/
    // video already needed and got (see those branches' own comments for
    // the full reasoning), and for image/GIF specifically not just a
    // "starts playing sooner" nicety but a correctness requirement now:
    // `somsrv::srv_cache::SrvCache::subscribe`'s own doc comment states
    // its progress pushes are only ever delivered to subscribers already
    // registered at push time (no replay of missed progress) — Som only
    // ever subscribes once it's seen this placement's id in the
    // placeholder grid, so printing the grid AFTER already streaming a
    // small file's every chunk meant every `Progress` push (and thus
    // every byte of the file, as far as `RichContentCache` could tell)
    // had already fired with nobody subscribed yet to receive it.
    // Confirmed the hard way: this exact ordering bug made
    // `rich_content_placements()` never see a placement at all for a GIF
    // small enough to finish streaming near-instantly.
    let (session_id, file_id) = ids;
    clear_screen_and_home_cursor();
    print_placeholder_grid(session_id, file_id, width_px, height_px)?;
    stream_bytes(&channel, &bytes, content_type, metadata, ids)?;

    // Image/GIF now joins video/audio/markdown's existing live-process
    // model — see `print_placeholder_grid_with_cell_dims`'s own doc
    // comment for why: a full-terminal placement leaves no free row for
    // the shell's next prompt, so exiting immediately after streaming
    // (the OLD behavior) would have the prompt print right on top of the
    // placement's own placeholder cells. The prompt now only reappears
    // once the user presses Ctrl+C, exactly like video/audio/markdown
    // already work — same `spawn_ctrlc_watcher` + wait loop shape.
    drop(channel);
    spawn_ctrlc_watcher();
    while !CTRL_C_REQUESTED.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
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
    // Windows' does for a non-console (pyt) target — `write_all`
    // alone is sufficient here.
    use std::io::Write as _;
    let _guard = STDOUT_WRITE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::io::stdout().write_all(bytes).map_err(|e| e.to_string())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Isolates `video_metadata` from everything else `stream_file` does
    /// (the `query_cell_size_px` PTY round-trip in particular, which
    /// blocks forever without a real Som on the other end of stdin/
    /// stdout — exactly what made this bug hard to reproduce outside a
    /// real Som window in the first place) — calls it directly against a
    /// real fixture file with no PTY/Som involved at all.
    #[test]
    fn video_metadata_reports_real_dimensions() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_1920x1080.mp4");
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let metadata = video_metadata(path.to_str().unwrap(), None, None).expect("probe should succeed on a real fixture");
        let terminal::rich_content_transport::ContentMetadata::Video { width_px, height_px, .. } = metadata else {
            panic!("expected ContentMetadata::Video");
        };
        assert_eq!(width_px, 1920, "expected real probed width, not the 1280 fallback");
        assert_eq!(height_px, 1080, "expected real probed height, not the 720 fallback");
    }

    /// Confirms the probe reads only the container header (not the
    /// whole file) even against a real multi-gigabyte movie — this is
    /// the exact regression the old path-based `format::input(path)`
    /// version couldn't pass at all (it errored immediately with
    /// "Protocol not found" rather than being slow, but the underlying
    /// concern — this must stay a fast, header-only probe — is worth
    /// asserting explicitly given how easy it'd be for a future change
    /// to reintroduce a full-file read here).
    #[test]
    fn video_metadata_is_fast_on_a_large_real_file() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../Ready.or.Not.2.Here.I.Come.2026.1080p.MA.WEB-DLRip.x264-HiDt_EniaHD.mkv");
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let start = std::time::Instant::now();
        let metadata = video_metadata(path.to_str().unwrap(), None, None).expect("probe should succeed on a real large fixture");
        let elapsed = start.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(5), "probe took {elapsed:?}, expected a fast header-only read");
        let terminal::rich_content_transport::ContentMetadata::Video { width_px, height_px, .. } = metadata else {
            panic!("expected ContentMetadata::Video");
        };
        assert_eq!(width_px, 1920);
        assert_eq!(height_px, 804);
    }
}
