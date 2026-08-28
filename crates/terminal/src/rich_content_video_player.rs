//! Decode + playback state for Som's own rich-content protocol's video
//! files (`.mp4`/`.mkv`/`.avi`) — the video counterpart to
//! [`crate::rich_content_audio_player`] and [`crate::rich_content_player`].
//!
//! Includes the embedded audio track, if the container has one and its
//! codec is one of the four this build's trimmed FFmpeg decodes
//! (aac/mp3/opus/flac — see `vcpkg-overlays/ffmpeg/portfile.cmake`'s own
//! `--enable-decoder` list) — decoded on the SAME background thread and
//! the SAME demux packet stream as the picture (no second `ictx`, no
//! second thread: `ictx.packets()` already interleaves both stream
//! types, this module just stopped discarding the audio ones). Video's
//! own PTS-vs-wall-clock pacing throttle (see below) already paces how
//! fast this one thread consumes packets, so audio inherits that same
//! pacing for free — video stays the de facto master clock, no separate
//! audio clock to keep in sync. Missing/unsupported audio, or no output
//! device, all fall back to picture-only playback rather than failing
//! the whole player — sound is an enhancement here, not a requirement.
//!
//! Decoding is progressive, driven by a background thread reading
//! `ffmpeg-next` packets/frames from the SAME on-disk cache file
//! [`crate::rich_content_cache::RichContentCache`] is still writing into,
//! same shape as [`crate::rich_content_audio_player::run_decode_loop`].
//! Unlike audio's growing PCM buffer, a video player only ever needs "the
//! most recently decoded frame" for paint — so the shared state is a
//! single latest-frame slot, not an accumulating buffer. The embedded
//! audio track's own buffer ([`SharedAudio`]) is instead a BOUNDED
//! ring — not `crate::rich_content_audio_player::SharedPcm`'s permanent
//! ever-growing one — because unlike a standalone audio decoder (which
//! never seeks), this audio track must be flushed and cleared in
//! lockstep with every video seek; see [`SharedAudio`]'s own doc comment
//! for why the permanent-buffer model doesn't fit here.
//!
//! Decoding reads through a CUSTOM `AVIOContext`
//! ([`GrowingFileStream`]/`ffmpeg_next::format::context::StreamIo`), not
//! plain `ffmpeg::format::input(path)` (file-path open). This matters
//! for a growing file: a plain path-based open has no persistent
//! read-across-calls story — running out of currently-written bytes
//! means the ENTIRE container has to be reopened and reprobed from byte
//! 0 to continue, and reprobing a container format (EBML headers for
//! MKV, moov/ftyp boxes for MP4) against however many bytes happen to be
//! written at that exact moment is unreliable: confirmed live as
//! decoding silently stalling partway through a real, several-minutes
//! long movie clip once chunk sizes got realistic (64KB, matching
//! `somcat`'s own APC chunk size) rather than the handful of large
//! writes an earlier synthetic test used. `GrowingFileStream` instead
//! implements `Read`/`Seek` directly against the SAME on-disk cache file
//! `RichContentCache` is still writing into: a read past the currently
//! written prefix BLOCKS (short sleep-retry loop) until more bytes
//! arrive, rather than returning a premature EOF that would force a
//! reopen — FFmpeg's format probe therefore runs exactly ONCE, against
//! whatever the file eventually contains, the same as it would for a
//! file that was already complete when decoding started.
//!
//! Playback position advances by wall-clock elapsed time compared against
//! each decoded frame's PTS (converted to real time via the video
//! stream's `time_base`) — the decode thread's only job is "keep
//! producing frames as fast as it can," matching PTS to wall-clock at
//! paint time is the paint path's job, same division of labor
//! `rich_content_player::RichContentPlayer::current_frame` already uses
//! for GIF (elapsed-vs-per-frame-delay there, elapsed-vs-PTS here).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use gpui::RenderImage;
use image::Frame;
use smallvec::SmallVec;

/// Decoded (and, for the dark variant, color-inverted) once, on first
/// use, and reused for every stopped video placement from then on — same
/// one-decode-many-reuses shape as `RichContentVideoPlayer::
/// current_frame`'s own `last_rendered` cache, just keyed by which
/// variant is needed instead of nothing, since there are exactly two
/// such images for the whole process. `is_light` should be true when the
/// FIXED color the image is about to be painted against (the letterbox
/// fill — see `RichContentVideoPlayer::stop`'s own doc comment) is
/// itself light — NOT the overall active theme's polarity, which can
/// disagree with that one specific painted color. Falls back to `None`
/// (nothing painted, terminal background shows through) if the embedded
/// asset is somehow missing or not a decodable image — never a reason to
/// panic over a purely cosmetic stand-in frame.
fn stopped_placeholder_frame(is_light: bool) -> Option<image::RgbaImage> {
    static DARK: std::sync::OnceLock<Option<image::RgbaImage>> = std::sync::OnceLock::new();
    static LIGHT: std::sync::OnceLock<Option<image::RgbaImage>> = std::sync::OnceLock::new();
    // The embedded asset itself is black DNA on a light background — the
    // right image to show against a DARK letterbox fill needs inverting
    // (black↔white) so it stays legible instead of nearly vanishing into
    // a background of a similar color; alpha is left untouched since
    // it's transparency, not part of the drawn artwork's own color
    // scheme.
    let cell = if is_light { &LIGHT } else { &DARK };
    cell.get_or_init(|| {
        let bytes = assets::Assets::get("images/dna.png")?;
        let mut image = image::load_from_memory(&bytes.data).ok()?.to_rgba8();
        // The source asset isn't just the DNA glyph on a transparent
        // background — it also carries a faint diagonal watermark hatch
        // pattern at very low alpha (confirmed via direct pixel
        // inspection: roughly 30% of pixels sit at alpha 1-19 out of
        // 255, dark RGB ~(45,45,45)). That's invisible against a light
        // letterbox fill (dark-on-light at near-zero opacity vanishes),
        // but the SAME low-alpha pixels, after color inversion for a
        // dark letterbox fill, become light-on-dark — clearly visible as
        // stray diagonal stripes, confirmed live. Dropping every pixel
        // below this threshold keeps only the glyph itself (whose
        // alpha is much higher, confirmed the same way) regardless of
        // which fill color it's shown against.
        const HATCH_ALPHA_CUTOFF: u8 = 30;
        for pixel in image.pixels_mut() {
            if pixel[3] < HATCH_ALPHA_CUTOFF {
                pixel[3] = 0;
            }
        }
        if !is_light {
            for pixel in image.pixels_mut() {
                pixel[0] = 255 - pixel[0];
                pixel[1] = 255 - pixel[1];
                pixel[2] = 255 - pixel[2];
            }
        }
        Some(image)
    })
    .clone()
}

/// Interleaved f32 PCM the decode thread pushes into and the `cpal`
/// output callback drains from — bounded (see [`AUDIO_BUFFER_MAX_FRAMES`]
/// below), unlike [`crate::rich_content_audio_player::SharedPcm`]'s
/// permanent ever-growing `Vec<f32>`. That permanent-buffer model fits a
/// STANDALONE audio decoder, which never seeks (see this module's own
/// top-level architecture note on why an embedded video's audio track
/// can't reuse it as-is): every video seek flushes and clears this
/// buffer instead (see the seek-handling block in `run_decode_loop`),
/// so keeping only a modest amount of already-decoded-but-not-yet-played
/// audio around is both correct (stale pre-seek samples must never
/// survive a seek) and bounded (a multi-hour movie must not accumulate
/// unbounded PCM the way the pre-fix GPU-texture leak earlier this
/// session accumulated frames).
struct SharedAudio {
    samples: Mutex<VecDeque<f32>>,
    sample_rate: AtomicU64,
    channels: AtomicU64,
}

/// How many audio FRAMES (one sample per channel counts as one frame) to
/// keep buffered ahead of playback — chosen as a few seconds' worth at
/// typical sample rates: enough to absorb ordinary scheduling jitter
/// between the decode thread and the `cpal` callback without either
/// side blocking, small enough that memory use stays bounded regardless
/// of how long the video is. The decode thread stops pushing once the
/// buffer reaches this size (checked before each frame's worth of
/// samples is pushed) and naturally catches up once the `cpal` callback
/// drains it during normal playback.
const AUDIO_BUFFER_MAX_FRAMES: usize = 4 * 48_000;

/// How much of an in-progress SRP video transfer is available right now —
/// same shape and reasoning as
/// [`crate::rich_content_audio_player::AudioTransferProgress`]: `Terminal`
/// is the sole writer, the decode thread the sole reader, kept as
/// independent state rather than a shared reference because the decode
/// thread can't safely hold one into `Terminal`'s own `RichContentCache`.
#[derive(Default)]
pub struct VideoTransferProgress {
    contiguous_len: AtomicU64,
    total_size: AtomicU64,
}

impl VideoTransferProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&self, contiguous_len: u64, total_size: u64) {
        self.contiguous_len.store(contiguous_len, Ordering::Release);
        self.total_size.store(total_size, Ordering::Release);
    }

    fn contiguous_len(&self) -> u64 {
        self.contiguous_len.load(Ordering::Acquire)
    }

    pub fn total_size(&self) -> u64 {
        self.total_size.load(Ordering::Acquire)
    }
}

/// How long the background decode thread sleeps between retries — same
/// value and reasoning as
/// [`crate::rich_content_audio_player::DECODE_RETRY_INTERVAL`].
const DECODE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// The most recently decoded frame, already scaled to BGRA (what
/// `gpui::RenderImage` requires) — `Mutex`-guarded since the decode
/// thread writes it and the paint path reads it from a different thread
/// context (`&self` access, same reasoning `RichContentAudioPlayer`'s
/// fields document). Only ever holds ONE frame: paint only ever needs
/// "the current picture," unlike audio's full played-so-far buffer
/// (needed there for seek math) — see this module's own doc comment for
/// why that rules out a growing/ring buffer here.
struct LatestFrame {
    /// `None` until the decode thread produces its first frame.
    slot: Mutex<Option<(image::RgbaImage, i64)>>,
}

/// A `Read + Seek` view over the SAME on-disk cache file
/// [`crate::rich_content_cache::RichContentCache`] is still writing into
/// — see this module's own top-level doc comment for why this exists
/// instead of `ffmpeg::format::input(path)`'s plain file-path open.
///
/// `read` blocks (sleep-retry) when the requested range extends past
/// [`VideoTransferProgress::contiguous_len`] — the file's real end is
/// [`VideoTransferProgress::total_size`], not the current on-disk
/// length, and only returns `Ok(0)` (real EOF) once `contiguous_len`
/// has actually reached `total_size`. `seek`'s `SeekFrom::End` and
/// `AVSEEK_SIZE` probing (handled by `StreamIo`'s own `seek` callback
/// via `Seek::stream_position`+`SeekFrom::End(0)`) both need `Seek`'s
/// own notion of "the end" to agree with that same `total_size`, not
/// the file's current physical length — otherwise FFmpeg's index/moov
/// scan (which seeks near the presumed end of the file) would either
/// undershoot on a still-growing file or overshoot past what a
/// completed one's own `stream_len` reports, both of which produce a
/// `seek` past valid bounds. This struct's `Seek` impl therefore treats
/// the file as if it were already `total_size` bytes long from the very
/// first byte, with everything past `contiguous_len` simply not
/// readable yet — reads into that region block exactly like a live
/// network read past its currently-buffered prefix would.
struct GrowingFileStream {
    file: std::fs::File,
    progress: Arc<VideoTransferProgress>,
    stop: Arc<AtomicBool>,
    position: u64,
}

impl GrowingFileStream {
    fn open(path: &std::path::Path, progress: Arc<VideoTransferProgress>, stop: Arc<AtomicBool>) -> std::io::Result<Self> {
        Ok(Self { file: std::fs::File::open(path)?, progress, stop, position: 0 })
    }
}

impl std::io::Read for GrowingFileStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Seek, SeekFrom};
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "video player dropped"));
            }
            let contiguous_len = self.progress.contiguous_len();
            if self.position < contiguous_len {
                let readable = (contiguous_len - self.position).min(buf.len() as u64) as usize;
                self.file.seek(SeekFrom::Start(self.position))?;
                let n = self.file.read(&mut buf[..readable])?;
                self.position += n as u64;
                return Ok(n);
            }
            let total_size = self.progress.total_size();
            if total_size > 0 && self.position >= total_size {
                return Ok(0); // Real EOF — the whole file has arrived and we've read all of it.
            }
            // Caught up to what's currently on disk, but more is still
            // coming — block (StreamIo's own doc comment requires a
            // blocking stream; FFmpeg has no retry layer of its own for
            // custom I/O) until RichContentCache::apply_chunk advances
            // contiguous_len further.
            std::thread::sleep(DECODE_RETRY_INTERVAL);
        }
    }
}

impl std::io::Seek for GrowingFileStream {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        // `total_size` is 0 until the SENDER's very first chunk declares
        // it (see `VideoTransferProgress`'s own doc comment) — a
        // `SeekFrom::End` request landing before that has happened
        // must NOT resolve against 0, or it silently means "seek to
        // position 0" (the start), which every caller of `SeekFrom::
        // End(0)` (FFmpeg's own `avio_size`/`AVSEEK_SIZE` probe chief
        // among them) uses specifically to mean "tell me the end" and
        // would then misread as "the file is empty." Block until a
        // real total size is known, same blocking-stream contract
        // `read`'s own doc comment already requires.
        let total_size = loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "video player dropped"));
            }
            let total_size = self.progress.total_size();
            if total_size > 0 || !matches!(pos, SeekFrom::End(_)) {
                break total_size;
            }
            std::thread::sleep(DECODE_RETRY_INTERVAL);
        };
        let new_position = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => total_size as i64 + offset,
            SeekFrom::Current(offset) => self.position as i64 + offset,
        };
        if new_position < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek to a negative position"));
        }
        self.position = new_position as u64;
        Ok(self.position)
    }
}

/// Runs on a dedicated background thread for the lifetime of one
/// [`RichContentVideoPlayer`] — mirrors
/// [`crate::rich_content_audio_player::run_decode_loop`]'s shape closely,
/// but reads through [`GrowingFileStream`] (a custom `AVIOContext`, see
/// this module's own top-level doc comment) instead of a plain
/// path-based open, so the container is probed and opened exactly ONCE
/// regardless of how much of the file has arrived yet.
#[allow(clippy::too_many_arguments)]
fn run_decode_loop(
    path: PathBuf,
    shared: Arc<LatestFrame>,
    progress: Arc<VideoTransferProgress>,
    stop: Arc<AtomicBool>,
    time_base: Arc<(AtomicI64, AtomicI64)>,
    duration_us: Arc<AtomicI64>,
    seek_request: Arc<Mutex<Option<f32>>>,
    playing: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    shared_audio: Arc<SharedAudio>,
) {
    use ffmpeg_next as ffmpeg;

    let _ = ffmpeg::init();

    // Wait for a real, openable file to exist before even trying to
    // build the `GrowingFileStream` — `std::fs::File::open` itself needs
    // the path to exist, which it may not yet on the very first chunk's
    // arrival (`RichContentCache::apply_chunk` creates the file on its
    // first call, but there's an unavoidable gap between "the id is
    // known" and "the file is on disk").
    while !path.is_file() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(DECODE_RETRY_INTERVAL);
    }

    let Ok(stream) = GrowingFileStream::open(&path, progress.clone(), stop.clone()) else {
        return;
    };
    let Ok(stream_io) = ffmpeg::format::context::StreamIo::from_read_seek(stream) else {
        return;
    };
    // `filename` is passed through to ffmpeg's own probe purely as an
    // extension hint (nudges format detection, never itself opens
    // anything) — real bytes always come through `stream_io`.
    //
    // No retry loop here: `GrowingFileStream::read` (the only source of
    // bytes `input_from_stream`'s own probe can pull from) already
    // blocks internally until either more bytes arrive or the whole
    // transfer completes — a single call to `input_from_stream`
    // therefore waits exactly as long as necessary on its own. Reaching
    // `Err` here means probing genuinely failed against the COMPLETE
    // file (real EOF was reached without ever finding a valid
    // container), not "not enough bytes yet" — `StreamIo` also isn't
    // `Clone`, so there is no stream left to retry with even if that
    // distinction mattered here.
    let filename = path.file_name().and_then(|n| n.to_str());
    let Ok(mut ictx) = ffmpeg::format::input_from_stream(stream_io, filename, None) else {
        return;
    };

    // The container's own overall duration, in AV_TIME_BASE (microsecond)
    // units — usually resolvable from the header alone (`moov`/segment
    // info for MP4/MKV), so this is typically known well before decoding
    // reaches anywhere near the actual end, unlike a running elapsed-time
    // count. `<= 0` means "genuinely unknown" (a live/unseekable source,
    // or a container whose duration only becomes knowable once the whole
    // file has streamed) — the paint path treats that the same as audio's
    // own `duration_ms == 0` convention: show elapsed time, no total, no
    // seek-bar fill fraction to compute.
    let raw_duration = ictx.duration();
    if raw_duration > 0 {
        duration_us.store(raw_duration, Ordering::Release);
    }

    let Some(input) = ictx.streams().best(ffmpeg::media::Type::Video) else {
        return; // No video track at all — never will be one.
    };
    let video_stream_index = input.index();
    let stream_time_base = input.time_base();
    time_base.0.store(stream_time_base.numerator() as i64, Ordering::Release);
    time_base.1.store(stream_time_base.denominator() as i64, Ordering::Release);

    let Ok(context_decoder) = ffmpeg::codec::context::Context::from_parameters(input.parameters()) else {
        return;
    };
    let Ok(mut decoder) = context_decoder.decoder().video() else {
        return;
    };

    // Audio track — genuinely optional, unlike the video one above: a
    // video with no embedded sound (`ictx.streams().best(Audio)` returns
    // `None`), an audio codec not in the four this build supports
    // (aac/mp3/opus/flac — see `vcpkg-overlays/ffmpeg/portfile.cmake`'s
    // own `--enable-decoder` list), or no default output audio device on
    // this machine all fall back to today's picture-only behavior rather
    // than aborting the whole player — sound is an enhancement here, not
    // a requirement for video playback to work at all.
    let audio_stream_index = ictx.streams().best(ffmpeg::media::Type::Audio).map(|input| input.index());
    let mut audio_decoder = audio_stream_index.and_then(|index| {
        let input = ictx.stream(index)?;
        let context_decoder = match ffmpeg::codec::context::Context::from_parameters(input.parameters()) {
            Ok(context_decoder) => context_decoder,
            Err(e) => {
                log::warn!("video's audio decoder context open failed: {e}");
                return None;
            },
        };
        match context_decoder.decoder().audio() {
            Ok(decoder) => Some(decoder),
            Err(e) => {
                log::warn!("video's audio decoder open failed (unsupported codec?): {e}");
                None
            },
        }
    });
    // `cpal`'s output stream needs a concrete sample rate/channel count
    // up front — resolvable from the decoder's own codec parameters
    // immediately after opening it (unlike the video decoder's width/
    // height, which genuinely can be unresolved until the first frame —
    // see the `scaler` comment below), so the `cpal` stream can be built
    // right here rather than waiting for the first decoded audio frame.
    let audio_output = audio_decoder.as_ref().and_then(|decoder| {
        let channels = decoder.channels();
        let rate = decoder.rate();
        if channels == 0 || rate == 0 {
            log::warn!("video's audio decoder has channels=0 or rate=0 — codec params unresolved, skipping audio");
            return None;
        }
        shared_audio.sample_rate.store(rate as u64, Ordering::Release);
        shared_audio.channels.store(channels as u64, Ordering::Release);
        let host = cpal::default_host();
        let Some(device) = host.default_output_device() else {
            log::warn!("video's audio: no default output audio device");
            return None;
        };
        let config = cpal::StreamConfig { channels, sample_rate: rate, buffer_size: cpal::BufferSize::Default };
        let cb_shared = shared_audio.clone();
        let cb_playing = playing.clone();
        let stream = match device.build_output_stream(
            &config,
            move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                if !cb_playing.load(Ordering::Acquire) {
                    output.fill(0.0);
                    return;
                }
                let mut samples = cb_shared.samples.lock().unwrap_or_else(|p| p.into_inner());
                let available = samples.len().min(output.len());
                for (dst, src) in output[..available].iter_mut().zip(samples.drain(..available)) {
                    *dst = src;
                }
                output[available..].fill(0.0);
            },
            |err| log::error!("video's audio cpal output stream error: {err}"),
            None,
        ) {
            Ok(stream) => stream,
            Err(e) => {
                log::warn!("video's audio build_output_stream failed: {e}");
                return None;
            },
        };
        if let Err(e) = stream.play() {
            log::warn!("video's audio stream.play() failed: {e}");
            return None;
        }
        Some(stream)
    });
    // Not read past this point — kept alive only so dropping the decode
    // loop's local scope (thread exit) tears the device stream down; see
    // `RichContentAudioPlayer::_stream`'s identical reasoning for why a
    // `cpal::Stream` handle itself is never otherwise touched.
    let _audio_output_stream = audio_output;
    if audio_decoder.is_none() {
        // No usable audio track — never write into a buffer nothing will
        // ever drain, and skip decoding audio packets below entirely.
        shared_audio.sample_rate.store(0, Ordering::Release);
    }
    let mut resampler: Option<ffmpeg::software::resampling::context::Context> = None;
    let mut decoded_audio = ffmpeg::util::frame::audio::Audio::empty();
    let mut resampled_audio = ffmpeg::util::frame::audio::Audio::empty();

    // Built lazily on the first successfully decoded frame, NOT right
    // after opening the decoder — `decoder.format()`/`width()`/`height()`
    // can be unset (`Pixel::None`/`0`) at this point if the container's
    // codec parameters weren't fully resolvable yet (e.g. "unspecified
    // pixel format" from a still-arriving/truncated file), and building
    // an `sws` scaling context with those invalid values crashes inside
    // FFmpeg's own C code (`swscale_internal.h` assertion / stack buffer
    // overrun) rather than surfacing as an `Err` — confirmed live. A
    // decoded `Video` frame's own `format()`/`width()`/`height()` are
    // always valid once `receive_frame` succeeds, unlike the decoder
    // context's.
    let mut scaler: Option<ffmpeg::software::scaling::context::Context> = None;

    let mut decoded = ffmpeg::util::frame::video::Video::empty();
    let mut scaled = ffmpeg::util::frame::video::Video::empty();

    // Real decode throughput is FAR faster than real playback speed (a
    // whole 28-second 1080p clip decodes in ~1.5 wall-clock seconds on
    // ordinary hardware, confirmed live) — without pacing this loop,
    // every frame of the entire file gets written into `shared.slot` one
    // after another almost instantly, and since `slot` only ever holds
    // ONE frame (see this module's own doc comment for why that's
    // deliberate, not a bug), only the LAST one written survives: the
    // decode thread reaches EOF and stops long before `current_frame`'s
    // own PTS-vs-wall-clock gating (in `RichContentVideoPlayer::
    // current_frame`) would ever let an earlier frame through. The
    // reported symptom this fixes: a black screen for the video's WHOLE
    // duration, then a single frozen frame (the last one decoded) forever
    // — `current_frame`'s gate has nothing else left to eventually catch
    // up to once decoding has already finished. Throttling here instead
    // — sleeping before each `slot` write until wall-clock time has
    // actually caught up to that frame's own PTS — keeps `slot` holding
    // whatever frame SHOULD be on screen right now, matching what
    // `current_frame`'s gate expects to find waiting for it.
    // `(wall-clock instant, PTS)` anchor pacing is measured from — reset
    // on the first frame decoded AND on every seek (see the seek handling
    // below), so throttling always paces relative to whatever point
    // decoding most recently (re)started from, not the original file
    // start. `None` means "no anchor yet, the very next frame becomes
    // one" (handles both the initial start and the frame right after a
    // seek uniformly).
    let mut pace_anchor: Option<(Instant, i64)> = None;

    let mut receive_and_store = |decoder: &mut ffmpeg::decoder::Video, scaler: &mut Option<ffmpeg::software::scaling::context::Context>, pace_anchor: &mut Option<(Instant, i64)>| {
        while decoder.receive_frame(&mut decoded).is_ok() {
            if decoded.format() == ffmpeg::format::Pixel::None || decoded.width() == 0 || decoded.height() == 0 {
                continue;
            }
            let pts = decoded.pts().or_else(|| decoded.timestamp()).unwrap_or(0);
            let (anchor_at, anchor_pts) = *pace_anchor.get_or_insert((Instant::now(), pts));
            let num = stream_time_base.numerator() as f64;
            let den = stream_time_base.denominator() as f64;
            if den > 0.0 {
                let frame_time_from_start = (pts - anchor_pts) as f64 * num / den;
                // Slept in short pieces (not one long sleep for the whole
                // gap) so a widget closed mid-wait — `stop` flipped to
                // `true` from another thread — is noticed within one
                // `DECODE_RETRY_INTERVAL` instead of blocking this
                // thread's exit for however long was left to wait. Also
                // bails out (without sleeping further) the moment a seek
                // request lands — the outer loop's own seek handling
                // needs to run promptly, not after this frame's full
                // pacing delay has elapsed for a frame about to be
                // discarded by the seek anyway.
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if seek_request.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
                        return;
                    }
                    let elapsed = anchor_at.elapsed().as_secs_f64();
                    if frame_time_from_start <= elapsed {
                        break;
                    }
                    std::thread::sleep(DECODE_RETRY_INTERVAL.min(Duration::from_secs_f64(frame_time_from_start - elapsed)));
                }
            }
            let scaler = match scaler {
                Some(scaler) => scaler,
                None => {
                    let Ok(built) = ffmpeg::software::scaling::context::Context::get(
                        decoded.format(),
                        decoded.width(),
                        decoded.height(),
                        ffmpeg::format::Pixel::RGBA,
                        decoded.width(),
                        decoded.height(),
                        ffmpeg::software::scaling::flag::Flags::BILINEAR,
                    ) else {
                        continue;
                    };
                    scaler.insert(built)
                },
            };
            if scaler.run(&decoded, &mut scaled).is_err() {
                continue;
            }
            let width = scaled.width();
            let height = scaled.height();
            let stride = scaled.stride(0);
            let src = scaled.data(0);
            let mut buf = Vec::with_capacity(width as usize * height as usize * 4);
            for row in 0..height as usize {
                let start = row * stride;
                buf.extend_from_slice(&src[start..start + width as usize * 4]);
            }
            let Some(rgba) = image::RgbaImage::from_raw(width, height, buf) else {
                continue;
            };
            *shared.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some((rgba, pts));
        }
    };

    // Mirrors `receive_and_store`'s shape for the audio side — no pacing
    // logic of its own (unlike the video half): audio packets interleave
    // with video packets in the SAME demux stream this one thread reads
    // sequentially, so video's own PTS-vs-wall-clock throttle above
    // already paces how fast this whole loop (and therefore audio
    // decoding too) advances — see this function's own doc comment on
    // why video stays the de facto master clock. This closure's only job
    // is decode + resample + push into the bounded buffer, stopping once
    // that buffer is full (the `cpal` callback draining it during normal
    // playback is what makes room for more).
    let mut receive_and_store_audio = |decoder: &mut ffmpeg::decoder::Audio,
                                        resampler: &mut Option<ffmpeg::software::resampling::context::Context>| {
        while decoder.receive_frame(&mut decoded_audio).is_ok() {
            if decoded_audio.format() == ffmpeg::format::Sample::None || decoded_audio.samples() == 0 {
                continue;
            }
            if shared_audio.samples.lock().unwrap_or_else(|p| p.into_inner()).len() >= AUDIO_BUFFER_MAX_FRAMES {
                continue; // Buffer's full — drop this frame rather than grow unbounded; playback will catch up.
            }
            let channels = decoder.channels();
            if channels == 0 {
                continue;
            }
            let target_layout = ffmpeg::ChannelLayout::default(channels as i32);
            let resampler = match resampler {
                Some(resampler) => resampler,
                None => {
                    let Ok(built) = ffmpeg::software::resampling::context::Context::get(
                        decoded_audio.format(),
                        decoded_audio.channel_layout(),
                        decoded_audio.rate(),
                        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
                        target_layout,
                        decoded_audio.rate(),
                    ) else {
                        continue;
                    };
                    resampler.insert(built)
                },
            };
            if resampler.run(&decoded_audio, &mut resampled_audio).is_err() {
                continue;
            }
            let samples: &[f32] = resampled_audio.plane(0);
            shared_audio.samples.lock().unwrap_or_else(|p| p.into_inner()).extend(samples.iter().copied());
        }
    };

    // No reopen/seek loop needed for the "ran out of currently-written
    // bytes" case here (an earlier version of this function had one) —
    // `ictx.packets().next()` returning `None` while more of the file is
    // still arriving simply cannot happen: `GrowingFileStream::read`, the
    // sole byte source behind `ictx`, blocks internally until either more
    // bytes land or the real end of the transfer is reached, so
    // `packets()`'s iterator only ever yields `None` for a genuine end of
    // stream. A REAL seek request (user-driven, via `seek_request`) is
    // handled explicitly below instead.
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }

        // Paused (and a frame already exists to show): don't decode any
        // further ahead at all — earlier this loop only throttled the
        // pace at which decoded frames reached `slot`, which still let
        // decoding (and thus `slot`'s contents) keep marching forward
        // while paused, just more slowly; visually indistinguishable
        // from playback continuing, confirmed live. A real pause needs
        // to stop consuming packets entirely, not just slow down. The
        // `slot` check keeps the player's OWN "starts paused" contract
        // working (see `RichContentVideoPlayer::open`'s doc comment) —
        // the very first frame must still decode immediately even before
        // any `toggle_play_pause()` call, matching every other paused-
        // by-default player in this codebase (audio, GIF) already
        // showing a first frame/thumbnail without a play click. Woken by
        // either `stop` or a seek request arriving mid-pause (checked
        // every `DECODE_RETRY_INTERVAL`) — a seek while paused must
        // still take effect immediately, and `RichContentVideoPlayer::
        // current_frame`'s own paused branch already shows whatever
        // `slot` holds with no PTS gating, so the seeked-to frame
        // becomes visible as soon as it decodes.
        let has_a_frame_already = shared.slot.lock().unwrap_or_else(|p| p.into_inner()).is_some();
        if has_a_frame_already && !playing.load(Ordering::Acquire) {
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                if playing.load(Ordering::Acquire) || seek_request.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
                    break;
                }
                std::thread::sleep(DECODE_RETRY_INTERVAL);
            }
            // Resuming from a pause (or handling a seek that arrived
            // while paused) needs a fresh pacing anchor — the old one's
            // wall-clock half stopped advancing meaningfully while
            // paused/blocked, so reusing it would make the next frame's
            // pacing math see a huge "elapsed" gap and rush ahead.
            // `None` here makes the next decoded frame become the new
            // anchor, same as after a seek. Only reset on the pause ->
            // resume transition, NOT on every outer-loop iteration
            // (which runs once per packet during normal playback) — an
            // unconditional reset here would defeat pacing entirely.
            pace_anchor = None;
        }

        // A pending seek request takes priority over continuing the
        // current sequential read — `take()` (not just `peek`) so a
        // later seek to the SAME fraction while this one is still being
        // handled doesn't get silently coalesced away.
        if let Some(fraction) = seek_request.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let duration = duration_us.load(Ordering::Acquire);
            if duration > 0 {
                // FFmpeg's own seek API works in AV_TIME_BASE (microsecond)
                // units regardless of the stream's own `time_base` — see
                // `ffmpeg_next::format::context::Input::seek`'s doc
                // comment — so the target here is plain
                // `fraction * duration_us`, converted to the stream's PTS
                // units only afterward (via `stream_time_base`) once a
                // frame actually arrives post-seek.
                let target_us = (duration as f64 * fraction.clamp(0.0, 1.0) as f64) as i64;

                // If the byte range this seek lands in hasn't arrived
                // yet, `RichContentVideoPlayer::seek_to_fraction` (the
                // only place that ever populates `seek_request`) already
                // fired its own `SrvRequest::RequestByteRange` for an
                // estimated offset before this decode thread ever sees
                // the request — see that method's own doc comment for
                // why the estimate doesn't need to be exact.
                // `GrowingFileStream::read`'s blocking sleep-retry (this
                // module's top-level doc comment) makes correct progress
                // regardless once those bytes land; this `seek()` call
                // itself never blocks on the network, only on whatever's
                // already on disk right now.
                let _ = ictx.seek(target_us, ..target_us);
                decoder.flush();
                pace_anchor = None;
                // Audio must be flushed and cleared in lockstep with the
                // video decoder above — leftover pre-seek samples still
                // sitting in `shared_audio.samples` would otherwise play
                // right after the post-seek video frame appears, an
                // audible desync bug, not just a cosmetic one (see this
                // module's own architecture note on the bounded-buffer
                // model for why embedded audio can't reuse standalone
                // audio's permanent-buffer/never-seeks assumption).
                if let Some(audio_decoder) = audio_decoder.as_mut() {
                    audio_decoder.flush();
                }
                shared_audio.samples.lock().unwrap_or_else(|p| p.into_inner()).clear();
            }
        }

        match ictx.packets().next() {
            Some((stream, packet)) if stream.index() == video_stream_index => {
                if decoder.send_packet(&packet).is_ok() {
                    receive_and_store(&mut decoder, &mut scaler, &mut pace_anchor);
                }
            },
            Some((stream, packet)) if Some(stream.index()) == audio_stream_index => {
                if let Some(audio_decoder) = audio_decoder.as_mut() {
                    if audio_decoder.send_packet(&packet).is_ok() {
                        receive_and_store_audio(audio_decoder, &mut resampler);
                    }
                }
            },
            Some(_) => continue,
            None => {
                let _ = decoder.send_eof();
                receive_and_store(&mut decoder, &mut scaler, &mut pace_anchor);
                if let Some(audio_decoder) = audio_decoder.as_mut() {
                    let _ = audio_decoder.send_eof();
                    receive_and_store_audio(audio_decoder, &mut resampler);
                }
                // Real end of stream — stop advancing wall-clock time.
                // Without this, `elapsed()`'s playing-branch keeps adding
                // `started_at.elapsed()` forever after the last frame,
                // confirmed live as the readout ticking past the video's
                // own duration indefinitely. Setting `playing` false here
                // routes `elapsed()` to its paused branch instead, which
                // freezes on the last decoded frame's own PTS.
                playing.store(false, Ordering::Release);

                // Do NOT `return` here — an earlier version of this
                // branch ended the thread outright, which left "play"
                // (`toggle_play_pause`) and "stop" (seek-to-0) both
                // silently inert after a video finished: `seek_request`
                // would be set by the player-side methods, but nothing
                // was left alive to ever read it, so `elapsed()` kept
                // computing from the stale `playback_started_at` a
                // caller-side `toggle_play_pause()` had just written,
                // growing forever with no new frames ever arriving to
                // reset it — confirmed live as the readout climbing past
                // the real duration after pressing play again post-EOF.
                // `finished` lets `RichContentVideoPlayer::toggle_play_pause`
                // tell this case apart from an ordinary mid-video pause
                // (see that method's own doc comment) so a plain play
                // click after the video ends re-seeks to the start
                // instead of resuming a decoder that has nothing left to
                // decode.
                finished.store(true, Ordering::Release);
                // Block here the same way the pause-gate above does,
                // waiting specifically for a seek (a bare `playing=true`
                // with no seek is exactly the stale-resume case just
                // described, so it does NOT alone justify falling through
                // to `ictx.packets().next()`, which would immediately
                // yield `None` again since the demuxer is still at EOF).
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if seek_request.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
                        break;
                    }
                    std::thread::sleep(DECODE_RETRY_INTERVAL);
                }
                finished.store(false, Ordering::Release);
                decoder.flush();
                continue;
            },
        }
    }
}

/// One rich-content video file's playback state — a background decoder
/// thread feeding a single latest-decoded-frame slot, plus wall-clock
/// based playback advancement (see this module's own doc comment).
/// `position`/`playing` follow the same `Arc<Atomic*>` reasoning
/// `RichContentAudioPlayer`'s fields document (read from the paint path
/// via `&self`, no cross-thread `Cell`).
pub struct RichContentVideoPlayer {
    shared: Arc<LatestFrame>,
    decode_stop: Arc<AtomicBool>,
    /// `(numerator, denominator)` of the video stream's `time_base` —
    /// populated by the decode thread once the container's opened;
    /// `(0, 1)` (an always-zero time) until then.
    time_base: Arc<(AtomicI64, AtomicI64)>,
    playing: Arc<std::sync::atomic::AtomicBool>,
    /// Wall-clock instant playback last (re)started, paired with the PTS
    /// (in stream time-base units) it started from — together these let
    /// [`Self::current_frame`] compute "how far into the video, in PTS
    /// units, has wall-clock time carried us" without the decode thread
    /// needing to track playback position itself (it only ever produces
    /// frames as fast as it can, same division of labor this module's
    /// doc comment describes).
    playback_started_at: Arc<Mutex<Option<(Instant, i64)>>>,
    /// The container's own overall duration in microseconds — see
    /// `run_decode_loop`'s own doc comment on where this comes from.
    /// `0` (the initial value) means "not yet known" (or genuinely
    /// unknowable), same convention `Terminal::rich_content_cache`'s
    /// `audio_metadata`'s `duration_ms == 0` already uses.
    duration_us: Arc<AtomicI64>,
    /// A pending seek target (0.0..=1.0 fraction of the video's total
    /// duration), consumed by the decode thread the next time it checks
    /// (see `run_decode_loop`'s main loop) — `Terminal::mouse_down`'s
    /// click-interception check (via `Self::seek_to_fraction`) is the
    /// only writer, the decode thread the only reader/clearer.
    seek_request: Arc<Mutex<Option<f32>>>,
    /// The most recently BUILT `RenderImage`, paired with the PTS it was
    /// built from — `RenderImage::new` mints a fresh globally-unique
    /// `ImageId` on every call (see `gpui::RenderImage::new`'s own
    /// `NEXT_ID` counter), and GPUI's sprite atlas caches GPU textures
    /// keyed on that id (`Window::paint_image`'s `sprite_atlas.
    /// get_or_insert_with`) — nothing ever calls `Window::drop_image` for
    /// an old one, so building a brand new `RenderImage` on every
    /// `current_frame()` call (this used to happen unconditionally)
    /// leaked one full-resolution GPU texture (~8MB for 1080p) per PAINT
    /// CALL, not per decoded frame, since paint happens far more often
    /// than the video's own frame rate. Confirmed live: this took the
    /// whole machine down via OOM within seconds of a video placement
    /// appearing on screen. Reusing the SAME `Arc<RenderImage>` (and
    /// therefore the same GPU texture) across paint calls whenever the
    /// underlying pixel data hasn't actually changed is not an
    /// optimization here — it's the fix for a real, reproduced crash.
    last_rendered: Mutex<Option<(i64, Arc<RenderImage>)>>,
    /// Every `RenderImage` `current_frame()` has replaced in
    /// `last_rendered` — GPUI's sprite atlas caches one GPU texture per
    /// `RenderImage` forever unless explicitly told to release it
    /// (`Window::drop_image`), and `current_frame()` itself has no
    /// `Window` to call that with (it's driven from `Terminal::
    /// rich_content_video_placements`, outside any paint pass). Queued
    /// here instead, for [`Self::take_pending_image_drops`] to drain from
    /// the ONE place in this whole pipeline that DOES have a `Window` —
    /// `paint_rich_content_placements` in `terminal_element.rs`. Without
    /// this, every decoded frame's GPU texture (~8MB for 1080p) leaked
    /// for the entire lifetime of the `Terminal`, confirmed live as
    /// multi-gigabyte RSS growth over an hour of continuous playback.
    pending_image_drops: Mutex<Vec<Arc<RenderImage>>>,
    /// Set by the decode thread while it's blocked at real end-of-stream
    /// waiting for a seek (see `run_decode_loop`'s EOF branch) — lets
    /// [`Self::is_finished`] tell "paused mid-video" apart from "nothing
    /// left to decode without a seek first", so callers (`Terminal::
    /// toggle_rich_content_video_playback`) know a plain play click needs
    /// to seek back to the start rather than just flipping `playing`.
    finished: Arc<AtomicBool>,
    /// Set by [`Self::stop`], cleared by [`Self::toggle_play_pause`] —
    /// makes [`Self::current_frame`] substitute `stopped_placeholder_
    /// frame` for whatever's actually in `shared.slot` (the real last-
    /// played frame is left untouched underneath, not overwritten, so
    /// the decode thread's own state stays consistent) instead of that
    /// real frame. A plain `bool` baked into `shared.slot` at the moment
    /// `stop` was called would freeze in whichever theme was active
    /// then — this flag instead lets the SAME stopped state re-resolve
    /// to whichever theme is active at each individual paint, so
    /// switching `theme.json` while a video sits stopped updates the
    /// stand-in image too.
    stopped: Arc<AtomicBool>,
    /// Embedded audio track's decoded PCM + format — see [`SharedAudio`]'s
    /// own doc comment. The actual `cpal::Stream` for this player's audio
    /// lives INSIDE the decode thread's own local scope (opened once the
    /// audio decoder's format is known, right after the container opens —
    /// see `run_decode_loop`), not on this struct, since a `cpal::Stream`
    /// isn't necessarily `Send` — this field only holds the state that
    /// crosses the thread boundary (the shared PCM buffer + format), same
    /// division `shared`/`LatestFrame` already uses for the picture side.
    /// Not read from outside this struct yet (the decode thread holds its
    /// own clone of the same `Arc` and is the only current reader/writer)
    /// — kept here anyway so a future caller (e.g. "does this video have
    /// audio at all" for UI purposes, via `shared_audio.channels()`) has
    /// somewhere to read it from without threading a new field through
    /// `open()` again.
    #[allow(dead_code)]
    shared_audio: Arc<SharedAudio>,
}

impl Drop for RichContentVideoPlayer {
    fn drop(&mut self) {
        self.decode_stop.store(true, Ordering::Relaxed);
    }
}

impl RichContentVideoPlayer {
    /// Starts decoding `path` (the on-disk cache file for one SRP video
    /// transfer, possibly still growing) on a background thread. Starts
    /// paused, matching audio/GIF's own "don't autoplay" convention.
    pub fn open(path: PathBuf, progress: Arc<VideoTransferProgress>) -> Self {
        let shared = Arc::new(LatestFrame { slot: Mutex::new(None) });
        let decode_stop = Arc::new(AtomicBool::new(false));
        let time_base = Arc::new((AtomicI64::new(0), AtomicI64::new(1)));
        let duration_us = Arc::new(AtomicI64::new(0));
        let seek_request = Arc::new(Mutex::new(None));
        let playing = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let shared_audio = Arc::new(SharedAudio {
            samples: Mutex::new(VecDeque::new()),
            sample_rate: AtomicU64::new(0),
            channels: AtomicU64::new(0),
        });

        {
            let shared = shared.clone();
            let decode_stop = decode_stop.clone();
            let time_base = time_base.clone();
            let duration_us = duration_us.clone();
            let seek_request = seek_request.clone();
            let playing = playing.clone();
            let finished = finished.clone();
            let shared_audio = shared_audio.clone();
            std::thread::spawn(move || {
                run_decode_loop(
                    path,
                    shared,
                    progress,
                    decode_stop,
                    time_base,
                    duration_us,
                    seek_request,
                    playing,
                    finished,
                    shared_audio,
                )
            });
        }

        Self {
            shared,
            decode_stop,
            time_base,
            playing,
            playback_started_at: Arc::new(Mutex::new(None)),
            duration_us,
            seek_request,
            last_rendered: Mutex::new(None),
            pending_image_drops: Mutex::new(Vec::new()),
            finished,
            stopped: Arc::new(AtomicBool::new(false)),
            shared_audio,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// True while the decode thread is blocked at real end-of-stream,
    /// waiting for a seek before it can produce any more frames — see
    /// `run_decode_loop`'s EOF branch and this struct's own `finished`
    /// field doc comment.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// True while [`Self::current_frame`] is returning the fixed
    /// stand-in image instead of a real decoded frame — see [`Self::
    /// stop`]'s own doc comment. Callers need this alongside the image
    /// itself because the stand-in has its OWN aspect ratio (the
    /// `dna.png` asset, not the video's), so it should be letterboxed to
    /// fit rather than stretched to fill the video's reserved footprint
    /// the way a real decoded frame correctly is.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub fn toggle_play_pause(&self) {
        let now_playing = !self.playing.fetch_xor(true, Ordering::AcqRel);
        if now_playing {
            // Clears whatever `Self::stop` set — resuming playback after
            // a stop means the real decoded frame (which the queued seek
            // back to 0.0 is already bringing in) should show again, not
            // the stand-in image.
            self.stopped.store(false, Ordering::Release);
            let current_pts = self.shared.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, pts)| *pts).unwrap_or(0);
            *self.playback_started_at.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), current_pts));
        }
    }

    /// The video's total duration, if known — see `run_decode_loop`'s own
    /// doc comment on `duration_us` for when this becomes available and
    /// what `ZERO` means. Mirrors `RichContentAudioPlayer`'s equivalent
    /// (that one takes `duration_ms` as a caller-supplied argument, since
    /// audio's duration comes from `ContentMetadata::Audio` instead of a
    /// container probe — video has no such upfront metadata, se this
    /// reads its own internally-tracked value instead).
    pub fn duration(&self) -> std::time::Duration {
        let us = self.duration_us.load(Ordering::Acquire);
        if us <= 0 { std::time::Duration::ZERO } else { std::time::Duration::from_micros(us as u64) }
    }

    /// Playback position, in the same units `duration()` reports —
    /// always derived directly from whatever frame's PTS is CURRENTLY in
    /// `shared.slot` (i.e. whatever `current_frame()` is actually
    /// returning to be painted right now), not from a separately tracked
    /// wall-clock counter. An earlier version computed this as "PTS at
    /// playback start + wall-clock time elapsed since", which could only
    /// ever be an ESTIMATE of what the decode thread was doing — and
    /// diverged from the real on-screen frame in more than one way,
    /// confirmed live: the estimate kept climbing indefinitely past the
    /// video's own duration once the decode thread had nothing left to
    /// decode (paused, seeking, or genuinely at end-of-stream) because
    /// nothing was left to stop the wall-clock half from advancing.
    /// Reading `shared.slot`'s PTS directly instead makes this number
    /// physically incapable of disagreeing with the picture on screen —
    /// whatever is currently displayed, playing, paused, or freshly
    /// landed after a seek, IS the position, by construction.
    pub fn elapsed(&self) -> std::time::Duration {
        if self.stopped.load(Ordering::Acquire) {
            return std::time::Duration::ZERO;
        }
        let pts = match self.shared.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            Some((_, pts)) => *pts,
            None => return std::time::Duration::ZERO,
        };
        let num = self.time_base.0.load(Ordering::Acquire).max(0) as f64;
        let den = self.time_base.1.load(Ordering::Acquire).max(1) as f64;
        let frame_time = if den > 0.0 { pts as f64 * num / den } else { 0.0 };
        std::time::Duration::from_secs_f64(frame_time.max(0.0))
    }

    /// Fraction (0.0..=1.0) of `duration()` that `elapsed()` represents —
    /// `0.0` if duration isn't known yet (nothing to divide by).
    pub fn position_fraction(&self) -> f32 {
        let duration = self.duration().as_secs_f64();
        if duration <= 0.0 {
            return 0.0;
        }
        (self.elapsed().as_secs_f64() / duration).clamp(0.0, 1.0) as f32
    }

    /// Requests a seek to `fraction` (0.0..=1.0) of the video's total
    /// duration — picked up by the decode thread on its next loop
    /// iteration (see `run_decode_loop`'s seek handling), which performs
    /// the real `ictx.seek()` + decoder flush. Also fires an immediate
    /// `SrvRequest::RequestByteRange` for the ESTIMATED byte offset this
    /// fraction corresponds to (`fraction * total_size`, from the same
    /// `VideoTransferProgress` the decode thread already reads) — a
    /// linear estimate, not an exact byte-for-byte target (video bitrate
    /// isn't constant), but close enough to prioritize downloading
    /// roughly the right region of a still-in-progress transfer well
    /// before the sequential stream would naturally reach it, matching
    /// the same "seek should feel instant, not wait for natural download
    /// order" requirement audio's own byte-range seek already satisfies.
    /// A no-op (still records the seek request, just skips the network
    /// call) if `total_size` isn't known yet.
    pub fn seek_to_fraction(&self, fraction: f32, progress: &VideoTransferProgress, request_byte_range: impl FnOnce(u64, u64)) {
        *self.seek_request.lock().unwrap_or_else(|p| p.into_inner()) = Some(fraction.clamp(0.0, 1.0));
        let total_size = progress.total_size();
        if total_size > 0 {
            let estimated_offset = (total_size as f64 * fraction.clamp(0.0, 1.0) as f64) as u64;
            // A generous fixed window around the estimate — covers
            // normal bitrate variance without needing to know the real
            // target precisely; `GrowingFileStream`'s own blocking
            // sleep-retry (this module's top-level doc comment) still
            // makes progress even if the true byte offset falls slightly
            // outside this window, just less instantly.
            const SEEK_RANGE_REQUEST_LEN: u64 = 4 * 1024 * 1024;
            let window_start = estimated_offset.saturating_sub(SEEK_RANGE_REQUEST_LEN / 2);
            request_byte_range(window_start, SEEK_RANGE_REQUEST_LEN);
        }
    }

    /// Stops playback and marks this player as showing a fixed stand-in
    /// image instead of the real last-played frame — unlike
    /// [`Self::seek_to_fraction`] alone (which `Terminal::
    /// stop_rich_content_video_playback` used to call directly), this
    /// makes [`Self::elapsed`] read `00:00:00` immediately rather than
    /// continuing to show the frame that was on screen when stop was
    /// pressed — a pressed stop looked visually identical to a plain
    /// pause, confirmed live, because seeking to 0.0 alone just re-queues
    /// a decode-thread seek that doesn't actually land (and overwrite
    /// `shared.slot`) until playback resumes; pausing first left the last
    /// frame's pixels sitting in `shared.slot`/`last_rendered` the whole
    /// time in between. An earlier version of this method cleared
    /// `shared.slot` to `None` instead — abandoned because the
    /// placeholder grid's picture cells paint their OWN background color
    /// independent of whatever `current_frame()` returns (see
    /// `print_placeholder_grid_with_cell_dims` in `somcat`), so `None`
    /// still left a solid-colored rectangle on screen, just not the
    /// terminal's own background — confirmed live as a stark black block
    /// instead of the active theme's real background showing through.
    /// A LATER version wrote a decoded stand-in image directly into
    /// `shared.slot` — also abandoned, because that bakes in whichever
    /// theme was active at the moment `stop` was called, and Som lets the
    /// user switch `theme.json` at any time, including while a video sits
    /// stopped. Setting the `stopped` flag instead defers the actual
    /// image choice to [`Self::current_frame`], which re-resolves it
    /// against the CURRENTLY active theme on every call — `shared.slot`
    /// itself is left untouched, so the decode thread's own state stays
    /// consistent underneath. The queued `seek_request` still makes the
    /// decode thread actually re-seek so the NEXT play starts decoding
    /// from the beginning rather than wherever playback happened to stop.
    pub fn stop(&self, progress: &VideoTransferProgress, request_byte_range: impl FnOnce(u64, u64)) {
        if self.is_playing() {
            self.toggle_play_pause();
        }
        self.stopped.store(true, Ordering::Release);
        self.seek_to_fraction(0.0, progress, request_byte_range);
    }


    /// The current frame to paint, if the decode thread has produced one
    /// yet. Returns the SAME `Arc<RenderImage>` (same `ImageId`, same
    /// cached GPU texture) across repeated calls as long as the
    /// underlying decoded frame hasn't changed — see [`Self::
    /// last_rendered`]'s own doc comment for why this is a correctness
    /// fix, not an optimization: paint happens far more often than the
    /// video's actual frame rate, and building a fresh `RenderImage`
    /// every call leaked one full-resolution GPU texture per PAINT call.
    ///
    /// While playing, this only returns a NEWLY decoded frame once
    /// wall-clock elapsed time has caught up to that frame's own PTS —
    /// mirrors `RichContentPlayer::current_frame`'s identical
    /// elapsed-vs-delay gating for GIF, just measured against PTS instead
    /// of a per-frame delay list. While paused, always returns whatever
    /// the decode thread has produced so far (no PTS gating) — matches
    /// audio's "seeking while paused still updates position" ergonomics.
    ///
    /// `is_light` should reflect whether the FIXED letterbox fill color
    /// the stopped-state stand-in image is about to be painted against is
    /// itself light (passed in fresh on every call, not cached) — see
    /// `stopped_placeholder_frame`'s own doc comment for why this is
    /// deliberately NOT the same thing as the active theme's overall
    /// polarity, and [`Self::stop`]'s own doc comment for why the choice
    /// is resolved here rather than baked in once when stop was pressed.
    pub fn current_frame(&self, is_light: bool) -> Option<Arc<RenderImage>> {
        // A synthetic PTS that can never collide with a real decoded
        // frame's own (`i64`, so `MIN` is never a legitimate stream
        // timestamp) — used as `last_rendered`'s cache key for the
        // stand-in image so switching `theme.json` while stopped (light
        // vs. dark stand-in are different images, same synthetic PTS)
        // still invalidates the cache correctly below, the same way a
        // real new decoded frame's differing PTS would.
        const STOPPED_PTS: i64 = i64::MIN;

        if self.stopped.load(Ordering::Acquire) {
            let rgba = stopped_placeholder_frame(is_light)?;
            let mut cache = self.last_rendered.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((cached_pts, cached_image)) = cache.as_ref() {
                if *cached_pts == STOPPED_PTS {
                    return Some(cached_image.clone());
                }
            }
            let mut bgra = rgba;
            for pixel in bgra.as_flat_samples_mut().samples.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            let frame = Frame::new(bgra);
            let image = Arc::new(RenderImage::new(SmallVec::from_vec(vec![frame])));
            if let Some((_, old_image)) = cache.replace((STOPPED_PTS, image.clone())) {
                self.pending_image_drops.lock().unwrap_or_else(|p| p.into_inner()).push(old_image);
            }
            return Some(image);
        }

        let slot = self.shared.slot.lock().unwrap_or_else(|p| p.into_inner());
        let (rgba, pts) = slot.as_ref()?;
        let pts = *pts;

        if self.is_playing() {
            let started = self.playback_started_at.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((started_at, started_pts)) = *started {
                let num = self.time_base.0.load(Ordering::Acquire).max(0) as f64;
                let den = self.time_base.1.load(Ordering::Acquire).max(1) as f64;
                let frame_time_from_start = if den > 0.0 { (pts - started_pts) as f64 * num / den } else { 0.0 };
                let elapsed = started_at.elapsed().as_secs_f64();
                if frame_time_from_start > elapsed {
                    // This frame is further ahead than wall-clock has
                    // reached yet — nothing new to show this paint. Drop
                    // the `slot` guard implicitly at scope end below;
                    // whatever was last rendered stays cached as-is.
                    drop(started);
                    let cached = self.last_rendered.lock().unwrap_or_else(|p| p.into_inner());
                    return cached.as_ref().map(|(_, image)| image.clone());
                }
            }
        }

        let mut cache = self.last_rendered.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((cached_pts, cached_image)) = cache.as_ref() {
            if *cached_pts == pts {
                return Some(cached_image.clone());
            }
        }

        let mut bgra = rgba.clone();
        for pixel in bgra.as_flat_samples_mut().samples.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let frame = Frame::new(bgra);
        let image = Arc::new(RenderImage::new(SmallVec::from_vec(vec![frame])));
        // The OLD cached image's GPU texture is now unreachable from this
        // player — queue it for `take_pending_image_drops` to release
        // via `Window::drop_image` (see `pending_image_drops`'s own doc
        // comment for why this can't just be dropped here directly).
        if let Some((_, old_image)) = cache.replace((pts, image.clone())) {
            self.pending_image_drops.lock().unwrap_or_else(|p| p.into_inner()).push(old_image);
        }
        Some(image)
    }

    /// Drains every `RenderImage` this player has stopped using since the
    /// last call — see `pending_image_drops`'s own doc comment. Callers
    /// (the paint path, where a `Window` is actually available) must
    /// call `window.drop_image(image, ...)` for each one returned here.
    pub fn take_pending_image_drops(&self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut *self.pending_image_drops.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_fixture_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(name)
    }

    fn open_test_player(fixture: &str) -> Option<RichContentVideoPlayer> {
        let path = test_fixture_path(fixture);
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return None;
        }
        let total_size = std::fs::metadata(&path).unwrap().len();
        let progress = Arc::new(VideoTransferProgress::new());
        progress.update(total_size, total_size);
        Some(RichContentVideoPlayer::open(path, progress))
    }

    fn wait_for_decode(player: &RichContentVideoPlayer) -> bool {
        for _ in 0..150 {
            if player.shared.slot.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn player_starts_paused() {
        let Some(player) = open_test_player("sample_1920x1080.mkv") else { return };
        assert!(!player.is_playing());
    }

    #[test]
    fn decode_thread_populates_a_frame_for_mkv() {
        let Some(player) = open_test_player("sample_1920x1080.mkv") else { return };
        assert!(wait_for_decode(&player), "decode thread never produced a frame for the mkv fixture");
        let frame = player.current_frame(false);
        assert!(frame.is_some(), "current_frame() must return a frame once paused decode has produced one");
    }

    #[test]
    fn decode_thread_populates_a_frame_for_mp4() {
        let Some(player) = open_test_player("sample_1920x1080.mp4") else { return };
        assert!(wait_for_decode(&player), "decode thread never produced a frame for the mp4 fixture");
    }

    #[test]
    fn decode_thread_populates_a_frame_for_avi() {
        let Some(player) = open_test_player("sample_1920x1080.avi") else { return };
        assert!(wait_for_decode(&player), "decode thread never produced a frame for the avi fixture");
    }

    #[test]
    fn toggle_play_pause_flips_state() {
        let Some(player) = open_test_player("sample_1920x1080.mkv") else { return };
        assert!(!player.is_playing());
        player.toggle_play_pause();
        assert!(player.is_playing());
        player.toggle_play_pause();
        assert!(!player.is_playing());
    }

    #[test]
    fn decoded_frame_has_expected_dimensions() {
        let Some(player) = open_test_player("sample_1920x1080.mkv") else { return };
        assert!(wait_for_decode(&player));
        let slot = player.shared.slot.lock().unwrap_or_else(|p| p.into_inner());
        let (rgba, _) = slot.as_ref().expect("decoded above");
        assert_eq!(rgba.width(), 1920);
        assert_eq!(rgba.height(), 1080);
    }

    /// Regression test for the bug [`GrowingFileStream`]'s custom
    /// `AVIOContext` fixes: writes the fixture to a growing file in
    /// small pieces with real delays between them (the same "still
    /// arriving" shape a real SRP transfer has), then confirms the
    /// decode thread's PTS progress actually advances close to the
    /// whole file's worth of content rather than stalling partway
    /// through — an earlier version of this module reopened
    /// `ffmpeg::format::input(path)` from scratch every time it ran out
    /// of currently-written bytes, which this test would have caught as
    /// `last_pts` never climbing much past wherever the first probe
    /// happened to succeed.
    #[test]
    fn progressive_write_reaches_near_the_end_of_the_file() {
        let source_path = test_fixture_path("sample_1920x1080.mkv");
        if !source_path.is_file() {
            eprintln!("skipping: {} not present", source_path.display());
            return;
        }
        let source_bytes = std::fs::read(&source_path).expect("reading fixture");
        let total_size = source_bytes.len() as u64;

        let dest_path = std::env::temp_dir().join("som_video_progressive_write_test.mkv");
        std::fs::write(&dest_path, b"").expect("creating empty dest file");

        let progress = Arc::new(VideoTransferProgress::new());
        let player = RichContentVideoPlayer::open(dest_path.clone(), progress.clone());
        // The decode thread only advances PAST its first frame while
        // actually playing (see `run_decode_loop`'s pause-gate doc
        // comment) — a real pause fix that made this test's own
        // "did decode reach deep into the file" check meaningless
        // without this, since a paused player now deliberately produces
        // only ONE frame and stops there.
        player.toggle_play_pause();

        // Small pieces with real sleeps between them — deliberately
        // smaller than DECODE_RETRY_INTERVAL's own sleep so the decode
        // thread is very likely to run out of currently-available bytes
        // and hit the reopen path multiple times before the whole file
        // has landed.
        let piece_count = 12;
        let piece_len = (source_bytes.len() / piece_count).max(1);
        let mut written = 0usize;
        for _ in 0..piece_count {
            let end = (written + piece_len).min(source_bytes.len());
            std::fs::write(&dest_path, &source_bytes[..end]).expect("appending to dest file");
            written = end;
            progress.update(written as u64, total_size);
            std::thread::sleep(Duration::from_millis(150));
        }
        // Final write covers any remainder from integer division above.
        std::fs::write(&dest_path, &source_bytes).expect("writing final dest file");
        progress.update(total_size, total_size);

        // Give the decode thread a real chance to catch up to the now-
        // complete file — generous budget since this test's own writes
        // already took piece_count * 150ms.
        let mut last_seen_pts = i64::MIN;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(50));
            let slot = player.shared.slot.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((_, pts)) = slot.as_ref() {
                last_seen_pts = last_seen_pts.max(*pts);
            }
        }

        let _ = std::fs::remove_file(&dest_path);

        assert!(last_seen_pts > i64::MIN, "decode thread never produced any frame for the progressively-written file");
        // Without the seek-on-reopen fix, `last_seen_pts` stalled at
        // whatever the very first reopen managed to decode (a small
        // fraction of the file) — a real regression this asserts
        // against by requiring decode to reach a PTS in the LATTER HALF
        // of the stream's own time_base-scaled duration, not just "some
        // nonzero value early in the file."
        let num = player.time_base.0.load(Ordering::Acquire).max(0) as f64;
        let den = player.time_base.1.load(Ordering::Acquire).max(1) as f64;
        assert!(den > 0.0, "time_base must have been populated by the decode thread");
        let last_seen_seconds = last_seen_pts as f64 * num / den;
        assert!(
            last_seen_seconds > 1.0,
            "decode only reached {last_seen_seconds:.2}s into the file — reopen-with-seek likely isn't resuming progress correctly"
        );
    }
}
