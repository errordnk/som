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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use gpui::RenderImage;
use image::Frame;
use smallvec::SmallVec;

/// D3D11VA hardware-accelerated video decode setup — `ffmpeg-next`'s safe
/// wrapper has NO `hwaccel`/`hw_device_ctx` API at all (confirmed via
/// grep of its source), so this is unavoidably raw FFI against
/// `ffmpeg-sys-next`'s own bindgen-generated bindings. This is Part 2,
/// step 1 of the zero-copy GPU decode redesign — hardware decode WITHOUT
/// a zero-copy render path yet: the decoded frame still gets copied back
/// into ordinary system memory (`av_hwframe_transfer_data`, into an NV12/
/// software-pixel-format frame) and fed through the EXISTING `sws_scale`-
/// based BGRA conversion below, same as the software-decode path — only
/// the decode step itself moves from CPU to GPU. A later step replaces
/// this transfer-back with a direct D3D11-to-Vulkan texture import (see
/// this crate's own plan doc), at which point this copy goes away
/// entirely; until then, this alone already removes decode's CPU cost
/// (the dominant cost for H264/HEVC), which `sws_scale`'s own CPU cost
/// does not share.
/// Extracts the embedded decode-only FFmpeg shared libs (avcodec/avformat/
/// avutil/swresample/swscale) to `~/.config/som/ffmpeg/` on first run and
/// adds that directory to the process's DLL search path, so `ffmpeg-next`'s
/// FFI calls resolve them without requiring a system FFmpeg install.
///
/// Shared between `som.exe` (which needs this for its own embedded-video
/// playback) and `somcat.exe` (a separate, short-lived process that links
/// `ffmpeg-next`/`ffmpeg-sys-next` directly to probe a video file's real
/// dimensions before printing its placeholder grid — see `somcat`'s own
/// `video_metadata`) — each is an independent OS process with its own DLL
/// search path, so both must call this, not just `som.exe`. Idempotent:
/// safe to call from both, and safe to call even if the other process
/// already extracted the files (byte-for-byte comparison skips a
/// redundant write, `AddDllDirectory` is a per-process call regardless of
/// what's already on disk).
///
/// Also copies each DLL next to the running process's own `.exe` (not just
/// `AddDllDirectory`'ing `~/.config/som/ffmpeg/`) — confirmed live that
/// `somcat.exe` crashes with `STATUS_DLL_NOT_FOUND` before a single line of
/// `main()` runs (no stderr, no extracted directory) when only
/// `AddDllDirectory` is used: unlike `som.exe` (whose FFmpeg calls are only
/// reachable through code paths the linker doesn't eagerly resolve, so it
/// ends up with no static FFmpeg imports in its own PE import table at
/// all — confirmed via `pefile`), `somcat.exe` calls FFmpeg-backed code
/// (`video_metadata`) directly from `main()`, so the linker keeps real
/// static imports for `avcodec`/`avformat`/`avutil` in its PE header —
/// Windows resolves those at process-load time, before `main()` starts,
/// which is too early for any `AddDllDirectory` call made from inside
/// `main()` to help. The exe's own directory is always the first place
/// Windows' standard DLL search order checks, so a copy there closes this
/// gap regardless of which import-resolution timing a given binary ends up
/// with.
#[cfg(target_os = "windows")]
pub fn ensure_ffmpeg_extracted_and_wired() {
    use gpui::AssetSource;
    use windows::Win32::System::LibraryLoader::{
        AddDllDirectory, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, SetDefaultDllDirectories,
    };
    use windows::core::HSTRING;

    if let Err(err) = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) } {
        log::error!("SetDefaultDllDirectories failed: {err:#} — ffmpeg extraction dir may not be honored");
    }

    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf()));
    let ffmpeg_dir = paths::config_dir().join("ffmpeg");
    for file_name in
        ["avcodec-63.dll", "avformat-63.dll", "avutil-61.dll", "swresample-7.dll", "swscale-10.dll"]
    {
        let target = ffmpeg_dir.join(file_name);
        // Embedded as `.dll.zst` (see `assets::Assets`'s own doc comment
        // on the ffmpeg `#[include]` block for why) — one zstd-decompress
        // pass turns it back into the real DLL bytes before it's ever
        // written to disk. Decompressed unconditionally (not gated behind
        // `target.is_file()`) so the byte-for-byte comparison below can
        // catch a stale on-disk copy from an OLDER build whose embedded
        // FFmpeg trim differs (e.g. missing audio decoders) — an earlier
        // version of this function skipped extraction outright whenever
        // ANY file already existed at `target`, which silently kept a
        // stale DLL in place across every later upgrade until someone
        // manually deleted `~/.config/som/ffmpeg/` — confirmed live as
        // video audio staying silent for an entire debugging session
        // despite the newly built DLL correctly containing the needed
        // decoders, because the STALE one on disk was still the one
        // actually being loaded.
        let asset_path = format!("ffmpeg/windows-amd/{file_name}.zst");
        let Some(compressed) = assets::Assets.load(&asset_path).ok().flatten() else {
            log::error!("missing embedded asset {asset_path:?} — video playback will be unavailable");
            continue;
        };
        let bytes = match assets::decompress_zst(&compressed) {
            Ok(bytes) => bytes,
            Err(err) => {
                log::error!("failed to decompress {asset_path:?}: {err:#} — video playback will be unavailable");
                continue;
            },
        };
        let config_copy_current = std::fs::read(&target).is_ok_and(|existing| existing == bytes);
        if !config_copy_current {
            if let Err(err) = std::fs::create_dir_all(&ffmpeg_dir) {
                log::error!("failed to create {ffmpeg_dir:?}: {err:#}");
            } else if let Err(err) = std::fs::write(&target, &bytes) {
                log::error!("failed to write {target:?}: {err:#}");
            }
        }

        if let Some(exe_dir) = &exe_dir {
            let exe_target = exe_dir.join(file_name);
            let already_current = std::fs::read(&exe_target).is_ok_and(|existing| existing == bytes);
            if !already_current {
                if let Err(err) = std::fs::write(&exe_target, &bytes) {
                    log::error!("failed to write {exe_target:?}: {err:#}");
                }
            }
        }
    }

    if unsafe { AddDllDirectory(&HSTRING::from(ffmpeg_dir.as_os_str())) }.is_null() {
        log::error!("AddDllDirectory({ffmpeg_dir:?}) failed — video playback may be unavailable");
    }
}

#[cfg(windows)]
mod hwaccel {
    /// Attempts to create a D3D11VA hardware device context and attach it
    /// to `codec_ctx` (via `hw_device_ctx`) before the decoder is opened
    /// — mirrors FFmpeg's own documented hwaccel setup sequence
    /// (`doc/examples/hw_decode.c` upstream). Also installs a
    /// `get_format` callback that tells FFmpeg to actually pick the
    /// hardware pixel format (`AV_PIX_FMT_D3D11`) when the codec offers
    /// it — without this callback FFmpeg silently falls back to a
    /// software pixel format even with `hw_device_ctx` set, since
    /// multiple codecs may offer several possible output formats and the
    /// caller must choose.
    ///
    /// Returns `false` (not an `Err`) on any failure — hwaccel setup
    /// failing (no compatible GPU, driver issue, etc.) is an expected,
    /// non-fatal outcome: the caller falls back to ordinary software
    /// decode exactly as if this function had never been called, per
    /// this module's own "hardware decode is an enhancement, not a
    /// requirement" pattern already used for the embedded audio track.
    pub fn try_attach_d3d11va(codec_ctx: *mut ffmpeg_sys_next::AVCodecContext) -> bool {
        unsafe {
            let mut hw_device_ctx: *mut ffmpeg_sys_next::AVBufferRef = std::ptr::null_mut();
            let ret = ffmpeg_sys_next::av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                ffmpeg_sys_next::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if ret < 0 || hw_device_ctx.is_null() {
                log::info!("video hwaccel: D3D11VA device creation failed (ret={ret}) — falling back to software decode");
                return false;
            }
            (*codec_ctx).hw_device_ctx = ffmpeg_sys_next::av_buffer_ref(hw_device_ctx);
            ffmpeg_sys_next::av_buffer_unref(&mut hw_device_ctx);
            (*codec_ctx).get_format = Some(get_hw_format);
            true
        }
    }

    /// FFmpeg's `AVCodecContext::get_format` callback — called once the
    /// decoder knows the codec's possible output pixel formats and needs
    /// the caller to pick one. `fmt` is a null-terminated array; picking
    /// `AV_PIX_FMT_D3D11` when present is what actually activates
    /// hardware decode — otherwise FFmpeg falls through to its own
    /// default (a software format), silently defeating
    /// `try_attach_d3d11va` above despite `hw_device_ctx` being set.
    unsafe extern "C" fn get_hw_format(
        _ctx: *mut ffmpeg_sys_next::AVCodecContext,
        fmt: *const ffmpeg_sys_next::AVPixelFormat,
    ) -> ffmpeg_sys_next::AVPixelFormat {
        unsafe {
            let mut p = fmt;
            while *p != ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_NONE {
                if *p == ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_D3D11 {
                    log::info!("video hwaccel: decoder offered AV_PIX_FMT_D3D11 — hardware decode active");
                    return *p;
                }
                p = p.add(1);
            }
            log::info!("video hwaccel: D3D11 pixel format not offered by decoder — falling back to software decode");
            *fmt
        }
    }

    /// `true` if `frame` is still GPU-resident (`AV_PIX_FMT_D3D11`) and
    /// needs [`transfer_to_system_memory`] before any CPU-side pixel
    /// access (`sws_scale` included) can touch it.
    pub fn is_hw_frame(frame: &ffmpeg_next::util::frame::video::Video) -> bool {
        unsafe { (*frame.as_ptr()).format == ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 }
    }

    /// Copies a GPU-resident decoded frame back into an ordinary system-
    /// memory frame (`av_hwframe_transfer_data`) — FFmpeg picks the
    /// transferred-to pixel format itself (typically NV12 for 8-bit
    /// 4:2:0 content), same as it would for `av_hwframe_transfer_data`'s
    /// documented default-format behavior when `dst` arrives empty/
    /// unconfigured. `dst` is reused across calls by the caller (an
    /// `ffmpeg_next::util::frame::video::Video::empty()` reused each
    /// frame) purely to avoid a fresh allocation every frame; ownership
    /// of any previously-held buffer is dropped by FFmpeg's own internal
    /// unref inside `av_hwframe_transfer_data` — see that function's own
    /// documented behavior for `dst` frames that already reference data.
    pub fn transfer_to_system_memory(
        hw_frame: &ffmpeg_next::util::frame::video::Video,
        dst: &mut ffmpeg_next::util::frame::video::Video,
    ) -> bool {
        unsafe {
            let ret = ffmpeg_sys_next::av_hwframe_transfer_data(dst.as_mut_ptr(), hw_frame.as_ptr(), 0);
            ret >= 0
        }
    }
}

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

/// The video's embedded audio track is always resampled down to this many
/// channels before reaching `cpal`, regardless of the source layout (mono,
/// stereo, 5.1, 7.1, ...) — matches what every other real media player
/// does by default when the output device itself is stereo (the vast
/// majority of real-world playback setups), and lets FFmpeg's
/// `swresample` perform an actual channel MIX (folding center/surround
/// channels into L/R at the correct gain) rather than a naive channel-
/// count-preserving pass-through. Confirmed live as a real bug otherwise:
/// with the source's own channel count passed straight to `cpal`, a 5.1
/// track's front L/R/LFE/surrounds came through fine on a stereo output
/// device, but the CENTER channel — where dialogue normally lives —
/// played far too quiet, since nothing was ever mixing it into L/R.
const OUTPUT_CHANNELS: u16 = 2;

/// How much of an in-progress SRP video transfer is available right now —
/// same shape and reasoning as
/// [`crate::rich_content_audio_player::AudioTransferProgress`]: `Terminal`
/// is the sole writer, the decode thread the sole reader, kept as
/// independent state rather than a shared reference because the decode
/// thread can't safely hold one into `Terminal`'s own `RichContentCache`.
#[derive(Default)]
pub struct VideoTransferProgress {
    contiguous_len: AtomicU64,
    /// See `som_srv::protocol::SrvResponse::Progress::tail_available_from`'s
    /// own doc comment — lets [`GrowingFileStream::read`] serve a
    /// `SeekFrom::End`-derived read once the specific tail region has
    /// arrived, without waiting for `contiguous_len` to grow all the way
    /// there from 0. Defaults to 0 (matching `total_size`'s own 0
    /// default before the first real update) rather than `u64::MAX`, so
    /// an unset `VideoTransferProgress` doesn't look like "tail already
    /// available" before any real progress has been reported.
    tail_available_from: AtomicU64,
    /// Out-of-order byte ranges that have arrived (via an explicit
    /// `RequestByteRange` seek response) but haven't yet been folded into
    /// either watermark above — see `som_srv::protocol::SrvResponse::
    /// Progress::pending_ranges`'s own doc comment. Lets
    /// [`GrowingFileStream::read`] serve a mid-file seek's target
    /// directly once ITS specific range has arrived, instead of only
    /// ever waiting for `contiguous_len` to grow forward into it —
    /// that fallback is what made the seek-delay scale with how far the
    /// seek target sat from the front of the file.
    pending_ranges: Mutex<Vec<(u64, u64)>>,
    total_size: AtomicU64,
}

impl VideoTransferProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&self, contiguous_len: u64, tail_available_from: u64, pending_ranges: Vec<(u64, u64)>, total_size: u64) {
        self.contiguous_len.store(contiguous_len, Ordering::Release);
        self.tail_available_from.store(tail_available_from, Ordering::Release);
        *self.pending_ranges.lock().unwrap_or_else(|p| p.into_inner()) = pending_ranges;
        self.total_size.store(total_size, Ordering::Release);
    }

    fn contiguous_len(&self) -> u64 {
        self.contiguous_len.load(Ordering::Acquire)
    }

    pub fn total_size(&self) -> u64 {
        self.total_size.load(Ordering::Acquire)
    }

    /// Returns the pending range (if any) that covers `position` — i.e.
    /// `position` already sits at or past this range's start, so a read
    /// starting there can be served up to this range's end without
    /// waiting for either front/back watermark to reach it.
    fn pending_range_covering(&self, position: u64) -> Option<(u64, u64)> {
        self.pending_ranges
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .copied()
            .find(|&(start, end)| position >= start && position < end)
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

/// A `Read + Seek` view over the SAME in-memory forward-only buffer
/// [`crate::rich_content_srv_channel::SrvProgressState`] owns — see this
/// module's own top-level doc comment for why this exists instead of
/// `ffmpeg::format::input(path)`'s plain file-path open, and see
/// `SrvProgressState`'s own doc comment for why there's no on-disk file
/// behind any of this anymore (`som-srv` no longer persists chunks to
/// disk at all — a full on-disk copy per playback previously exhausted
/// real disk space on a large-enough file and blocked it from playing).
///
/// `read` blocks (sleep-retry) when the requested range extends past
/// [`VideoTransferProgress::contiguous_len`] — the file's real end is
/// [`VideoTransferProgress::total_size`], not how much has been
/// buffered so far, and only returns `Ok(0)` (real EOF) once
/// `contiguous_len` has actually reached `total_size`. `seek`'s
/// `SeekFrom::End` and `AVSEEK_SIZE` probing (handled by `StreamIo`'s
/// own `seek` callback via `Seek::stream_position`+`SeekFrom::End(0)`)
/// both need `Seek`'s own notion of "the end" to agree with that same
/// `total_size`, not the buffer's current length — otherwise FFmpeg's
/// index/moov scan (which seeks near the presumed end of the file)
/// would either undershoot on a still-growing file or overshoot past
/// what a completed one's own `stream_len` reports, both of which
/// produce a `seek` past valid bounds. This struct's `Seek` impl
/// therefore treats the file as if it were already `total_size` bytes
/// long from the very first byte, with everything past `contiguous_len`
/// simply not readable yet — reads into that region block exactly like
/// a live network read past its currently-buffered prefix would.
struct GrowingFileStream {
    srv_state: Arc<crate::rich_content_srv_channel::SrvProgressState>,
    progress: Arc<VideoTransferProgress>,
    stop: Arc<AtomicBool>,
    /// Same `seek_request` [`run_decode_loop`]'s outer loop reads — this
    /// struct's own `read` checks it on every retry-sleep iteration so a
    /// blocking read stuck waiting for bytes that may be far from
    /// arriving yet can bail out immediately once the user seeks
    /// elsewhere. This is NOT redundant with `input_from_stream_with_
    /// interrupt`'s own interrupt closure passed at `run_decode_loop`'s
    /// `ictx` construction site: that closure is polled by FFmpeg's
    /// custom-AVIO `read` trampoline only at the START of each attempt to
    /// call INTO this `read` — but this `read` never itself returns
    /// control while it's blocked in its own internal sleep-retry loop
    /// waiting for bytes, so the trampoline has no opportunity to poll
    /// the interrupt closure again until this call returns SOMETHING.
    /// Returning `Err(Interrupted)` here is that "something": the
    /// trampoline's own retry loop (`ffmpeg_next::format::context::
    /// stream_io::read`) treats `Interrupted` as "call me again," which
    /// immediately re-polls the interrupt closure — now `true` — and
    /// aborts cleanly with `AVERROR_EXIT` instead of calling back into
    /// this `read` a second time (confirmed via that function's own
    /// source: `Interrupted` isn't a spin risk here specifically because
    /// the interrupt check the wrapper does right before is exactly what
    /// makes forward progress).
    seek_request: Arc<Mutex<Option<f32>>>,
    position: u64,
    /// Fires a fresh `RequestByteRange` for whatever position `read`
    /// itself is blocked waiting on — see [`RequestByteRange`]'s own doc
    /// comment for why this exists as well as `seek_to_fraction`'s own
    /// one-off window request: `avformat_seek_file`'s internal keyframe/
    /// index hunt reads at positions this struct has no visibility into
    /// ahead of time, so the ONLY reliable way to keep a cold (never-
    /// requested-before) seek fast is for `read` to ask for bytes right
    /// where it's ACTUALLY stuck, the moment it first notices it's
    /// stuck — not to guess a wider window up front and hope it's wide
    /// enough. `last_requested_position` suppresses re-requesting the
    /// SAME stuck position on every 100ms retry-sleep iteration (the
    /// request is already in flight; re-sending it doesn't make the
    /// bytes arrive any faster, just adds needless socket traffic).
    request_byte_range: Option<RequestByteRange>,
    last_requested_position: Option<u64>,
}

impl GrowingFileStream {
    fn open(
        srv_state: Arc<crate::rich_content_srv_channel::SrvProgressState>,
        progress: Arc<VideoTransferProgress>,
        stop: Arc<AtomicBool>,
        seek_request: Arc<Mutex<Option<f32>>>,
        request_byte_range: Option<RequestByteRange>,
    ) -> std::io::Result<Self> {
        Ok(Self {
            srv_state,
            progress,
            stop,
            seek_request,
            position: 0,
            request_byte_range,
            last_requested_position: None,
        })
    }
}

impl std::io::Read for GrowingFileStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let diag_wait_started_at = Instant::now();
        let mut diag_slept = false;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "video player dropped"));
            }
            if self.seek_request.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
                return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "seek requested"));
            }
            let contiguous_len = self.progress.contiguous_len();
            let total_size = self.progress.total_size();
            let tail_available_from = self.progress.tail_available_from.load(Ordering::Acquire);
            // Three independent sources can make `self.position` readable
            // — see `VideoTransferProgress::is_readable`'s own doc comment
            // for why a `SeekFrom::End`-derived position needs the SECOND
            // one (`tail_available_from`), not just `contiguous_len`; the
            // THIRD (`pending_ranges`) is what lets a mid-file seek's
            // targeted `RequestByteRange` response be read immediately,
            // instead of only ever waiting for `contiguous_len` to grow
            // forward into it.
            let readable_up_to = if self.position < contiguous_len {
                contiguous_len
            } else if total_size > 0 && self.position >= tail_available_from {
                total_size
            } else if let Some((_, end)) = self.progress.pending_range_covering(self.position) {
                end
            } else {
                0
            };
            if readable_up_to > self.position {
                // `read_buffered` can still return 0 here even though the
                // watermark says this position is readable — the buffer's
                // own `append_chunk` runs on the SAME background thread as
                // the watermark update, so this is normally momentary
                // (the next loop iteration sees it), not a real gap; see
                // `SrvProgressState::append_chunk`'s own doc comment for
                // the one case it genuinely never arrives (a `RequestByte
                // Range` answer landing ahead of the buffer's own tail
                // gets dropped, same as any other out-of-order chunk this
                // reader hasn't caught up to yet) — falling through to the
                // retry-sleep below handles both cases identically.
                let n = self.srv_state.read_buffered(self.position, buf);
                if n > 0 {
                    self.position += n as u64;
                    self.srv_state.advance_consumed_up_to(self.position);
                    self.last_requested_position = None;
                    if diag_slept {
                        log::debug!("DIAG: GrowingFileStream::read unblocked after {:?}", diag_wait_started_at.elapsed());
                    }
                    return Ok(n);
                }
            }
            if total_size > 0 && self.position >= total_size {
                return Ok(0); // Real EOF — the whole file has arrived and we've read all of it.
            }
            // Actively ask for bytes at THIS exact position rather than
            // only ever waiting on whatever `seek_to_fraction`'s own
            // fixed-size window guessed — see `request_byte_range`'s own
            // doc comment for why the guessed window frequently misses:
            // `avformat_seek_file`'s internal keyframe/index hunt for
            // this container can land anywhere, not just near the linear
            // fraction-based estimate. Only fires once per stuck
            // position (`last_requested_position` dedupes across this
            // loop's own 100ms retry-sleep iterations) — the request is
            // already in flight; resending it doesn't make bytes arrive
            // faster.
            if let Some(request_byte_range) = self.request_byte_range.as_ref()
                && self.last_requested_position != Some(self.position)
            {
                // The buffer has nothing usable for `self.position` (that's
                // why we're here) and the answer to this request will land
                // at `self.position`, arbitrarily far ahead of whatever the
                // buffer's current tail is — `append_chunk`'s own doc
                // comment is explicit that an ahead-of-tail chunk is
                // otherwise silently DROPPED, not queued, so without this
                // reset the fetched bytes would vanish the moment they
                // arrive and this read would block forever. Clearing here
                // (immediately before the request that will need the
                // fresh anchor) rather than from `Seek::seek` itself keeps
                // FFmpeg's own frequent probe-time seeks within the
                // already-buffered region cheap — only a seek that's
                // ACTUALLY unreachable from what's buffered pays this cost.
                self.srv_state.reset_buffer_for_seek(self.position);
                const ON_DEMAND_RANGE_LEN: u64 = 4 * 1024 * 1024;
                request_byte_range(self.position, ON_DEMAND_RANGE_LEN);
                self.last_requested_position = Some(self.position);
            }
            // Caught up to what's currently buffered, but more is still
            // coming — block (StreamIo's own doc comment requires a
            // blocking stream; FFmpeg has no retry layer of its own for
            // custom I/O) until the next `Progress` push advances
            // `contiguous_len` further.
            diag_slept = true;
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

/// Carries a raw `*mut AVFormatContext` (plus the two plain values a
/// stuck-seek watchdog thread needs) across a thread boundary — see
/// `run_decode_loop`'s own seek-timeout call site for the full
/// reasoning. `unsafe impl Send` asserts what the type system can't see
/// on its own: the pointed-to memory is never touched by more than one
/// thread at a time in practice (`seek_in_flight` is what actually
/// guarantees that), so moving the raw pointer VALUE across threads is
/// sound even though the pointee itself isn't `Sync`. Declared at
/// module scope (NOT as a local `struct` inside `run_decode_loop`
/// itself) — a local type's `unsafe impl Send` was observed NOT
/// satisfying `std::thread::spawn`'s `F: Send` bound for the closure
/// that captures it, even with the impl written directly beside it;
/// promoting the type to module scope resolved it.
struct SendPtr(*mut ffmpeg_sys_next::AVFormatContext, usize, i64);
unsafe impl Send for SendPtr {}

/// Outcome of one attempt to read the next demuxed packet — a thin
/// wrapper around `ffmpeg::codec::packet::Packet::read`'s own
/// `Result<(), ffmpeg::Error>` that distinguishes the THREE outcomes
/// `run_decode_loop`'s caller needs to treat differently, which
/// `ffmpeg_next::format::context::input::PacketIter` (the ergonomic
/// `Iterator` wrapper used everywhere else in this codebase) collapses
/// into just `Some`/`None` — see [`read_next_packet`]'s own doc comment
/// for why that collapse is unsafe to rely on here specifically.
enum NextPacket {
    Packet(ffmpeg_next::codec::packet::Packet),
    /// The read was cut short by `run_decode_loop`'s own interrupt
    /// closure (see `input_from_stream_with_interrupt`'s call site) —
    /// NOT a real end of stream. The container is still perfectly
    /// healthy; the caller should simply retry (typically after handling
    /// whatever `seek_request` caused the interrupt in the first place).
    Interrupted,
    /// A genuine end of stream — every byte of the file has been
    /// demuxed.
    Eof,
}

/// Reads exactly one packet, retrying past a corrupt one exactly like
/// `PacketIter::next`'s own doc comment describes (`AVERROR_INVALIDDATA`
/// is not latched into the `AVIOContext`, so retrying makes progress),
/// but — unlike `PacketIter`, which folds EVERY non-`Ok`/non-`InvalidData`
/// outcome into a single `None` — keeping `Error::Exit` (our own
/// interrupt closure firing) distinguishable from `Error::Eof` (a real
/// end of stream). Collapsing those two would misroute an interrupted
/// mid-playback seek into `run_decode_loop`'s real-EOF handling, which
/// sets `playing = false`/`finished = true`; nothing else in this module
/// ever restores `playing` afterward, so that misrouting would silently
/// pause playback on every seek that happens to land while a blocking
/// read is in flight — the exact class of bug `PacketIter`'s own doc
/// comment warns "callers that must observe these errors" need to avoid
/// by driving `Packet::read` directly.
fn read_next_packet(ictx: &mut ffmpeg_next::format::context::Input) -> NextPacket {
    use ffmpeg_next::Error;
    use ffmpeg_next::codec::packet::Packet;
    loop {
        let mut packet = Packet::empty();
        match packet.read(ictx) {
            Ok(()) => return NextPacket::Packet(packet),
            Err(Error::Eof) => return NextPacket::Eof,
            Err(Error::Exit) => return NextPacket::Interrupted,
            Err(Error::InvalidData) => continue,
            Err(_) => return NextPacket::Eof,
        }
    }
}

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
/// a HIGHER resolution than the feature itself. The result: playback
/// opened, decoded the codec parameters for stream 8 (mjpeg) instead of
/// stream 0 (h264), failed immediately ("Could not find codec
/// parameters"), and the placement never appeared at all — not a crash,
/// just silent failure. MJPEG is the standard codec real-world muxers
/// (ffmpeg, mkvmerge) use for embedded cover art specifically — never a
/// legitimate movie/show's own primary video codec in practice — so
/// deprioritizing it below every non-MJPEG candidate closes this gap
/// without needing per-container attachment-flag parsing (MKV's own
/// attachment/cover-art disposition flags aren't uniformly exposed
/// through `ffmpeg-next`'s safe API today).
fn best_video_stream(ictx: &ffmpeg_next::format::context::Input) -> Option<ffmpeg_next::format::stream::Stream<'_>> {
    use ffmpeg_next as ffmpeg;
    let non_mjpeg = ictx
        .streams()
        .filter(|s| s.parameters().id() != ffmpeg::codec::Id::MJPEG)
        .max_by_key(|s| (s.parameters().medium() == ffmpeg::media::Type::Video, s.frames(), s.duration()));
    non_mjpeg.filter(|s| s.parameters().medium() == ffmpeg::media::Type::Video).or_else(|| ictx.streams().best(ffmpeg::media::Type::Video))
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
    srv_state: Arc<crate::rich_content_srv_channel::SrvProgressState>,
    shared: Arc<LatestFrame>,
    progress: Arc<VideoTransferProgress>,
    stop: Arc<AtomicBool>,
    time_base: Arc<(AtomicI64, AtomicI64)>,
    duration_us: Arc<AtomicI64>,
    seek_request: Arc<Mutex<Option<f32>>>,
    seek_generation: Arc<AtomicU64>,
    seek_in_flight: Arc<AtomicBool>,
    playing: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    shared_audio: Arc<SharedAudio>,
    // Overrides FFmpeg's own `best()` heuristic for which audio stream
    // to decode — `somcat`'s `-a <N>` CLI flag, carried here via
    // `ContentMetadata::Video::audio_stream_index` (see that field's own
    // doc comment). `None` keeps the existing heuristic-based selection
    // unchanged.
    audio_stream_index_override: Option<u32>,
    request_byte_range: Option<RequestByteRange>,
) {
    use ffmpeg_next as ffmpeg;

    let _ = ffmpeg::init();

    // Wait for the sender's own file extension to arrive (part of the
    // FIRST `Progress` push's `ContentMetadata::Video::extension` — see
    // `SrvProgressState::extension`'s own doc comment) before even
    // attempting the probe below — confirmed live as load-bearing, not
    // cosmetic: a fixed generic hint (e.g. `.video` for every file
    // regardless of real container) let MKV probe successfully (its
    // EBML header is distinctive enough on its own) but made MP4 and AVI
    // fixtures fail probing entirely ("Could not find codec parameters
    // ... unspecified pixel format") — those containers' probes lean on
    // the extension hint more heavily. There is no on-disk `Path` to
    // derive this from anymore (`SrvCache`'s own doc comment: `som-srv`
    // no longer persists chunks to disk at all).
    let extension = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Some(extension) = srv_state.extension() {
            break extension;
        }
        std::thread::sleep(DECODE_RETRY_INTERVAL);
    };
    let filename_owned = format!("placement.{extension}");

    let Ok(stream) = GrowingFileStream::open(
        srv_state.clone(),
        progress.clone(),
        stop.clone(),
        seek_request.clone(),
        request_byte_range.clone(),
    ) else {
        return;
    };
    let Ok(stream_io) = ffmpeg::format::context::StreamIo::from_read_seek(stream) else {
        return;
    };
    // `filename` is passed through to ffmpeg's own probe purely as an
    // extension hint (nudges format detection, never itself opens
    // anything) — real bytes always come through `stream_io`.
    let filename = Some(filename_owned.as_str());
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
    // An interrupt callback, not just the blocking-retry `Read` above, is
    // what actually lets a pending seek cut a stalled read short:
    // `ictx.packets().next()` is a single call into FFmpeg from Rust's
    // point of view, and `GrowingFileStream::read`'s own retry-sleep loop
    // has no way to hand control back to `run_decode_loop`'s outer loop
    // until it has real bytes to return — a seek arriving while stuck
    // waiting on bytes far from the current read position would
    // otherwise only be noticed once those original bytes eventually
    // arrive (confirmed live: seeking mid-playback appeared to "do
    // nothing" for minutes, then suddenly apply). FFmpeg's custom-AVIO
    // read path polls this closure at the top of every read attempt
    // (`ffmpeg_next::format::context::stream_io::read`) — returning
    // `true` aborts the in-flight read immediately with `Error::Exit`,
    // which `PacketIter::next` (see its own doc comment on terminal
    // errors) turns into a plain `None`, letting the outer decode loop
    // regain control on its very next iteration and handle the seek from
    // `seek_request` there.
    // Interrupts BOTH an ordinary packet read stuck waiting for bytes
    // (compares against `seek_request`) AND a seek already in progress
    // whose OWN `ictx.seek()` call is stuck waiting for the target
    // keyframe's bytes to arrive (compares against `handled_seek_
    // generation`, updated below right before `ictx.seek()` runs) — see
    // that field's own doc comment for why a plain `seek_request.is_
    // some()` check alone isn't enough to interrupt a SECOND seek that
    // arrives while the FIRST one's own `ictx.seek()` is still in
    // flight (confirmed live: repeated rapid clicks on the seek bar
    // could freeze the whole window for minutes, not just the first
    // click).
    let interrupt_seek_request = seek_request.clone();
    let interrupt_seek_generation = seek_generation.clone();
    let handled_seek_generation = Arc::new(AtomicU64::new(0));
    let interrupt_handled_seek_generation = handled_seek_generation.clone();
    let Ok(mut ictx) = ffmpeg::format::input_from_stream_with_interrupt(stream_io, filename, None, move || {
        let _ = &interrupt_seek_generation;
        let _ = &interrupt_handled_seek_generation;
        interrupt_seek_request.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }) else {
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

    let Some(input) = best_video_stream(&ictx) else {
        return; // No video track at all — never will be one.
    };
    let video_stream_index = input.index();
    let stream_time_base = input.time_base();
    time_base.0.store(stream_time_base.numerator() as i64, Ordering::Release);
    time_base.1.store(stream_time_base.denominator() as i64, Ordering::Release);

    let Ok(mut context_decoder) = ffmpeg::codec::context::Context::from_parameters(input.parameters()) else {
        return;
    };
    // Zero-copy GPU decode, step 1: attempt D3D11VA hardware decode
    // before opening the decoder — `try_attach_d3d11va` mutates the
    // codec context in place (sets `hw_device_ctx`/`get_format`) and
    // simply does nothing (leaving software decode as the outcome) on
    // any failure, so this call is unconditional and its result isn't
    // otherwise consulted here — the actual pixel-format check happens
    // per-frame below (`hwaccel::is_hw_frame`), since a `get_format`
    // callback returning a software format on this specific stream is
    // also a valid (if slower) outcome the decode loop must already
    // handle uniformly with "hwaccel never attached at all."
    #[cfg(windows)]
    hwaccel::try_attach_d3d11va(unsafe { context_decoder.as_mut_ptr() });
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
    // `audio_stream_index_override` counts audio streams ONLY (0-based
    // among just the audio tracks, matching how `somcat`'s `-a <N>`
    // flag — and every other player's own track picker — numbers them
    // for a user, e.g. "the 2nd dub language"), NOT the raw
    // `Stream::index()` (which counts every stream in the container —
    // video, audio, subtitles — interleaved). Falls back to FFmpeg's own
    // `best()` heuristic if `None` (unset) or the requested index is out
    // of range (fewer audio tracks than asked for) — same graceful-
    // fallback principle every other probe failure in this function
    // already uses, rather than aborting playback outright over an
    // audio-track mismatch.
    let audio_stream_index = audio_stream_index_override
        .and_then(|n| ictx.streams().filter(|s| s.parameters().medium() == ffmpeg::media::Type::Audio).nth(n as usize))
        .or_else(|| ictx.streams().best(ffmpeg::media::Type::Audio))
        .map(|input| input.index());
    // Tell libavformat to skip every stream this player will never touch
    // (extra audio dubs, subtitle tracks, cover-art/font attachments) at
    // the DEMUX level, not just discard their packets after the fact in
    // this loop's own `NextPacket::Packet(_) => continue` branch below.
    // `AVStream.discard` is checked inside `av_read_frame` itself — a
    // discarded stream's packets are skipped without the same per-packet
    // allocation/parsing cost a kept-then-thrown-away packet still pays.
    // A real (if secondary) win for files with unusually many tracks —
    // e.g. a 16GB Matroska file with 22 streams (1 video + 7 audio dubs +
    // 14 subtitle tracks, `ffprobe`-confirmed) — though live measurement
    // showed this alone doesn't explain a much bigger bug found the same
    // session: see `awaiting_post_seek_pts`'s own doc comment for the
    // actual cause of "seeking while paused never shows the new frame."
    // `ffmpeg_next`'s safe `Stream`/`StreamMut` API has no discard
    // setter (only a getter), so this goes through the raw `AVStream`
    // pointer directly, matching this module's own established pattern
    // for raw FFI where the safe wrapper doesn't expose something.
    unsafe {
        let format_ctx = ictx.as_mut_ptr();
        for i in 0..(*format_ctx).nb_streams as isize {
            let stream = *(*format_ctx).streams.offset(i);
            let index = (*stream).index as usize;
            if index != video_stream_index && Some(index) != audio_stream_index {
                (*stream).discard = ffmpeg_sys_next::AVDiscard::AVDISCARD_ALL;
            }
        }
    }
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
        let source_channels = decoder.channels();
        let rate = decoder.rate();
        if source_channels == 0 || rate == 0 {
            log::warn!("video's audio decoder has channels=0 or rate=0 — codec params unresolved, skipping audio");
            return None;
        }
        // Always downmix to stereo (see `OUTPUT_CHANNELS`'s own doc
        // comment for why) — the value stored here and read by the UI
        // is deliberately the OUTPUT channel count, matching what
        // `resampled_audio`'s plane actually contains.
        shared_audio.sample_rate.store(rate as u64, Ordering::Release);
        shared_audio.channels.store(OUTPUT_CHANNELS as u64, Ordering::Release);
        let host = cpal::default_host();
        let Some(device) = host.default_output_device() else {
            log::warn!("video's audio: no default output audio device");
            return None;
        };
        let config =
            cpal::StreamConfig { channels: OUTPUT_CHANNELS, sample_rate: rate, buffer_size: cpal::BufferSize::Default };
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
    // Only populated/used when `hwaccel::is_hw_frame(&decoded)` — holds
    // the system-memory copy `hwaccel::transfer_to_system_memory`
    // produces from a GPU-resident decoded frame, reused across frames
    // like `decoded`/`scaled` above to avoid a fresh allocation every
    // frame.
    #[cfg(windows)]
    let mut hw_transferred = ffmpeg::util::frame::video::Video::empty();

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
    // The `pts` that was in `shared.slot` at the moment the MOST RECENT
    // seek was taken — `None` once a frame with a DIFFERENT `pts` has
    // been stored since. While `Some`, the pause-gate below must not
    // stall: without this, a seek that arrives while paused breaks the
    // gate once (correctly), but the very NEXT outer-loop iteration
    // (reached after `Interrupted`/`continue`, or simply after handling
    // the seek and looping back around) re-checks `has_a_frame_already
    // && !playing` — which is STILL true, because `shared.slot` still
    // holds the PRE-seek frame; the real post-seek frame hasn't been
    // decoded yet. With `seek_request` already consumed (`None`), the
    // gate then blocks forever: nothing else ever sets `playing = true`
    // or repopulates `seek_request` for a paused player. Confirmed live
    // as the actual cause of "seeking while paused just does nothing" —
    // not slow demuxing, not a slow `avformat_seek_file` call (both were
    // separately measured at low milliseconds), but the decode thread
    // never reaching `read_next_packet` again after the seek at all.
    let mut awaiting_post_seek_pts: Option<i64> = None;
    let mut diag_seek_started_at: Option<Instant> = None;

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
            // Zero-copy GPU decode, step 1 (see this module's own
            // `hwaccel` doc comment): a GPU-resident decoded frame can't
            // be handed to `sws_scale` directly — copy it back into
            // system memory first. This is the ONLY extra cost hwaccel
            // decode adds over the software path (removing it entirely
            // is the follow-up zero-copy-render step); the decode itself
            // still ran on the GPU, which is the expensive part for
            // H264/HEVC.
            #[cfg(windows)]
            let source_frame = if hwaccel::is_hw_frame(&decoded) {
                if !hwaccel::transfer_to_system_memory(&decoded, &mut hw_transferred) {
                    continue;
                }
                &hw_transferred
            } else {
                &decoded
            };
            #[cfg(not(windows))]
            let source_frame = &decoded;

            let scaler = match scaler {
                Some(scaler) => scaler,
                None => {
                    let Ok(built) = ffmpeg::software::scaling::context::Context::get(
                        source_frame.format(),
                        source_frame.width(),
                        source_frame.height(),
                        ffmpeg::format::Pixel::RGBA,
                        source_frame.width(),
                        source_frame.height(),
                        ffmpeg::software::scaling::flag::Flags::BILINEAR,
                    ) else {
                        continue;
                    };
                    scaler.insert(built)
                },
            };
            if scaler.run(source_frame, &mut scaled).is_err() {
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
            if decoder.channels() == 0 {
                continue;
            }
            // Always resample down to stereo, regardless of the source
            // layout (mono, stereo, 5.1, 7.1, ...) — see `OUTPUT_CHANNELS`'s
            // own doc comment for why: FFmpeg's `swresample` performs a
            // proper channel-mix (folding the center/surround channels into
            // L/R at correct gain) whenever source and target layouts
            // differ, rather than the naive channel-count-preserving
            // pass-through this code used before, which sent all 6 raw
            // 5.1 channels straight to `cpal` with NO mixing — confirmed
            // live as a real bug: a stereo-only output device played the
            // front L/R and LFE/surround channels fine, but the dialogue-
            // carrying CENTER channel (which physically has no direct L/R
            // output path in 5.1) came through far too quiet, since
            // nothing was ever folding it in.
            let target_layout = ffmpeg::ChannelLayout::STEREO;
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
        if has_a_frame_already && !playing.load(Ordering::Acquire) && awaiting_post_seek_pts.is_none() {
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
        let taken_seek_request = seek_request.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(fraction) = taken_seek_request {
            diag_seek_started_at = Some(Instant::now());
            let duration = duration_us.load(Ordering::Acquire);
            let handled_generation_before = seek_generation.load(Ordering::Acquire);
            log::debug!(
                "video decode loop saw seek_request: fraction={fraction} duration_us={duration} generation={handled_generation_before}"
            );
            // Snapshot the pre-seek `pts` — see `awaiting_post_seek_pts`'s
            // own doc comment for why the pause-gate must stay exempt
            // until `shared.slot` shows something ELSE.
            awaiting_post_seek_pts = shared.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, pts)| *pts);
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
                //
                // Re-arm the interrupt for THIS seek's own internal I/O
                // (`ictx.seek()` can call back into `GrowingFileStream::
                // read` while hunting for the target keyframe) by
                // stamping `handled_seek_generation` to match the
                // generation this seek was issued under — done
                // BEFORE calling `ictx.seek()` so the interrupt closure
                // reports `false` for the remainder of this seek's own
                // reads, but immediately starts reporting `true` again
                // the instant a NEWER seek bumps `seek_generation` past
                // this snapshot (see `handled_seek_generation`'s own doc
                // comment at the interrupt-closure construction site for
                // why `seek_request.is_some()` alone can't express this).
                handled_seek_generation.store(handled_generation_before, Ordering::Release);
                // `ffmpeg_next::format::context::Input::seek` hardcodes
                // `stream_index = -1` in its call to `avformat_seek_file`
                // (confirmed by reading that function's own source) —
                // fine for a SINGLE-stream container (`avformat_seek_
                // file` itself special-cases `stream_index == -1 && nb_
                // streams == 1` by rewriting it to `0`), but for a
                // container with MORE than one stream (this file has
                // both video AND audio), `stream_index` stays `-1` all
                // the way down into `matroska_read_seek`, which does
                // `AVStream *st = s->streams[stream_index]` — an
                // out-of-bounds C array read at index `-1` when `stream_
                // index` is `-1` and `nb_streams > 1`. That reads
                // garbage as an `AVStream*`, and the garbage `sti->
                // index_entries` it dereferences afterward can make
                // `matroska_read_seek`'s own `while` loop (hunting for a
                // valid index entry) spin forever — confirmed live as
                // exactly the freeze this fix targets: `ictx.seek()`
                // never returning, reproduced deterministically by a
                // dedicated regression test
                // (`rapid_repeated_seeks_do_not_hang_the_decode_thread`)
                // against this SAME two-stream fixture. Bypassing the
                // safe wrapper and calling `avformat_seek_file` directly
                // with the REAL `video_stream_index` fixes this at the
                // root — mirrors `Input::seek`'s own `unlatch_exit`/
                // re-poison-on-failure logic exactly, since that part of
                // the contract is unrelated to the stream-index bug.
                // `seek_in_flight` brackets the raw `avformat_seek_file`
                // call below — see that field's own doc comment (on
                // `RichContentVideoPlayer`) for why: a burst of rapid
                // seek requests arriving before the decode thread had
                // even handled the FIRST one reproduced a genuine hang
                // deep inside `avformat_seek_file` for this codec/
                // container (confirmed via a dedicated regression test
                // AND a battery of isolation tests ruling out this
                // module's own generation-counter/interrupt-closure
                // logic, `GrowingFileStream`'s watermark checks, hwaccel
                // attachment, and the `stream_index` value passed in —
                // none of those were the cause). `seek_to_fraction`
                // itself checks this same flag and refuses to queue a
                // new seek while it's set, so by construction only ONE
                // `avformat_seek_file` call for this `AVFormatContext`
                // is ever in flight at a time.
                seek_in_flight.store(true, Ordering::Release);
                // Run the raw call on a SEPARATE, throwaway thread with a
                // bounded wait — confirmed live (via a dedicated
                // regression test plus a battery of isolation tests
                // ruling out every piece of this module's own logic:
                // the generation counter, the interrupt closure,
                // `GrowingFileStream`'s watermark checks, hwaccel
                // attachment, and the exact `stream_index` value passed
                // in) that `avformat_seek_file` can hang FOREVER inside
                // libavformat/matroskadec itself for this codec/
                // container combination — not a bug in anything this
                // module controls, and not something fixable without
                // patching FFmpeg's own C source and rebuilding it via
                // vcpkg. A raw `*mut AVFormatContext` isn't `Send`, so
                // `SendPtr` below asserts (unsafely, but soundly: the
                // POINTED-TO memory is never touched by more than one
                // thread at a time in practice, since `seek_in_flight`
                // already prevents this decode thread from touching
                // `ictx` again until this call resolves ONE way or the
                // other) that moving the raw pointer value across the
                // thread boundary is fine even though the pointee isn't
                // `Sync`. If the timeout elapses, the spawned thread is
                // simply abandoned — Rust has no safe way to kill a
                // thread stuck in someone else's C code — and this
                // decode thread re-opens the file from scratch instead
                // of ever calling into this SAME poisoned
                // `AVFormatContext` again.
                // `avformat_seek_file`'s `min_ts`/`ts`/`max_ts` are in
                // `AV_TIME_BASE` (microsecond) units ONLY when
                // `stream_index == -1` — passing a REAL stream index (as
                // this call does, to avoid the `stream_index=-1` OOB read
                // for multi-stream containers, see the long comment
                // above) means FFmpeg instead interprets them in THAT
                // STREAM's own `time_base` units. `target_us` must be
                // rescaled from microseconds into `stream_time_base`
                // ticks before the call, or every seek's timestamp is
                // silently off by orders of magnitude — confirmed live as
                // exactly this bug: every click landed within the last
                // few seconds of the file regardless of where on the
                // seek bar it was, because the un-rescaled microsecond
                // value, read as (typically much coarser) stream ticks,
                // is a timestamp far beyond the stream's real duration,
                // which `avformat_seek_file` clamps down to the last
                // available keyframe.
                let stream_num = time_base.0.load(Ordering::Acquire).max(1);
                let stream_den = time_base.1.load(Ordering::Acquire).max(1);
                let target_ts = ((target_us as i128 * stream_den as i128) / (stream_num as i128 * 1_000_000)) as i64;
                let send_ptr = SendPtr(unsafe { ictx.as_mut_ptr() }, video_stream_index, target_ts);
                let (seek_done_tx, seek_done_rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    // Force capturing the WHOLE `SendPtr` (which IS
                    // `Send`), not just its `.0` field (a bare `*mut
                    // AVFormatContext`, which ISN'T) — Rust 2021's
                    // disjoint closure captures otherwise capture
                    // individual fields directly the moment they're the
                    // only ones a closure body touches, silently
                    // bypassing the wrapper's own `unsafe impl Send`
                    // entirely (confirmed live: this was the actual
                    // cause of `*mut AVFormatContext cannot be sent
                    // between threads safely` even with `SendPtr`
                    // declared at module scope with its own `Send` impl
                    // right next to it). Binding the WHOLE value under
                    // its own name first, then destructuring, makes the
                    // closure capture that single named `SendPtr` value.
                    let send_ptr = send_ptr;
                    let SendPtr(format_ctx, video_stream_index, target_ts) = send_ptr;
                    let result = unsafe {
                        let pb = (*format_ctx).pb;
                        let was_exit_latched = !pb.is_null() && (*pb).error == ffmpeg_sys_next::AVERROR_EXIT;
                        if was_exit_latched {
                            (*pb).error = 0;
                            (*pb).eof_reached = 0;
                        }
                        let ret = ffmpeg_sys_next::avformat_seek_file(
                            format_ctx,
                            video_stream_index as i32,
                            i64::MIN,
                            target_ts,
                            target_ts,
                            0,
                        );
                        if ret < 0 && was_exit_latched && !pb.is_null() {
                            (*pb).error = ffmpeg_sys_next::AVERROR_EXIT;
                            (*pb).eof_reached = 1;
                        }
                        ret
                    };
                    let _ = seek_done_tx.send(result);
                });
                const SEEK_TIMEOUT: Duration = Duration::from_secs(5);
                let seek_result = match seek_done_rx.recv_timeout(SEEK_TIMEOUT) {
                    Ok(ret) if ret >= 0 => Ok(()),
                    Ok(ret) => Err(ffmpeg::Error::from(ret)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        log::warn!(
                            "video seek to target_us={target_us} did not return within {SEEK_TIMEOUT:?} — \
                             a known FFmpeg/libavformat hang for this codec/container combination; \
                             re-opening the file from scratch instead of waiting forever"
                        );
                        seek_in_flight.store(false, Ordering::Release);
                        // Restore the target fraction into `seek_request`
                        // BEFORE recursing — this specific call already
                        // `take()`n it (see the `if let Some(fraction) =
                        // ... .take()` above), so without this the user's
                        // seek would simply vanish: the file would
                        // re-open at the start and play from position 0
                        // instead of honoring where they actually clicked.
                        *seek_request.lock().unwrap_or_else(|p| p.into_inner()) = Some(fraction);
                        // Re-open the file from scratch on a FRESH
                        // `AVFormatContext` — the one this loop was just
                        // using (`ictx`) is left behind, still owned by
                        // the abandoned thread stuck inside `avformat_
                        // seek_file`'s C code for it, and must never be
                        // touched again from here (a second call into
                        // the SAME poisoned context would just hang
                        // again). Plain tail recursion, not a loop, so
                        // every local this function's top half sets up
                        // (`ictx`, `decoder`, `scaler`, `resampler`, the
                        // audio decoder, `pace_anchor`, etc.) gets
                        // rebuilt cleanly rather than needing to be
                        // manually reset in place.
                        return run_decode_loop(
                            srv_state,
                            shared,
                            progress,
                            stop,
                            time_base,
                            duration_us,
                            seek_request,
                            seek_generation,
                            seek_in_flight,
                            playing,
                            finished,
                            shared_audio,
                            audio_stream_index_override,
                            request_byte_range,
                        );
                    },
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        Err(ffmpeg::Error::Other { errno: ffmpeg_sys_next::EINVAL })
                    },
                };
                seek_in_flight.store(false, Ordering::Release);
                if let Some(started_at) = diag_seek_started_at {
                    log::debug!("DIAG: avformat_seek_file resolved after {:?}", started_at.elapsed());
                }
                if let Err(err) = seek_result {
                    log::warn!("video seek to target_us={target_us} failed: {err}");
                }
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

        match read_next_packet(&mut ictx) {
            NextPacket::Packet(packet) if packet.stream() == video_stream_index => {
                if decoder.send_packet(&packet).is_ok() {
                    receive_and_store(&mut decoder, &mut scaler, &mut pace_anchor);
                    // Clear the pause-gate exemption once a genuinely NEW
                    // frame (different `pts` than the pre-seek snapshot)
                    // has landed — see `awaiting_post_seek_pts`'s own doc
                    // comment. A decoder can need more than one packet
                    // before `receive_frame` yields anything (B-frame
                    // reordering) or can occasionally yield the SAME pts
                    // back (rare, but not impossible for some streams),
                    // so this checks the actual stored value rather than
                    // assuming one `receive_and_store` call is enough.
                    if awaiting_post_seek_pts.is_some() {
                        let current_pts =
                            shared.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, pts)| *pts);
                        if current_pts != awaiting_post_seek_pts {
                            awaiting_post_seek_pts = None;
                            if let Some(started_at) = diag_seek_started_at.take() {
                                log::debug!("DIAG: post-seek frame landed after {:?}", started_at.elapsed());
                            }
                        }
                    }
                }
            },
            NextPacket::Packet(packet) if Some(packet.stream()) == audio_stream_index => {
                if let Some(audio_decoder) = audio_decoder.as_mut() {
                    if audio_decoder.send_packet(&packet).is_ok() {
                        receive_and_store_audio(audio_decoder, &mut resampler);
                    }
                }
            },
            NextPacket::Packet(_) => continue,
            // The read was cut short by our own interrupt closure because
            // a seek is pending (see `GrowingFileStream::read`'s and
            // `input_from_stream_with_interrupt`'s call site doc comments)
            // — NOT a real end of stream. Must NOT fall into the `Eof`
            // branch below: that branch sets `playing = false` and
            // `finished = true`, which would silently pause playback on
            // every mid-playback seek (confirmed by working through the
            // control flow: nothing else ever restores `playing` to
            // `true` after that, so the video would sit paused until the
            // user manually pressed play again — exactly the kind of
            // regression this distinction exists to avoid). Simply loop
            // back to the top, where the pending `seek_request` is
            // handled on the very next iteration.
            NextPacket::Interrupted => continue,
            NextPacket::Eof => {
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
    /// The `pts` that was sitting in `shared.slot` at the moment
    /// [`Self::seek_to_fraction`] was last called — `None` once
    /// `current_frame()` has re-anchored past it. `current_frame()`'s own
    /// `started.is_none()` re-anchor branch used to grab whatever `pts`
    /// happened to be in `shared.slot` the very next time it was called,
    /// which is paint-driven and runs far more often than the decode
    /// thread can complete a seek — in practice this almost always meant
    /// re-anchoring to the STALE pre-seek frame still sitting in the
    /// slot, which made every later real post-seek frame's `pts` look
    /// enormously far in the future relative to that bogus anchor, so
    /// `current_frame()` kept returning the stale cached image
    /// indefinitely (confirmed live: only an unrelated pause/resume or a
    /// second seek — both of which reset `started` again AND happen to
    /// land after the real frame has already arrived — ever unstuck it).
    /// Recording the pre-seek `pts` here lets `current_frame()` refuse to
    /// re-anchor until `shared.slot` actually shows something ELSE,
    /// guaranteeing the anchor is built from the real post-seek frame.
    pending_seek_from_pts: Arc<Mutex<Option<i64>>>,
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
    /// Incremented by [`Self::seek_to_fraction`] every time it's called
    /// — lets the interrupt closure installed at `run_decode_loop`'s
    /// `ictx` construction site distinguish "no newer seek has arrived"
    /// from "a newer seek has arrived" even though `seek_request` itself
    /// gets `take()`n (cleared to `None`) the moment the decode thread
    /// STARTS handling a seek, well before that seek's own `ictx.seek()`
    /// call (which can itself block on `GrowingFileStream::read` hunting
    /// for the target keyframe) finishes. Without this, a SECOND seek
    /// arriving while the first one's own `ictx.seek()` is still
    /// in-flight had no way to interrupt it: the interrupt closure only
    /// ever checked `seek_request.is_some()`, which was already `false`
    /// again (the first seek had already consumed it) — confirmed live
    /// as the exact bug behind "freezes after several rapid clicks on
    /// the seek bar, not on the first click."
    seek_generation: Arc<AtomicU64>,
    /// Set by the decode thread right before it calls `avformat_seek_
    /// file` (via the raw FFI wrapper around `ictx.seek()`), cleared
    /// right after that call returns — regardless of success or
    /// failure. `seek_to_fraction` refuses to queue a NEW seek while
    /// this is `true`, deliberately dropping the click on the floor
    /// rather than letting it queue up: several rapid clicks arriving
    /// before the decode thread had handled even the FIRST one
    /// reproduced a genuine hang deep inside `avformat_seek_file` for
    /// this codec/container combination, confirmed by a dedicated
    /// regression test — root-caused to something inside libavformat's
    /// own seek machinery reacting badly to being re-entered while a
    /// PRIOR `avformat_seek_file` call for the same `AVFormatContext`
    /// hadn't actually returned yet, not to anything wrong with the
    /// generation-counter/interrupt logic above (which was built,
    /// tested, and confirmed NOT to be the cause — see the isolation
    /// tests immediately below this struct's own module for the full
    /// investigation trail). This flag makes that scenario structurally
    /// impossible rather than chasing the exact line inside FFmpeg's C
    /// code where it hangs.
    seek_in_flight: Arc<AtomicBool>,
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

/// A `RequestByteRange` request for `(offset, len)` — shared shape for
/// both the one-off request `seek_to_fraction` fires and the ongoing
/// requests [`GrowingFileStream::read`] itself fires when it blocks on a
/// byte range nobody has asked for yet. See [`GrowingFileStream::read`]'s
/// own doc comment for why relying SOLELY on the seek-time request isn't
/// enough: FFmpeg's `avformat_seek_file` frequently needs bytes outside
/// the fixed window that call requested (hunting for a container's own
/// index/keyframe near, but not exactly at, the estimated linear byte
/// offset), and this callback is `read`'s only way to ask for more
/// without knowing anything about `session_id`/`file_id` itself (those
/// live in `crates/terminal`'s `Terminal`, a GPUI type this module can't
/// depend on).
type RequestByteRange = Arc<dyn Fn(u64, u64) + Send + Sync>;

impl RichContentVideoPlayer {
    /// Starts decoding `path` (the on-disk cache file for one SRP video
    /// transfer, possibly still growing) on a background thread. Starts
    /// paused, matching audio/GIF's own "don't autoplay" convention.
    /// `request_byte_range` is `None` in tests that already have the
    /// whole file locally (nothing to request) — see [`RequestByteRange`]'s
    /// own doc comment for why production callers always pass `Some`.
    pub fn open(
        srv_state: Arc<crate::rich_content_srv_channel::SrvProgressState>,
        progress: Arc<VideoTransferProgress>,
        audio_stream_index: Option<u32>,
        request_byte_range: Option<RequestByteRange>,
    ) -> Self {
        let shared = Arc::new(LatestFrame { slot: Mutex::new(None) });
        let decode_stop = Arc::new(AtomicBool::new(false));
        let time_base = Arc::new((AtomicI64::new(0), AtomicI64::new(1)));
        let duration_us = Arc::new(AtomicI64::new(0));
        let seek_request = Arc::new(Mutex::new(None));
        let seek_generation = Arc::new(AtomicU64::new(0));
        let seek_in_flight = Arc::new(AtomicBool::new(false));
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
            let seek_generation = seek_generation.clone();
            let seek_in_flight = seek_in_flight.clone();
            let playing = playing.clone();
            let finished = finished.clone();
            let shared_audio = shared_audio.clone();
            std::thread::spawn(move || {
                run_decode_loop(
                    srv_state,
                    shared,
                    progress,
                    decode_stop,
                    time_base,
                    duration_us,
                    seek_request,
                    seek_generation,
                    seek_in_flight,
                    playing,
                    finished,
                    shared_audio,
                    audio_stream_index,
                    request_byte_range,
                )
            });
        }

        Self {
            shared,
            decode_stop,
            time_base,
            playing,
            playback_started_at: Arc::new(Mutex::new(None)),
            pending_seek_from_pts: Arc::new(Mutex::new(None)),
            duration_us,
            seek_request,
            seek_generation,
            seek_in_flight,
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
        // Deliberately drops the click on the floor while a PRIOR seek's
        // own `avformat_seek_file` call hasn't returned yet — see
        // `seek_in_flight`'s own doc comment for the full reasoning
        // (several rapid clicks arriving before the decode thread
        // handled even the first one reproduced a genuine hang deep
        // inside FFmpeg's own seek machinery for this codec/container,
        // root-caused via a dedicated regression test plus a battery of
        // isolation tests). The user's next click after the in-flight
        // seek actually completes works normally — this only rejects
        // seeks that arrive WHILE one is already running, not seeking in
        // general.
        if self.seek_in_flight.load(Ordering::Acquire) {
            return;
        }
        let _generation = self.seek_generation.fetch_add(1, Ordering::AcqRel) + 1;
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
        // Snapshot whatever `pts` is in `shared.slot` RIGHT NOW (still the
        // pre-seek frame) — see `pending_seek_from_pts`'s own doc comment
        // for why `current_frame()` needs this instead of re-anchoring on
        // whatever it finds the next time it's called.
        *self.pending_seek_from_pts.lock().unwrap_or_else(|p| p.into_inner()) =
            self.shared.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, pts)| *pts);
        // Invalidates the paint-side pacing anchor `current_frame` uses
        // while playing — see that method's own doc comment on the `if
        // started.is_none()` branch for why this is required (not just
        // an optimization) for a seek that happens WHILE ALREADY
        // PLAYING: without clearing this, `current_frame` kept comparing
        // the freshly seeked-to frame's PTS against the PRE-seek anchor,
        // almost always judging the new frame "too far in the future"
        // and freezing the picture on the pre-seek frame until an
        // unrelated pause/resume cycle happened to re-establish a fresh
        // anchor via `toggle_play_pause`.
        *self.playback_started_at.lock().unwrap_or_else(|p| p.into_inner()) = None;
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
            let mut started = self.playback_started_at.lock().unwrap_or_else(|p| p.into_inner());
            // `None` here means "no anchor yet relative to the CURRENT
            // decode run" — true both right after `toggle_play_pause()`
            // resumes (which sets a real anchor immediately, so this
            // branch is mostly moot there) and, more importantly, right
            // after a seek WHILE ALREADY PLAYING: `seek_to_fraction`
            // clears this to `None` but has no way to know the new
            // frame's PTS in advance (only the decode thread, on its own
            // separate `pace_anchor`, learns that once the first
            // post-seek frame actually decodes) — so establish the
            // anchor HERE, from whatever `slot` holds right now, the
            // first time this method observes the cleared state. Without
            // this, `started` stayed permanently `None` after a
            // mid-playback seek — no anchor ever got set again once
            // `toggle_play_pause()` wasn't the thing that triggered the
            // resume — so EVERY subsequent frame after the very first
            // post-seek one skipped this gate entirely, i.e. pacing
            // silently stopped being enforced at all for the rest of
            // playback. Confirmed live as the actual bug: a seek while
            // playing left the on-screen picture frozen on the pre-seek
            // frame (this same `if started.is_none()` gap meant the
            // stale cached image in `last_rendered` — from BEFORE this
            // fix, when the gate as originally written unconditionally
            // fell through past a `None` anchor straight to the
            // cache — never got invalidated by a fresh PTS check), and
            // only a pause/resume cycle (which DOES call
            // `toggle_play_pause`, re-establishing a real anchor) made
            // the seeked-to frame appear.
            if started.is_none() {
                // Refuse to anchor on a `pts` that matches the snapshot
                // taken at seek time (still the pre-seek frame — the
                // decode thread hasn't overwritten `shared.slot` yet) —
                // see `pending_seek_from_pts`'s own doc comment. Falling
                // through with `started` still `None` just means this
                // same "not anchored yet" branch runs again next paint,
                // which is fine: paint happens far more often than the
                // decode thread can complete a seek, so within a frame
                // or two `pts` will actually change and this unblocks.
                let mut pending_from = self.pending_seek_from_pts.lock().unwrap_or_else(|p| p.into_inner());
                if *pending_from != Some(pts) {
                    *pending_from = None;
                    *started = Some((Instant::now(), pts));
                }
            }
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
        let data = std::fs::read(&path).unwrap();
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let srv_state = Arc::new(crate::rich_content_srv_channel::SrvProgressState::default());
        srv_state.seed_whole_file_for_test(&data, extension);
        let total_size = data.len() as u64;
        let progress = Arc::new(VideoTransferProgress::new());
        progress.update(total_size, 0, Vec::new(), total_size);
        Some(RichContentVideoPlayer::open(srv_state, progress, None, None))
    }

    fn wait_for_decode(player: &RichContentVideoPlayer) -> bool {
        wait_for_decode_with_attempts(player, 150)
    }

    /// Same as [`wait_for_decode`], but with a caller-chosen attempt count
    /// (20ms apart) instead of the fixed 3s budget — needed by tests whose
    /// probe genuinely has more work to do before a first frame, e.g.
    /// `seek_near_end_of_large_local_file_completes_quickly`'s initial
    /// open, where the container's index sits near the end of a 16GB file
    /// and reaching it can require SEVERAL chained on-demand `RequestByteRange`
    /// round trips (each with its own thread-spawn + disk-seek + retry-
    /// interval latency), not just one.
    fn wait_for_decode_with_attempts(player: &RichContentVideoPlayer, attempts: u32) -> bool {
        for _ in 0..attempts {
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

    /// Regression test for [`read_next_packet`]: an interrupted read
    /// (`Error::Exit`, fired by `input_from_stream_with_interrupt`'s
    /// closure) must come back as `NextPacket::Interrupted`, NOT
    /// `NextPacket::Eof` — the two look identical through the ergonomic
    /// `PacketIter` this module deliberately avoids for exactly this
    /// reason (see `read_next_packet`'s own doc comment). Drives a real
    /// fixture file directly (not through `RichContentVideoPlayer`) so
    /// the interrupt closure's `true`/`false` transition is fully
    /// controlled from the test itself.
    #[test]
    fn read_next_packet_distinguishes_interrupted_from_real_eof() {
        let path = test_fixture_path("sample_1920x1080.mkv");
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let _ = ffmpeg_next::init();

        let file = std::fs::File::open(&path).expect("opening fixture");
        let stream_io =
            ffmpeg_next::format::context::StreamIo::from_read_seek(file).expect("wrapping fixture in StreamIo");
        let abort = Arc::new(AtomicBool::new(false));
        let interrupt_flag = abort.clone();
        let mut ictx = ffmpeg_next::format::input_from_stream_with_interrupt(
            stream_io,
            Some("sample_1920x1080.mkv"),
            None,
            move || interrupt_flag.load(Ordering::Acquire),
        )
        .expect("opening fixture as an Input");

        // Baseline: with the interrupt flag not yet set, an ordinary read
        // against a healthy, fully-on-disk file must succeed.
        match read_next_packet(&mut ictx) {
            NextPacket::Packet(_) => {},
            other => panic!("expected a real packet before any interrupt, got a {other:?}-shaped outcome"),
        }

        // Now arm the interrupt and keep reading — FFmpeg's custom-AVIO
        // read trampoline only polls the closure when it actually needs
        // to pull fresh bytes through our `Read` callback, not on every
        // packet (its own internal `AVIOContext` buffer, 32KB by
        // default, can serve several packets from what a PREVIOUS read
        // already pulled in) — so this drains packets until the buffer
        // is exhausted and a real callback (and therefore the interrupt
        // check) happens. Bounded so a genuine regression (interrupt
        // never observed at all) fails the test instead of hanging.
        abort.store(true, Ordering::Release);
        let mut saw_interrupted = false;
        for _ in 0..10_000 {
            match read_next_packet(&mut ictx) {
                NextPacket::Packet(_) => continue,
                NextPacket::Interrupted => {
                    saw_interrupted = true;
                    break;
                },
                NextPacket::Eof => panic!("hit real EOF before ever observing an interrupt — file too short for this test's buffer-draining assumption"),
            }
        }
        assert!(saw_interrupted, "expected Interrupted once the abort flag was set and enough packets were drained to exhaust AVIOContext's read-ahead buffer");

        // Un-arm and seek back to the start (mirrors `run_decode_loop`'s
        // own seek-handling block: `take()` the request BEFORE calling
        // `ictx.seek()`, so the interrupt closure already reports `false`
        // by the time `Input::seek`'s own `unlatch_exit` clears the
        // `AVERROR_EXIT` latch) — reads must resume normally afterward,
        // proving the interrupt doesn't permanently poison the context.
        abort.store(false, Ordering::Release);
        ictx.seek(0, ..).expect("seeking back to the start after an interrupt");
        match read_next_packet(&mut ictx) {
            NextPacket::Packet(_) => {},
            other => panic!("expected reads to resume normally after un-arming + seeking, got a {other:?}-shaped outcome"),
        }
    }

    impl std::fmt::Debug for NextPacket {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                NextPacket::Packet(_) => write!(f, "Packet"),
                NextPacket::Interrupted => write!(f, "Interrupted"),
                NextPacket::Eof => write!(f, "Eof"),
            }
        }
    }

    /// Regression test for the bug this session's fix targets: seeking
    /// WHILE the video is actively playing must leave `is_playing()`
    /// still `true` afterward. Before `read_next_packet` existed,
    /// `run_decode_loop` drove `ictx.packets().next()` (`PacketIter`),
    /// which collapses an interrupted read into a plain `None` —
    /// indistinguishable from real end-of-stream — so a seek landing
    /// while a blocking read was in flight would fall into the
    /// real-EOF branch, which sets `playing = false`/`finished = true`.
    /// Nothing else in this module ever restores `playing` afterward, so
    /// that misrouting silently paused playback on every such seek,
    /// requiring a manual play click to resume (confirmed live as the
    /// exact symptom reported: seeking mid-playback appeared to freeze
    /// the video until pause/play was pressed).
    #[test]
    fn seeking_while_playing_does_not_pause() {
        let Some(player) = open_test_player("sample_1920x1080.mkv") else { return };
        assert!(wait_for_decode(&player), "decode thread never produced a first frame");
        player.toggle_play_pause();
        assert!(player.is_playing(), "must be playing before the seek this test exercises");

        let total_size = std::fs::metadata(test_fixture_path("sample_1920x1080.mkv")).unwrap().len();
        let progress = Arc::new(VideoTransferProgress::new());
        progress.update(total_size, 0, Vec::new(), total_size);
        player.seek_to_fraction(0.5, &progress, |_offset, _len| {});

        // Give the decode thread real time to observe and act on the
        // seek request — generous budget since this is exactly the path
        // that used to stall for a long time under the bug this test
        // guards against.
        let mut still_playing_after_seek = false;
        for _ in 0..250 {
            std::thread::sleep(Duration::from_millis(20));
            if player.is_playing() {
                still_playing_after_seek = true;
                break;
            }
        }
        assert!(still_playing_after_seek, "seeking while playing must not leave the player paused");
    }

    /// Regression test for a real, reproduced-live bug: seeking WHILE
    /// PAUSED on a large file never showed the new frame at all — not
    /// slow, permanently stuck. Root cause, confirmed via live
    /// instrumentation (temporary, since removed): the outer decode
    /// loop's pause-gate (`has_a_frame_already && !playing`) correctly
    /// breaks the FIRST time it sees a pending `seek_request`, but the
    /// very NEXT time the outer loop reaches the top of its `loop` body
    /// — reached either right after handling the seek, or via `NextPacket::
    /// Interrupted`'s `continue` — `shared.slot` STILL holds the pre-seek
    /// frame (the real one hasn't decoded yet) and `seek_request` is
    /// already consumed, so the gate re-triggers and sleeps forever:
    /// nothing else ever sets `playing = true` or repopulates `seek_
    /// request` for a paused player. `avformat_seek_file` itself and
    /// individual `GrowingFileStream`/`read_next_packet` calls were all
    /// separately measured in the low milliseconds — this was purely a
    /// control-flow bug in this module, not slow demuxing or a slow
    /// libavformat call. See `awaiting_post_seek_pts`'s own doc comment
    /// for the fix (exempts the pause-gate until a genuinely new frame
    /// lands).
    ///
    /// Runs against a real, large (16GB+) local file specifically
    /// because that's what the original live report used — `GrowingFileStream`
    /// behaves identically whether the file arrived via `som-srv`
    /// streaming or (as here) already sits fully on disk, so a local
    /// open reproduces the same bug without needing a live transfer.
    /// Skipped (not failed) if the specific movie file isn't present on
    /// this machine.
    #[test]
    fn seek_near_end_of_large_local_file_completes_quickly() {
        let path = std::path::PathBuf::from(
            "C:/home/dnk/som/Ready.or.Not.2.Here.I.Come.2026.1080p.MA.WEB-DLRip.x264-HiDt_EniaHD.mkv",
        );
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let total_size = std::fs::metadata(&path).unwrap().len();
        let srv_state = crate::rich_content_srv_channel::SrvProgressState::spawn_streaming_seed_for_test(path.clone());
        let progress = Arc::new(VideoTransferProgress::new());
        progress.update(0, 0, Vec::new(), total_size);
        // Real production callers always pass `Some` here (see `RequestByteRange`'s
        // own doc comment) — a seek target far ahead of the sequential
        // streaming thread's current position (the whole point of this
        // test, seeking to fraction=0.9 of a 16GB file) needs an on-demand
        // fetch, not a wait for the sequential stream to catch up.
        let request_byte_range_state = srv_state.clone();
        let request_byte_range_path = path.clone();
        let request_byte_range: RequestByteRange = Arc::new(move |offset, len| {
            request_byte_range_state.serve_byte_range_from_disk_for_test(request_byte_range_path.clone(), offset, len);
        });
        let player = RichContentVideoPlayer::open(srv_state, progress.clone(), None, Some(request_byte_range));
        // 750 * 20ms = 15s — this file's index sits near the very end
        // (confirmed: probing it reads at byte offset ~16.15GB), so
        // reaching it can take several chained on-demand `RequestByteRange`
        // round trips before the first frame decodes, unlike every other
        // fixture in this test module (all either small enough to seed
        // whole, or fast to probe from the front) — still bounded, not
        // unconditional, so a genuine hang here still fails the test
        // rather than blocking forever.
        assert!(wait_for_decode_with_attempts(&player, 750), "decode thread never produced a first frame");

        let pts_before = player.shared.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, pts)| *pts);
        let seek_started_at = Instant::now();
        player.seek_to_fraction(0.9, &progress, |_offset, _len| {});

        // 250 * 20ms = 5s — generous headroom over the ~120ms this
        // actually takes post-fix, while still failing fast (not 30s)
        // if the pause-gate bug (or something equally bad) regresses.
        let mut new_pts_seen = false;
        for _ in 0..250 {
            std::thread::sleep(Duration::from_millis(20));
            let current_pts = player.shared.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, pts)| *pts);
            if current_pts.is_some() && current_pts != pts_before {
                new_pts_seen = true;
                break;
            }
        }
        let elapsed = seek_started_at.elapsed();
        eprintln!("seek to fraction=0.9 on the 16GB file: new frame appeared after {elapsed:?} (new_pts_seen={new_pts_seen})");
        assert!(new_pts_seen, "seek near the end of the 16GB file never produced a new frame within {elapsed:?}");
    }

    /// Regression test for the bug reported live: the decode thread
    /// freezing for minutes (dragging the whole GPUI window down with
    /// it, since `Terminal::mouse_down` runs synchronously on the main
    /// thread) after SEVERAL rapid clicks on the seek bar — not on the
    /// first click. Root cause: `ictx.seek()` itself can block on
    /// `GrowingFileStream::read` while hunting for the target keyframe's
    /// bytes; a SECOND seek arriving while the FIRST one's `ictx.seek()`
    /// call is still in flight had no way to interrupt it, because the
    /// interrupt closure only checked `seek_request.is_some()` — which
    /// was already `false` again (the first seek had already `take()`n
    /// it before calling `ictx.seek()`). Fixed via `seek_generation`, a
    /// counter bumped on every `seek_to_fraction` call that the interrupt
    /// closure compares against a snapshot taken right before `ictx.
    /// seek()` runs (see that field's own doc comment for the full
    /// reasoning). This test fires many seeks back-to-back, with no
    /// delay between them, and asserts the decode thread is still able
    /// to make forward progress (produce a fresh frame) within a bounded
    /// time afterward — before the fix, this reliably hung until the
    /// test's own timeout.
    #[test]
    fn rapid_repeated_seeks_do_not_hang_the_decode_thread() {
        let Some(player) = open_test_player("sample_1920x1080.mkv") else { return };
        assert!(wait_for_decode(&player), "decode thread never produced a first frame");
        player.toggle_play_pause();
        assert!(player.is_playing());

        let total_size = std::fs::metadata(test_fixture_path("sample_1920x1080.mkv")).unwrap().len();
        let progress = Arc::new(VideoTransferProgress::new());
        progress.update(total_size, 0, Vec::new(), total_size);

        // Fire seeks to varying fractions with NO delay between them —
        // mirrors "several rapid clicks on the seek bar," deliberately
        // not waiting for one seek to finish before issuing the next, so
        // there's a real chance of landing squarely inside a prior
        // seek's own in-flight `ictx.seek()` call.
        for fraction in [0.1_f32, 0.9, 0.2, 0.8, 0.3, 0.7, 0.4, 0.6, 0.5] {
            player.seek_to_fraction(fraction, &progress, |_offset, _len| {});
        }

        // The decode thread must still be alive and producing frames
        // after all that — clear the last-known frame's PTS and wait for
        // a NEW one to land, proving the decode loop is still iterating
        // and not stuck inside a single `ictx.seek()` call forever.
        let last_pts_before = player.shared.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, pts)| *pts);
        let mut made_progress = false;
        for _ in 0..500 {
            std::thread::sleep(Duration::from_millis(20));
            let current_pts = player.shared.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, pts)| *pts);
            if current_pts.is_some() && current_pts != last_pts_before {
                made_progress = true;
                break;
            }
        }
        assert!(
            made_progress,
            "decode thread never produced a new frame after a burst of rapid seeks — it's stuck (the hang this test guards against)"
        );
    }

    /// Isolation test for the hang above: opens the SAME fixture through
    /// FFmpeg's own plain path-based `ffmpeg_next::format::input` (real
    /// `std::fs::File` under the hood, NOT our custom `GrowingFileStream`
    /// `AVIOContext` at all), decodes a handful of packets (mirrors
    /// `wait_for_decode` + `toggle_play_pause` establishing real decoder
    /// state before the seek in the failing test above), then fires ONE
    /// `ictx.seek()` to the exact same `target_us` that hangs in the
    /// real player. If this ALSO hangs, the bug is entirely inside
    /// FFmpeg/libavformat's own seek machinery for this file — nothing
    /// to do with `GrowingFileStream`, the interrupt closure, or
    /// `seek_generation`. If it does NOT hang, the bug is specific to
    /// something about the custom AVIOContext path.
    #[test]
    fn plain_path_open_single_seek_does_not_hang() {
        let path = test_fixture_path("sample_1920x1080.mkv");
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let _ = ffmpeg_next::init();
        // `ffmpeg_next::format::input` needs a registered protocol
        // handler for plain file paths, which this build's statically-
        // linked FFmpeg doesn't have wired up (confirmed live: "Protocol
        // not found") — use `StreamIo::from_read_seek` over an ordinary
        // `std::fs::File` instead, still through the custom-AVIOContext
        // path (same mechanism `GrowingFileStream` itself uses), but
        // with NONE of `GrowingFileStream`'s own watermark-blocking/
        // interrupt logic — isolates whether the hang is inherent to
        // FFmpeg's own seek machinery for this file, or specific to
        // something `GrowingFileStream` itself does.
        let file = std::fs::File::open(&path).expect("opening fixture file");
        let stream_io = ffmpeg_next::format::context::StreamIo::from_read_seek(file).expect("wrapping fixture in StreamIo");
        let filename = path.file_name().and_then(|n| n.to_str());
        let mut ictx = ffmpeg_next::format::input_from_stream(stream_io, filename, None)
            .expect("opening fixture via plain StreamIo (no custom watermark/interrupt logic)");
        let video_stream_index = ictx.streams().best(ffmpeg_next::media::Type::Video).expect("has a video stream").index();

        // Decode a handful of real packets first — mirrors the failing
        // test's own `wait_for_decode` establishing real decoder state
        // before the seek that hangs.
        let mut decoded_packets = 0;
        for (stream, _packet) in ictx.packets() {
            if stream.index() == video_stream_index {
                decoded_packets += 1;
                if decoded_packets >= 5 {
                    break;
                }
            }
        }
        assert!(decoded_packets >= 5, "fixture too short to decode 5 packets before the seek this test exercises");

        // The EXACT target_us the real player's own hang was observed
        // at (`fraction=0.5` against this fixture's `duration_us=
        // 28237000`, per that test's own log output).
        let target_us: i64 = 14_118_500;
        let seek_started_at = Instant::now();
        let seek_result = ictx.seek(target_us, ..target_us);
        let elapsed = seek_started_at.elapsed();
        eprintln!("DEBUG plain path-based ictx.seek finished: result={seek_result:?} took={elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(10),
            "plain path-based ictx.seek() took {elapsed:?} (>10s) — the hang reproduces even WITHOUT the custom AVIOContext, \
             meaning the bug is inside FFmpeg/libavformat itself for this file, not in GrowingFileStream/the interrupt closure"
        );
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

        let srv_state = Arc::new(crate::rich_content_srv_channel::SrvProgressState::default());
        srv_state.set_extension_for_test("mkv");
        let progress = Arc::new(VideoTransferProgress::new());
        let player = RichContentVideoPlayer::open(srv_state.clone(), progress.clone(), None, None);
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
            srv_state.append_piece_for_test(written as u64, &source_bytes[written..end], total_size);
            written = end;
            progress.update(written as u64, total_size, Vec::new(), total_size);
            std::thread::sleep(Duration::from_millis(150));
        }
        // Final piece covers any remainder from integer division above.
        if written < source_bytes.len() {
            srv_state.append_piece_for_test(written as u64, &source_bytes[written..], total_size);
        }
        progress.update(total_size, 0, Vec::new(), total_size);

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
