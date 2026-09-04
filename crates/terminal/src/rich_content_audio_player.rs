//! Decode + playback state for Som's own rich-content protocol's audio
//! files (`.mp3`/`.flac`) — the audio counterpart to
//! [`crate::rich_content_gif_player`] and [`crate::rich_content_player`].
//!
//! Unlike GIF/image players (rebuilt fresh from a cache re-decode on
//! every paint, see `rich_content_player::refresh_or_create`), an audio
//! player owns a real `cpal` output stream whose lifetime must span many
//! paints — a device stream can't be torn down and rebuilt every frame
//! the way an `Arc<RenderImage>` can be replaced. This is why
//! `RichContentAudioPlayer` lives in its own map on `Terminal`
//! (`rich_content_audio_players`), not folded into
//! `rich_content_player::RichContentPlayer`'s image-shaped state.
//!
//! Decoding IS progressive, same principle as GIF's — but shaped
//! differently: GIF cheaply re-decodes its whole available prefix from
//! byte 0 on every paint; re-decoding a growing MP3/FLAC prefix from
//! scratch on every paint would be wasteful for continuous PCM in a way
//! it isn't for GIF's frame counts. Instead, a background thread owns a
//! persistent `symphonia` decoder that reads forward through the SAME
//! (growing) cache file, appending newly-decoded samples to a shared
//! buffer the `cpal` output callback plays from — see
//! [`RichContentAudioPlayer::open`]'s doc comment for the full shape.
//! `open()` can be called as soon as a small prefix of the file has
//! streamed in (enough for `symphonia` to probe format headers), not
//! only once the whole file has arrived — playback and duration are
//! both available immediately, catching up as more bytes arrive over
//! SRP, potentially serviced out of sequential order via a byte-range
//! query (`Terminal::request_audio_byte_range`) when the user seeks
//! ahead of what's been streamed so far.
//!
//! Playback is entirely local to wherever Som itself is running, even
//! when the SRP-sending client (`somcat`) is on a remote SSH host — see
//! this crate's `SRP_INTEGRATION_GUIDE.md` audio section for why: Som is
//! the only process guaranteed to be physically local to the user's
//! speakers.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Fires a `SrvRequest::RequestByteRange` for `(offset, len)` — the audio
/// counterpart to `rich_content_video_player`'s own `RequestByteRange`
/// alias, same shape for the same reason (this module can't depend on
/// `Terminal`/`session_id`/`file_id` directly, so the caller closes over
/// them). `None` in tests that already have the whole file locally
/// (nothing to request).
type RequestByteRange = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// How large an on-demand byte-range fetch to ask for once `GrowingSrvStream::
/// read` finds itself stuck — mirrors video's own `ON_DEMAND_RANGE_LEN`
/// (`rich_content_video_player.rs`). Audio files are typically much
/// smaller than video, but this same fixed window comfortably covers a
/// stuck read regardless — a request that overshoots past `total_size`
/// is harmless (the sender simply clamps it).
const ON_DEMAND_RANGE_LEN: u64 = 4 * 1024 * 1024;

/// A `Read + Seek` view over one placement's `SrvProgressState` in-memory
/// forward-only buffer — the audio counterpart to `rich_content_video_
/// player`'s `GrowingFileStream` (see that type's own doc comment for
/// why there's no on-disk file behind this at all).
///
/// Actively fires its own `RequestByteRange` when stuck, exactly like
/// `GrowingFileStream::read` — an earlier version of this reader only
/// ever waited passively, relying on `Terminal::request_audio_byte_range`
/// (an explicit user seek) to be the sole source of on-demand fetches.
/// That leaves a real gap `som-srv`'s own `SrvCache::subscribe` doc
/// comment calls out explicitly: `som-srv` no longer retains any chunk's
/// bytes once forwarded, so a subscriber arriving after a transfer has
/// already fully streamed through (confirmed live: a short/fast file,
/// e.g. this module's own `tone.flac` test fixture, can finish streaming
/// before Som ever subscribes) gets a replay of the watermark numbers
/// only (`contiguous_len`/`total_size`, both already at the file's full
/// size) with an EMPTY local buffer — `contiguous_len` says every byte
/// is "available," but none of them ever actually arrive, and nothing
/// was watching to trigger a range fetch. Firing a fetch here, the
/// moment a read is stuck despite the watermark claiming readability,
/// closes that gap without requiring the sender to still be connected.
struct GrowingSrvStream {
    srv_state: Arc<crate::rich_content_srv_channel::SrvProgressState>,
    stop: Arc<AtomicBool>,
    position: u64,
    request_byte_range: Option<RequestByteRange>,
    last_requested_position: Option<u64>,
}

impl std::io::Read for GrowingSrvStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "audio player dropped"));
            }
            let n = self.srv_state.read_buffered(self.position, buf);
            if n > 0 {
                self.position += n as u64;
                self.srv_state.advance_consumed_up_to(self.position);
                self.last_requested_position = None;
                return Ok(n);
            }
            let total_size = self.srv_state.total_size();
            if total_size > 0 && self.position >= total_size {
                return Ok(0); // Real EOF.
            }
            // Stuck: the buffer has nothing for `self.position` right
            // now, whether because it genuinely hasn't arrived yet OR
            // because it arrived before this reader ever subscribed (see
            // this struct's own doc comment) — both cases look identical
            // from here, and both are fixed the same way: ask for it.
            // Deduped by `last_requested_position` so a stuck read
            // doesn't resend the same request on every retry-sleep tick.
            if let Some(request_byte_range) = self.request_byte_range.as_ref()
                && self.last_requested_position != Some(self.position)
            {
                self.srv_state.reset_buffer_for_seek(self.position);
                request_byte_range(self.position, ON_DEMAND_RANGE_LEN);
                self.last_requested_position = Some(self.position);
            }
            std::thread::sleep(DECODE_RETRY_INTERVAL);
        }
    }
}

impl std::io::Seek for GrowingSrvStream {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        let total_size = loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "audio player dropped"));
            }
            let total_size = self.srv_state.total_size();
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

impl symphonia::core::io::MediaSource for GrowingSrvStream {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        let total_size = self.srv_state.total_size();
        if total_size > 0 { Some(total_size) } else { None }
    }
}

/// How much of an in-progress SRP audio transfer is available right now
/// — shared between `Terminal` (the only writer, updated each time
/// `RichContentCache::apply_chunk` advances this file's watermark) and
/// a [`RichContentAudioPlayer`]'s background decode thread (the only
/// reader). Plain `Arc<AtomicU64>` pair rather than a closure capturing
/// `&RichContentCache` — `Terminal` (and its `RichContentCache` field)
/// live on the paint/main thread only, so a decode thread can't safely
/// hold a reference into either; this type is the actual data both
/// sides need to agree on, owned independently of both.
#[derive(Default)]
pub struct AudioTransferProgress {
    contiguous_len: std::sync::atomic::AtomicU64,
    total_size: std::sync::atomic::AtomicU64,
}

impl AudioTransferProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called by `Terminal` right after `RichContentCache::apply_chunk`
    /// returns a new `contiguous_len` for this same `(session_id,
    /// file_id)` — keeps this progress snapshot in sync with the cache's
    /// own watermark without the decode thread needing access to the
    /// cache itself.
    pub fn update(&self, contiguous_len: u64, total_size: u64) {
        self.contiguous_len.store(contiguous_len, Ordering::Release);
        self.total_size.store(total_size, Ordering::Release);
    }

    fn contiguous_len(&self) -> u64 {
        self.contiguous_len.load(Ordering::Acquire)
    }

    fn total_size(&self) -> u64 {
        self.total_size.load(Ordering::Acquire)
    }
}

/// How long the background decode thread sleeps between retries when it
/// hits a truncation error but the file isn't fully downloaded yet —
/// short enough that newly-arrived bytes get picked up promptly, long
/// enough not to spin a thread doing nothing but re-probing a handful of
/// bytes hundreds of times a second while a large file is still
/// streaming in over SRP (which itself has its own pacing — a chunk
/// arrives roughly every network round trip, not continuously).
const DECODE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Shared decode target the background thread appends to and the `cpal`
/// callback reads from — plain samples plus the format spec, since both
/// sides need `channels` to interpret `samples` as interleaved frames.
struct SharedPcm {
    samples: Mutex<Vec<f32>>,
    sample_rate: std::sync::atomic::AtomicU32,
    channels: std::sync::atomic::AtomicU32,
    /// Set once the background thread has decoded everything it's ever
    /// going to (file fully downloaded and consumed, or a genuine decode
    /// error stopped it early) — lets `cpal`'s callback distinguish
    /// "no more samples because we're caught up, more might still
    /// arrive" from a real end-of-file, though today both are treated as
    /// "output silence" either way; kept as a separate flag rather than
    /// inferred from thread-liveness so a paint-path caller could later
    /// show a distinct "loading"/"buffering" state without needing to
    /// join a thread handle to check.
    decode_finished: AtomicBool,
}

/// Runs on a dedicated background thread for the lifetime of one
/// [`RichContentAudioPlayer`] — decodes packets from `srv_state`'s
/// in-memory forward-only buffer (see [`GrowingSrvStream`]'s own doc
/// comment for why there's no on-disk file behind this at all) as far as
/// `progress` currently allows, appending samples to `shared`, then
/// repeats: re-checks how many contiguous bytes are available NOW (via
/// `progress`, updated independently by `Terminal` — see
/// [`AudioTransferProgress`]'s own doc comment for why this indirection
/// exists instead of a direct `RichContentCache` reference), and keeps
/// decoding forward from wherever the persistent `FormatReader` left
/// off. `next_packet()` on a partial file reliably errors
/// (IoError/UnexpectedEof-shaped) without corrupting the reader's
/// internal state when it runs out of currently-available bytes —
/// `GrowingSrvStream::read` blocks internally until either more bytes
/// arrive or the transfer completes, same contract `rich_content_video_
/// player::GrowingFileStream::read` documents.
///
/// Exits once `total_size` bytes have been consumed (the whole file is
/// decoded) or a genuine (non-truncation) decode error occurs.
fn run_decode_loop(
    srv_state: Arc<crate::rich_content_srv_channel::SrvProgressState>,
    shared: Arc<SharedPcm>,
    progress: Arc<AudioTransferProgress>,
    stop: Arc<AtomicBool>,
    request_byte_range: Option<RequestByteRange>,
) {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    // Wait for the sender's own file extension (part of the FIRST
    // `Progress` push's `ContentMetadata::Audio::extension` — see
    // `SrvProgressState::extension`'s own doc comment) before probing —
    // same reasoning `rich_content_video_player::run_decode_loop`'s
    // identical wait documents: some containers' probes lean on the
    // extension hint heavily enough that a missing/wrong one can fail
    // probing outright, and there's no on-disk `Path` to derive this
    // from anymore (`SrvCache`'s own doc comment).
    let extension = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Some(extension) = srv_state.extension() {
            break extension;
        }
        std::thread::sleep(DECODE_RETRY_INTERVAL);
    };
    let mut hint = Hint::new();
    if !extension.is_empty() {
        hint.with_extension(&extension);
    }

    // Wait for enough of a prefix to exist that `symphonia` can even
    // find a valid container header — a handful of bytes (a partial
    // MP3 frame sync word, an incomplete FLAC STREAMINFO block) isn't
    // enough, and there's no cheaper way to know "enough" than trying.
    let (mut probed, track_id) = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let stream = GrowingSrvStream {
            srv_state: srv_state.clone(),
            stop: stop.clone(),
            position: 0,
            request_byte_range: request_byte_range.clone(),
            last_requested_position: None,
        };
        let mss = MediaSourceStream::new(Box::new(stream), Default::default());
        match symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        {
            Ok(probed) => {
                let Some(found_track_id) =
                    probed.format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).map(|t| t.id)
                else {
                    return; // No audio track at all — never will be one.
                };
                break (probed, found_track_id);
            },
            Err(_) => {
                let total_size = progress.total_size();
                if total_size > 0 && progress.contiguous_len() >= total_size {
                    return; // Whole file present and still unprobeable — genuinely not decodable.
                }
                std::thread::sleep(DECODE_RETRY_INTERVAL);
            },
        }
    };

    let track = probed.format.tracks().iter().find(|t| t.id == track_id).expect("found above");
    let Ok(mut decoder) = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())
    else {
        return;
    };

    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let packet = match probed.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(_)) => {
                let total_size = progress.total_size();
                if total_size > 0 && progress.contiguous_len() >= total_size {
                    shared.decode_finished.store(true, Ordering::Release);
                    return; // Ran out of bytes AND the file is fully downloaded — real EOF.
                }
                std::thread::sleep(DECODE_RETRY_INTERVAL);
                continue;
            },
            Err(_) => {
                shared.decode_finished.store(true, Ordering::Release);
                return; // A genuine (non-truncation) format error — nothing more to decode.
            },
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A single malformed packet is skippable — same tolerance
            // principle used throughout this protocol's decode paths.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => continue,
            Err(_) => {
                shared.decode_finished.store(true, Ordering::Release);
                return;
            },
        };

        if sample_buf.is_none() {
            let spec = *decoded.spec();
            shared.sample_rate.store(spec.rate, Ordering::Release);
            shared.channels.store(spec.channels.count() as u32, Ordering::Release);
            sample_buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        }
        let buf = sample_buf.as_mut().expect("initialized above");
        buf.copy_interleaved_ref(decoded);
        shared.samples.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(buf.samples());
    }
}

/// One rich-content audio file's playback state — a background decoder
/// thread feeding a shared PCM buffer, plus a live `cpal` output stream
/// reading from it. `position_frames`/`playing` are `Arc<Atomic*>` (not
/// plain fields) because they're read from the paint path (`&self`, see
/// `rich_content_player::RichContentPlayer`'s identical `Cell`-based
/// reasoning) AND written from `cpal`'s own audio-callback thread, which
/// runs independently of any paint call — a `Cell`/`RefCell` isn't
/// `Send`+`Sync` across that boundary, so atomics are the minimum
/// needed, not a stylistic choice.
pub struct RichContentAudioPlayer {
    shared: Arc<SharedPcm>,
    /// `None` until [`Self::poll_ready`] observes a decoded sample
    /// rate/channel count and actually opens the device — `cpal::
    /// StreamConfig` has no "figure it out later" mode, and the decode
    /// thread can take an unbounded amount of time to probe a growing
    /// file's format (worst case: a still-arriving file with no
    /// probeable prefix yet), so this can no longer be a blocking wait
    /// inside `open()` (see `open()`'s own doc comment — an earlier
    /// version blocked the caller, which was this project's actual
    /// paint/main thread, for up to 5 seconds per open and confirmed
    /// live to hang the whole window when whatever ran after that wait
    /// also stalled). Kept alive once `Some` only so `Drop`ping the
    /// player tears down the device stream — never read otherwise.
    /// `cpal::Stream` is not `Send` on every backend, so this field, and
    /// this whole struct, must stay on whichever thread creates it (the
    /// paint/main thread, same as every other `Terminal` field).
    _stream: Option<cpal::Stream>,
    /// Signals the background decode thread to stop — set on `Drop` so
    /// the thread doesn't keep polling a cache file that no longer has
    /// a live player watching it. Not joined (same reasoning `somcat`'s
    /// own background query-reader thread uses): a thread parked in a
    /// blocking `sleep` will notice within `DECODE_RETRY_INTERVAL`, and
    /// nothing in this struct's own lifetime needs to wait for that.
    decode_stop: Arc<AtomicBool>,
    position_frames: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
}

impl Drop for RichContentAudioPlayer {
    fn drop(&mut self) {
        self.decode_stop.store(true, Ordering::Relaxed);
    }
}

impl RichContentAudioPlayer {
    /// Starts decoding `srv_state`'s in-memory buffer (one SRP audio
    /// transfer, possibly still growing) on a background thread and
    /// opens a `cpal` output stream against the default output device,
    /// starting paused (matching how a freshly opened media file/browser
    /// audio element starts paused, not autoplaying) — a placement only
    /// starts making sound once the user clicks its play control.
    ///
    /// Can be called as soon as a probeable prefix exists — the decode
    /// thread itself waits/retries for that internally (see
    /// [`run_decode_loop`]) — callers don't need to gate on
    /// `contiguous_len`/`total_size` themselves before calling this;
    /// `Terminal::rich_content_audio_placements` still needs SOME gate
    /// (a nonzero `contiguous_len`) just to have a real path to pass in
    /// at all.
    ///
    /// Returns immediately — it does NOT wait for the decode thread to
    /// produce a sample rate/channel count, and does NOT open the `cpal`
    /// device itself. An earlier version blocked here (up to 5 seconds)
    /// waiting for exactly that, which meant every call ran on this
    /// project's real caller, the GPUI paint/main thread — confirmed
    /// live to freeze the whole window (unresponsive even to the title
    /// bar's close button) the moment probing or device/stream creation
    /// took long enough. The caller must poll [`Self::poll_ready`] once
    /// per paint until it returns `true` before treating this player as
    /// actually producing sound; until then all playback-state queries
    /// (`is_playing`, `position_fraction`, etc.) are well-defined and
    /// simply report "not started yet" rather than erroring.
    ///
    /// `progress` is polled by the decode thread to distinguish "ran out
    /// of bytes because the file is still streaming in" (keep retrying)
    /// from "ran out of bytes because this really is the end of the
    /// file" (stop) — the CALLER owns and updates it (see
    /// [`AudioTransferProgress`]'s own doc comment for why), typically
    /// once per `RichContentCache::apply_chunk` call for this same
    /// `(session_id, file_id)`.
    pub fn open(
        srv_state: Arc<crate::rich_content_srv_channel::SrvProgressState>,
        progress: Arc<AudioTransferProgress>,
        request_byte_range: Option<RequestByteRange>,
    ) -> Result<Self, String> {
        let shared = Arc::new(SharedPcm {
            samples: Mutex::new(Vec::new()),
            sample_rate: std::sync::atomic::AtomicU32::new(0),
            channels: std::sync::atomic::AtomicU32::new(0),
            decode_finished: AtomicBool::new(false),
        });
        let decode_stop = Arc::new(AtomicBool::new(false));

        {
            let shared = shared.clone();
            let decode_stop = decode_stop.clone();
            std::thread::spawn(move || run_decode_loop(srv_state, shared, progress, decode_stop, request_byte_range));
        }

        Ok(Self {
            shared,
            _stream: None,
            decode_stop,
            position_frames: Arc::new(AtomicU64::new(0)),
            playing: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Non-blocking — call once per paint for a player whose device
    /// stream isn't open yet (`is_ready()` still `false`). Checks
    /// whether the background decode thread has produced a sample
    /// rate/channel count; if so, opens the `cpal` output device and
    /// starts the stream (the same work `open()` used to do inline
    /// while blocking the caller — see `open()`'s own doc comment for
    /// why that moved here) and returns `true`. Returns `false`
    /// (immediately, no waiting) if the format isn't known yet, if the
    /// decode thread finished without ever producing one (genuinely
    /// undecodable file — check [`Self::failed`] to distinguish this
    /// from "still waiting"), or if opening the device itself failed.
    /// A no-op returning `true` if the stream is already open.
    pub fn poll_ready(&mut self) -> bool {
        if self._stream.is_some() {
            return true;
        }
        let sample_rate = self.shared.sample_rate.load(Ordering::Acquire);
        let channels = self.shared.channels.load(Ordering::Acquire);
        if sample_rate == 0 || channels == 0 {
            return false;
        }
        let channels = channels as u16;

        match Self::open_device(sample_rate, channels, self.shared.clone(), self.position_frames.clone(), self.playing.clone())
        {
            Ok(stream) => {
                self._stream = Some(stream);
                true
            },
            Err(err) => {
                log::error!("opening audio output device: {err}");
                self.decode_stop.store(true, Ordering::Relaxed);
                self.shared.decode_finished.store(true, Ordering::Release);
                false
            },
        }
    }

    /// `true` once the decode thread has given up without ever
    /// producing a sample rate (an undecodable file, or a device-open
    /// failure from [`Self::poll_ready`]) — lets a caller distinguish
    /// "still buffering, try again next paint" from "never going to
    /// become ready, stop polling" without needing its own timeout.
    pub fn failed(&self) -> bool {
        self._stream.is_none() && self.shared.decode_finished.load(Ordering::Acquire)
    }

    /// `true` once the `cpal` output stream is open and playback control
    /// (`toggle_play_pause`, `set_playing`) actually produces sound —
    /// before this, those calls are harmless no-ops on `playing`'s own
    /// state (honored retroactively the moment [`Self::poll_ready`]
    /// finishes opening the stream).
    pub fn is_ready(&self) -> bool {
        self._stream.is_some()
    }

    fn open_device(
        sample_rate: u32,
        channels: u16,
        shared: Arc<SharedPcm>,
        position_frames: Arc<AtomicU64>,
        playing: Arc<AtomicBool>,
    ) -> Result<cpal::Stream, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| "no default output audio device".to_string())?;
        let config = cpal::StreamConfig { channels, sample_rate, buffer_size: cpal::BufferSize::Default };

        let cb_shared = shared;
        let cb_position = position_frames;
        let cb_playing = playing;
        let channels_usize = channels as usize;

        let stream = device
            .build_output_stream(
                &config,
                move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    if !cb_playing.load(Ordering::Acquire) {
                        output.fill(0.0);
                        return;
                    }
                    let samples = cb_shared.samples.lock().unwrap_or_else(|p| p.into_inner());
                    let total_frames_available = (samples.len() / channels_usize.max(1)) as u64;
                    let mut frame = cb_position.load(Ordering::Acquire);
                    for out_frame in output.chunks_mut(channels_usize) {
                        if frame >= total_frames_available {
                            // Either genuinely at the end (decode
                            // finished) or just caught up with a
                            // still-streaming file — either way there's
                            // nothing to play THIS callback; only stop
                            // `playing` outright once decoding is truly
                            // done, so a momentary catch-up doesn't look
                            // like the user paused it.
                            out_frame.fill(0.0);
                            if cb_shared.decode_finished.load(Ordering::Acquire) {
                                cb_playing.store(false, Ordering::Release);
                            }
                            continue;
                        }
                        let start = frame as usize * channels_usize;
                        for (dst, src) in out_frame.iter_mut().zip(&samples[start..start + channels_usize]) {
                            *dst = *src;
                        }
                        frame += 1;
                    }
                    cb_position.store(frame, Ordering::Release);
                },
                |err| log::error!("cpal output stream error: {err}"),
                None,
            )
            .map_err(|e| format!("building output stream: {e}"))?;
        stream.play().map_err(|e| format!("starting output stream: {e}"))?;
        Ok(stream)
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// `true` once decoding has consumed the whole file AND playback has
    /// caught up to the very end of it (not merely "decoding is done but
    /// there's still unplayed PCM buffered ahead") — the same "genuinely
    /// reached the end" signal video's own `is_finished` provides, used
    /// by `Terminal::rich_content_audio_placements` to trigger an
    /// automatic stop (freeing this player's resources) the moment
    /// playback naturally ends, not just when the user clicks the stop
    /// icon. `duration_ms` mirrors every other method here that needs a
    /// total-frame count (see [`Self::position_fraction`]'s own doc
    /// comment for why it's a caller-supplied parameter, not tracked
    /// internally).
    pub fn is_finished(&self, duration_ms: u32) -> bool {
        if !self.shared.decode_finished.load(Ordering::Acquire) {
            return false;
        }
        let total_frames = self.total_frames_for(duration_ms);
        total_frames > 0 && self.position_frames.load(Ordering::Acquire) >= total_frames
    }

    pub fn toggle_play_pause(&self) {
        self.playing.fetch_xor(true, Ordering::AcqRel);
    }

    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Release);
    }


    /// Current playback position as a fraction of `duration_ms`
    /// (0.0..=1.0) — unlike the old eager-decode design, THIS player has
    /// no reliable "total frames" of its own to divide by until decoding
    /// finishes (a still-streaming file's ultimate sample count isn't
    /// known from PCM alone), so the caller supplies the file's real
    /// duration, from `ContentMetadata::Audio::duration_ms` (known from
    /// the very first chunk, well before decoding catches up — see this
    /// module's own doc comment) rather than from decoded sample count.
    /// Converted to a frame count internally using the decode thread's
    /// own `sample_rate` once decoding has started (`0.0` before then —
    /// there's nothing to show a fraction of yet).
    pub fn position_fraction(&self, duration_ms: u32) -> f32 {
        let total_frames = self.total_frames_for(duration_ms);
        if total_frames == 0 {
            return 0.0;
        }
        (self.position_frames.load(Ordering::Acquire) as f32 / total_frames as f32).clamp(0.0, 1.0)
    }

    pub fn elapsed(&self) -> std::time::Duration {
        let sample_rate = self.shared.sample_rate.load(Ordering::Acquire);
        if sample_rate == 0 {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::from_secs_f64(self.position_frames.load(Ordering::Acquire) as f64 / sample_rate as f64)
    }

    /// Jumps playback to `fraction` (0.0..=1.0) of `duration_ms` — see
    /// [`Self::position_fraction`]'s doc comment for why the caller
    /// supplies the file's known duration rather than this type
    /// tracking a total itself. If `fraction` targets a position beyond
    /// what's currently decoded, the `cpal` callback simply plays
    /// silence (or, once `decode_finished`, stops) until the background
    /// decode thread (fed by either the ongoing sequential stream or a
    /// byte-range query response — see `Terminal::
    /// request_audio_byte_range`) produces samples that far; this
    /// method itself never blocks or triggers a query directly.
    pub fn seek_to_fraction(&self, fraction: f32, duration_ms: u32) {
        let total_frames = self.total_frames_for(duration_ms);
        let target = (total_frames as f64 * fraction.clamp(0.0, 1.0) as f64) as u64;
        self.position_frames.store(target, Ordering::Release);
    }

    fn total_frames_for(&self, duration_ms: u32) -> u64 {
        let sample_rate = self.shared.sample_rate.load(Ordering::Acquire);
        if sample_rate == 0 {
            return 0;
        }
        (duration_ms as u64 * sample_rate as u64) / 1000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short (1s, 440Hz tone) FLAC fixture — content doesn't matter for
    /// these tests (they check decode/playback bookkeeping, not audible
    /// correctness), just that it's real, valid FLAC. No FLAC/MP3
    /// *encoder* is pulled into this crate's dependency set (deliberately
    /// decode-only), so this is a checked-in fixture generated via
    /// `ffmpeg`, the same way `rich_content_player`'s GIF tests reuse
    /// `giphy.gif`.
    fn test_flac_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_fixtures/tone.flac")
    }

    /// Opens a player against the WHOLE fixture file already "arrived"
    /// in a freshly seeded `SrvProgressState` (`contiguous_len`/
    /// `total_size` both report the file's real, fixed size) — simulates
    /// the "file fully downloaded" case, the simplest one these
    /// bookkeeping tests need; a live end-to-end test of genuinely
    /// progressive decode-while-streaming lives in `terminal.rs`
    /// (`test_rich_content_audio_placement_decodes_and_plays_via_a_real_
    /// process`), which drives this through a real `somcat` process and
    /// real SRP chunk arrival instead.
    fn open_test_player() -> Option<RichContentAudioPlayer> {
        let path = test_flac_path();
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return None;
        }
        let data = std::fs::read(&path).unwrap();
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let srv_state = Arc::new(crate::rich_content_srv_channel::SrvProgressState::default());
        srv_state.seed_whole_file_for_test(&data, extension);
        let total_size = data.len() as u64;
        let progress = Arc::new(AudioTransferProgress::new());
        progress.update(total_size, total_size);
        RichContentAudioPlayer::open(srv_state, progress, None).ok()
    }

    fn wait_for_decode(player: &mut RichContentAudioPlayer) {
        // Give the background decode thread a moment to at least start
        // producing samples, polling `poll_ready` the same way the real
        // paint path now must (see `poll_ready`'s own doc comment for
        // why `open()` itself no longer waits) — same bounded-retry
        // shape used elsewhere in this codebase for "wait for a
        // background thread to make progress" (see
        // feedback_bounded_test_loops memory).
        for _ in 0..50 {
            if player.poll_ready() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The fixture is a 1-second tone — `1000` ms is what a real
    /// `ContentMetadata::Audio::duration_ms` would carry for it.
    const TEST_FIXTURE_DURATION_MS: u32 = 1000;

    #[test]
    fn player_starts_paused_at_zero_position() {
        let Some(player) = open_test_player() else { return };
        assert!(!player.is_playing());
        assert_eq!(player.position_fraction(TEST_FIXTURE_DURATION_MS), 0.0);
    }

    #[test]
    fn decode_thread_populates_sample_rate_and_channels() {
        let Some(mut player) = open_test_player() else { return };
        wait_for_decode(&mut player);
        assert!(player.shared.sample_rate.load(Ordering::Acquire) > 0);
        assert!(player.shared.channels.load(Ordering::Acquire) > 0);
    }

    #[test]
    fn seek_to_fraction_updates_position_fraction() {
        let Some(mut player) = open_test_player() else { return };
        wait_for_decode(&mut player);
        player.seek_to_fraction(0.5, TEST_FIXTURE_DURATION_MS);
        assert!((player.position_fraction(TEST_FIXTURE_DURATION_MS) - 0.5).abs() < 0.01);
    }

    #[test]
    fn toggle_play_pause_flips_state() {
        let Some(player) = open_test_player() else { return };
        assert!(!player.is_playing());
        player.toggle_play_pause();
        assert!(player.is_playing());
        player.toggle_play_pause();
        assert!(!player.is_playing());
    }

    #[test]
    fn open_returns_immediately_without_a_stream_and_poll_ready_opens_it() {
        // Regression test for the real hang this player used to cause:
        // `open()` itself must never block waiting on the decode thread
        // (confirmed live — it froze the whole GPUI window). `is_ready`
        // must be `false` right after `open()` returns, and only flip
        // to `true` once `poll_ready` is actually called and the decode
        // thread has produced a sample rate/channel count.
        let Some(mut player) = open_test_player() else { return };
        assert!(!player.is_ready());
        wait_for_decode(&mut player);
        assert!(player.is_ready());
    }

    #[test]
    fn decode_eventually_finishes_for_a_fully_present_file() {
        let Some(player) = open_test_player() else { return };
        for _ in 0..100 {
            if player.shared.decode_finished.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("decode never finished for a fully-present small fixture within the poll budget");
    }
}
