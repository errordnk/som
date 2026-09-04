//! Som's side of the `som-srv` binary side-channel: a background OS
//! thread per `(session_id, file_id)` placement, connecting to the local
//! `som-srv` daemon, subscribing to `SrvResponse::Progress` pushes, and
//! forwarding `SrvRequest::RequestByteRange` when playback needs to seek
//! past what's arrived sequentially. Mirrors `rich_content_audio_player`'s
//! own `AudioTransferProgress` shape exactly (a plain `Arc`-shared,
//! atomics-based struct that a background thread writes and the UI
//! thread reads, no GPUI `cx` needed on either side) — see that type's
//! own doc comment for why this shape works without any async
//! executor/channel machinery: `Terminal` (and `RichContentCache`) only
//! ever run on the main/paint thread, so a background thread can't hold
//! a reference into either, and doesn't need to — it only ever touches
//! this shared, independently-owned state.
//!
//! Where this differs from `AudioTransferProgress`: that type only ever
//! carries `contiguous_len`/`total_size` (numbers), because `Terminal`
//! itself already knew `content_type`/`ContentMetadata` before this
//! thread's data ever mattered (they arrived via the OLD APC/`Chunk`
//! parsing, in the SAME UI-thread call that also updated
//! `RichContentCache`). Now that chunks/metadata arrive over `som-srv`
//! instead, `content_type`/`ContentMetadata` themselves have to travel
//! from this background thread to the UI thread too — hence the
//! `Mutex<Option<(ContentType, ContentMetadata)>>` here, checked once per
//! paint pass by `Terminal::sync_rich_content_srv_progress` and applied
//! to `RichContentCache::record_progress` the first time it's non-`None`.

use crate::rich_content_transport::{ContentMetadata, ContentType};
use som_srv::protocol::{ConnectionKind, HandshakeInfo, SrvRequest, SrvResponse};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// How far behind the read position [`SrvProgressState::advance_consumed_
/// up_to`] keeps bytes resident before evicting them — NOT a strictly
/// zero-lookback forward-only buffer. Confirmed live as necessary, not
/// just cautious: FFmpeg's own `avformat_open_input`/`find_stream_info`
/// probe issues BACKWARD seeks within the container's header region
/// while it's still being read forward for the first time (re-parsing
/// atoms/elements after peeking ahead) — a strictly zero-lookback buffer
/// evicted those bytes the instant they were first read, so by the time
/// the prober seeked back to re-read them they were already gone, with
/// no `RequestByteRange` path to recover header bytes that were already
/// part of the initial sequential stream. MP4/AVI containers rely on this
/// backward re-read during probing; MKV's simpler EBML header mostly
/// doesn't, which is why this only surfaced as "MP4/AVI probe fails,
/// unspecified pixel format" and not as an MKV failure too.
///
/// 256MB, not a tighter bound — confirmed live against a real
/// non-faststart MP4 fixture (`moov` sized ~9KB but positioned at the
/// very END of the file, past a ~38MB leading `mdat`) that a 32MB window
/// wasn't enough for: the prober reads forward through the whole `mdat`
/// region hunting for `moov`, then once found at the tail, seeks back to
/// just past the file's start to actually decode — a real movie's `mdat`
/// can be gigabytes, but there is no reliable way to know the true header
/// span up front, so this stays a generous fixed bound rather than
/// something derived from one fixture's specific layout. The whole point
/// of Part 0's redesign is avoiding the OLD failure mode (unbounded disk
/// cache growing to hundreds of GB) — a bound in the hundreds-of-MB range
/// is nowhere near that regime, so there is no reason to fight for a
/// tighter number here.
const CONSUMED_RETENTION_WINDOW: u64 = 256 * 1024 * 1024;

/// Shared, thread-safe state for one `(session_id, file_id)` placement's
/// `som-srv` side-channel connection — see this module's own doc comment
/// for the full reasoning.
#[derive(Default)]
pub struct SrvProgressState {
    contiguous_len: AtomicU64,
    /// See `som_srv::protocol::SrvResponse::Progress::tail_available_from`'s
    /// own doc comment — defaults to 0 (not `total_size`) until the first
    /// `Progress` push arrives, same "nothing known yet" convention
    /// `contiguous_len`'s own 0 default already uses; `total_size()`
    /// being 0 too until then means [`RichContentVideoPlayer`]'s own
    /// tail-availability check (`total_size > 0 && tail_available_from
    /// <= total_size`) can't false-positive on this default pair.
    tail_available_from: AtomicU64,
    /// See `som_srv::protocol::SrvResponse::Progress::pending_ranges`'s
    /// own doc comment — the latest snapshot from the most recent
    /// `Progress` push, overwritten wholesale on each push (not merged
    /// locally) since `som-srv` is the single source of truth for this
    /// list.
    pending_ranges: Mutex<Vec<(u64, u64)>>,
    total_size: AtomicU64,
    /// Set once, the first time a `Progress` push arrives — `Terminal`
    /// takes this value out (`std::mem::take`) the first time it observes
    /// `Some`, seeding `RichContentCache`'s entry for this key; every
    /// later push updates `contiguous_len`/`total_size` above but leaves
    /// this alone; content type/metadata are established once, not
    /// tracked as a live-changing value (see `som_srv::protocol::
    /// SrvRequest::PutChunk`'s own doc comment: a real sender's metadata
    /// never actually changes mid-transfer).
    metadata: Mutex<Option<(ContentType, ContentMetadata)>>,
    stop: AtomicBool,
    /// Set when a `SrvResponse::StopPlayback` push arrives on this
    /// placement's subscription connection — see `SrvRequest::
    /// StopPlayback`'s own doc comment for who sends this and why
    /// (currently only the yazi driver, when its preview cursor moves to
    /// a different file). Checked once per paint pass by `Terminal::
    /// rich_content_audio_placements`/`rich_content_video_placements`,
    /// which tear the player down the same way a click on the widget's
    /// own stop icon already does — this flag is the trigger, not a
    /// player-owned field, so the SAME background thread that already
    /// owns this whole struct can set it without reaching into
    /// `Terminal`'s player maps at all (which live on the paint thread
    /// only).
    stop_playback_requested: AtomicBool,
    /// The actual bytes this placement has received so far, forward-only
    /// — replaces what used to be a full on-disk copy of the source file
    /// (`som-srv`'s `SrvCache` no longer persists chunks to disk at all,
    /// see that module's own doc comment for the incident that motivated
    /// this: a 16GB video's disk-cache copy exhausted real disk space and
    /// the file simply never played). `buffer_start_offset` is the
    /// absolute file offset `buffer`'s FIRST byte corresponds to — bytes
    /// before it have already been consumed (read by `GrowingFileStream`/
    /// the audio decoder's equivalent) and are gone for good; a seek to
    /// an earlier position re-fetches via `SrvRequest::RequestByteRange`
    /// exactly like a seek forward past what's buffered already does,
    /// per the user's explicit direction that this is forward-only, not
    /// a backward-looking window either.
    buffer: Mutex<VecDeque<u8>>,
    buffer_start_offset: AtomicU64,
    /// The offset of the most recent outstanding on-demand `RequestByteRange`
    /// fetch, or `None` if none is in flight — set by `GrowingFileStream::
    /// read` right before it calls `request_byte_range` (and clears the
    /// buffer via `reset_buffer_for_seek`), read by `append_chunk` to let
    /// that fetch's reply land even though it's arriving ahead of the
    /// buffer's own tail. Confirmed live as necessary: without this, a
    /// SEPARATE thread (the sequential streaming/`run()` background
    /// thread) can refill the just-cleared buffer with its own next-in-
    /// order chunk BEFORE the on-demand fetch's reply arrives — by the
    /// time that reply's own `append_chunk` call runs, the buffer is
    /// non-empty again (anchored at the sequential thread's stale
    /// position), so the seek target's bytes get silently dropped as
    /// "ahead of tail" and the reader blocks forever waiting for bytes
    /// that already arrived and were discarded.
    pending_seek_target: Mutex<Option<u64>>,
    /// The source file's own extension (no leading dot), from the FIRST
    /// `Progress` push's `ContentMetadata::Video::extension`/`Audio::
    /// extension` — unlike `metadata` above (a one-shot `Option` that
    /// `Terminal` consumes via `take_metadata`), this is set once and
    /// stays readable for as long as this state lives: `GrowingFileStream`/
    /// the audio decoder's equivalent (`rich_content_video_player`/
    /// `rich_content_audio_player`, both running on their OWN background
    /// decode thread, entirely separate from `Terminal`'s paint-thread
    /// `take_metadata` consumer) need to read it independently, possibly
    /// well after `take_metadata` has already been called once. `None`
    /// until the first push arrives; `Some(String::new())` is a valid,
    /// distinct state from `None` — an empty string is what a sender
    /// that genuinely doesn't know the extension sends, not "not seen
    /// yet."
    extension: Mutex<Option<String>>,
}

impl SrvProgressState {
    pub fn contiguous_len(&self) -> u64 {
        self.contiguous_len.load(Ordering::Acquire)
    }

    /// The source file's own extension, once known — see the `extension`
    /// field's own doc comment for why this is a persistent, re-readable
    /// getter rather than a one-shot `take`. `None` until the first
    /// `Progress` push has arrived (there's nothing to report yet); a
    /// caller that needs to BLOCK until it's known (e.g. `run_decode_
    /// loop`, which needs it before it can even attempt a format probe)
    /// polls this in its own retry-sleep loop, the same pattern already
    /// used for `total_size()`/`contiguous_len()`.
    pub fn extension(&self) -> Option<String> {
        self.extension.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn tail_available_from(&self) -> u64 {
        self.tail_available_from.load(Ordering::Acquire)
    }

    pub fn pending_ranges(&self) -> Vec<(u64, u64)> {
        self.pending_ranges.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn total_size(&self) -> u64 {
        self.total_size.load(Ordering::Acquire)
    }

    /// Copies up to `buf.len()` bytes starting at absolute file `position`
    /// into `buf`, returning how many bytes were actually available —
    /// `0` if `position` isn't covered by the buffer at all (either not
    /// arrived yet, or already evicted by an earlier [`Self::advance_
    /// consumed_up_to`]/[`Self::reset_buffer_for_seek`] call). Never
    /// blocks or waits — the caller ([`crate::rich_content_video_player::
    /// GrowingFileStream`]/the audio decoder's equivalent) already owns
    /// the retry-sleep loop that decides what to do with a `0` result
    /// (request more bytes, wait, or seek).
    pub fn read_buffered(&self, position: u64, buf: &mut [u8]) -> usize {
        let buffer = self.buffer.lock().unwrap_or_else(|p| p.into_inner());
        let start = self.buffer_start_offset.load(Ordering::Acquire);
        if position < start || buf.is_empty() {
            return 0;
        }
        let skip = (position - start) as usize;
        if skip >= buffer.len() {
            return 0;
        }
        let available = (buffer.len() - skip).min(buf.len());
        for (i, byte) in buffer.iter().skip(skip).take(available).enumerate() {
            buf[i] = *byte;
        }
        available
    }

    /// Appends one chunk's bytes at `chunk_offset` — called by [`run`]
    /// for every `SrvResponse::Progress` push, mirroring exactly what
    /// `som-srv`'s own `SrvCache::put_chunk` forwards. Chunks that land
    /// contiguously at (or behind) the buffer's own current tail are
    /// appended normally. A chunk landing AHEAD of the tail is normally
    /// dropped (an ordinary sequential chunk racing ahead of where this
    /// buffer's contiguous run has reached has nowhere correct to go yet
    /// — [`Self::read_buffered`] only ever serves a contiguous run
    /// starting at `buffer_start_offset`) — EXCEPT when `chunk_offset`
    /// matches [`Self::pending_seek_target`]: that's the answer to an
    /// on-demand `RequestByteRange` fetch a reader is actively blocked
    /// waiting on, so it re-anchors the buffer here instead of being
    /// dropped — see `pending_seek_target`'s own doc comment for the race
    /// this closes (a concurrent sequential-arrival chunk refilling a
    /// just-cleared buffer before this reply lands). A brand new (empty)
    /// buffer accepts whatever offset arrives first, becoming that
    /// offset's own starting point.
    fn append_chunk(&self, chunk_offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut buffer = self.buffer.lock().unwrap_or_else(|p| p.into_inner());
        let tail = self.buffer_start_offset.load(Ordering::Acquire) + buffer.len() as u64;
        let is_pending_seek_reply = *self.pending_seek_target.lock().unwrap_or_else(|p| p.into_inner()) == Some(chunk_offset);
        if buffer.is_empty() {
            self.buffer_start_offset.store(chunk_offset, Ordering::Release);
            buffer.extend(data.iter().copied());
        } else if chunk_offset == tail {
            buffer.extend(data.iter().copied());
        } else if is_pending_seek_reply {
            buffer.clear();
            self.buffer_start_offset.store(chunk_offset, Ordering::Release);
            buffer.extend(data.iter().copied());
        } else if chunk_offset > tail {
            // Out-of-order/ahead-of-tail chunk — see this method's own
            // doc comment for why it's dropped rather than stored.
        }
        // `chunk_offset < tail` (a retransmit of already-buffered bytes,
        // or bytes already evicted as consumed) needs no action either
        // way — the buffer already has (or no longer needs) this data.
    }

    /// Drops bytes more than [`CONSUMED_RETENTION_WINDOW`] behind
    /// `position` from the front of the buffer — called by the reader
    /// once it has actually consumed up to `position`, keeping this a
    /// forward-progressing buffer (bounded memory, doesn't grow to the
    /// whole file) without being a strictly zero-lookback one — see
    /// `CONSUMED_RETENTION_WINDOW`'s own doc comment for why a small
    /// trailing window has to survive eviction. Safe to call with a
    /// `position` behind the current `buffer_start_offset` (a no-op) or
    /// ahead of the buffer's tail (clears down to the window).
    pub fn advance_consumed_up_to(&self, position: u64) {
        let mut buffer = self.buffer.lock().unwrap_or_else(|p| p.into_inner());
        let start = self.buffer_start_offset.load(Ordering::Acquire);
        if position <= start {
            return;
        }
        let evict_up_to = position.saturating_sub(CONSUMED_RETENTION_WINDOW);
        if evict_up_to <= start {
            return;
        }
        let drop_count = (evict_up_to - start).min(buffer.len() as u64) as usize;
        buffer.drain(..drop_count);
        self.buffer_start_offset.store(start + drop_count as u64, Ordering::Release);
    }

    /// Test-only convenience: seeds this state with a whole file's worth
    /// of bytes already "arrived" (`contiguous_len`/`total_size` both set
    /// to `data.len()`, the entire buffer populated at offset 0) — lets
    /// `rich_content_video_player`/`rich_content_audio_player`'s own unit
    /// tests exercise a real decoder against a real fixture without
    /// standing up an actual `som-srv` connection, mirroring what
    /// `open_test_player`'s old `std::fs::read`-a-fixture-then-pass-a-
    /// path shape used to do before there was no path to pass anymore.
    /// Test-only convenience: sets just the extension, for a test that
    /// builds up its own buffer via [`Self::append_piece_for_test`]
    /// rather than [`Self::seed_whole_file_for_test`]'s whole-file-at-
    /// once shape.
    #[cfg(test)]
    pub fn set_extension_for_test(&self, extension: &str) {
        *self.extension.lock().unwrap_or_else(|p| p.into_inner()) = Some(extension.to_string());
    }

    #[cfg(test)]
    pub fn seed_whole_file_for_test(&self, data: &[u8], extension: &str) {
        let total_size = data.len() as u64;
        self.total_size.store(total_size, Ordering::Release);
        self.tail_available_from.store(0, Ordering::Release);
        self.append_chunk(0, data);
        self.contiguous_len.store(total_size, Ordering::Release);
        *self.extension.lock().unwrap_or_else(|p| p.into_inner()) = Some(extension.to_string());
    }

    /// Test-only convenience: appends one piece at `chunk_offset`,
    /// updates `total_size`/`contiguous_len` to match, and returns —
    /// lets a test in a DIFFERENT module (e.g. `rich_content_video_
    /// player`'s progressive-write regression test) push bytes in
    /// gradually with real sleeps between pieces, the same "still
    /// arriving" shape a real SRP transfer has, without needing `append_
    /// chunk` itself (private to this module) to be public.
    #[cfg(test)]
    pub fn append_piece_for_test(&self, chunk_offset: u64, data: &[u8], total_size: u64) {
        self.total_size.store(total_size, Ordering::Release);
        self.append_chunk(chunk_offset, data);
        let new_contiguous_len = chunk_offset + data.len() as u64;
        self.contiguous_len.fetch_max(new_contiguous_len, Ordering::AcqRel);
    }

    /// Test-only convenience for a fixture too large to read into memory
    /// all at once (a real multi-GB movie file, used by `rich_content_
    /// video_player`'s own large-file seek-latency regression test) —
    /// spawns a background thread that reads `path` off disk in
    /// `CHUNK_SIZE` pieces and feeds them through the SAME `append_chunk`
    /// call the real `run()` background thread uses, advancing `contiguous_
    /// len` as it goes, mirroring how bytes actually arrive over the wire
    /// in production. The returned `Arc<SrvProgressState>` is immediately
    /// usable (readers block on `contiguous_len`/the buffer exactly as
    /// they would against a real streaming transfer) — callers don't need
    /// to wait for this to finish before opening a decoder against it.
    ///
    /// Backpressured against [`CONSUMED_RETENTION_WINDOW`]: this thread
    /// pauses once the buffer already holds more than the retention
    /// window's worth of bytes the reader hasn't consumed yet, resuming
    /// once `advance_consumed_up_to` (driven by the actual reader) frees
    /// room again. Confirmed live as necessary, not just tidy: a real
    /// disk on this dev machine reads a 16GB file fast enough (page-cache
    /// hit, several GB/s) that an unthrottled version of this loop grew
    /// this test's own process to multiple GB of RAM — nowhere near a
    /// real `som-srv` daemon's behavior (which paces `PutChunk`s to
    /// actual network/read throughput, never blasts a whole file through
    /// as fast as a local disk allows), so an unthrottled test helper was
    /// exercising a code path production never hits.
    #[cfg(test)]
    pub fn spawn_streaming_seed_for_test(path: std::path::PathBuf) -> Arc<Self> {
        const CHUNK_SIZE: usize = 1024 * 1024;
        let state = Arc::new(Self::default());
        let total_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        state.total_size.store(total_size, Ordering::Release);
        state.tail_available_from.store(0, Ordering::Release);
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
        *state.extension.lock().unwrap_or_else(|p| p.into_inner()) = Some(extension);
        let thread_state = state.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let Ok(mut file) = std::fs::File::open(&path) else { return };
            let mut buf = vec![0u8; CHUNK_SIZE];
            let mut offset = 0u64;
            loop {
                let Ok(n) = file.read(&mut buf) else { break };
                if n == 0 {
                    break;
                }
                while offset.saturating_sub(thread_state.buffer_start_offset.load(Ordering::Acquire)) > CONSUMED_RETENTION_WINDOW {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                thread_state.append_chunk(offset, &buf[..n]);
                offset += n as u64;
                thread_state.contiguous_len.store(offset, Ordering::Release);
            }
        });
        state
    }

    /// Test-only counterpart to a real `SrvRequest::RequestByteRange`
    /// answer — reads `len` bytes at `offset` from `path` on a background
    /// thread and feeds them through the SAME `append_chunk` call
    /// `spawn_streaming_seed_for_test`'s sequential reader and the real
    /// `run()` background thread both use. Exists because `GrowingFileStream::
    /// read`'s on-demand `request_byte_range` callback is fire-and-forget
    /// (see `RequestByteRange`'s own doc comment in `rich_content_video_
    /// player.rs`) — a caller can't just return bytes synchronously, it has
    /// to answer by eventually calling `append_chunk` on this same state,
    /// exactly like a real daemon reply would.
    #[cfg(test)]
    pub fn serve_byte_range_from_disk_for_test(self: &Arc<Self>, path: std::path::PathBuf, offset: u64, len: u64) {
        let state = self.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Seek, SeekFrom};
            let Ok(mut file) = std::fs::File::open(&path) else { return };
            if file.seek(SeekFrom::Start(offset)).is_err() {
                return;
            }
            let mut buf = vec![0u8; len as usize];
            let Ok(n) = file.read(&mut buf) else { return };
            state.append_chunk(offset, &buf[..n]);
        });
    }

    /// Clears the buffer entirely and marks `position` as the awaited
    /// `pending_seek_target` — called right before issuing a fresh
    /// `SrvRequest::RequestByteRange` for a seek to a position outside the
    /// currently-buffered range (in either direction); this method itself
    /// only discards whatever's now stale and records what's expected
    /// next, it doesn't request anything itself. Setting `pending_seek_
    /// target` here (not as a separate call) is deliberate — see that
    /// field's own doc comment for the race this closes between this
    /// reset and a concurrent sequential-arrival chunk.
    pub fn reset_buffer_for_seek(&self, position: u64) {
        let mut buffer = self.buffer.lock().unwrap_or_else(|p| p.into_inner());
        buffer.clear();
        *self.pending_seek_target.lock().unwrap_or_else(|p| p.into_inner()) = Some(position);
    }

    /// Takes the pending `(ContentType, ContentMetadata)` out, if any has
    /// arrived since the last call — `None` on every call after the
    /// first (there is only ever one to report, see this struct's own
    /// doc comment).
    pub fn take_metadata(&self) -> Option<(ContentType, ContentMetadata)> {
        self.lock_metadata().take()
    }

    /// Puts metadata back after a failed [`Self::take_metadata`] consumer
    /// — needed because `RichContentCache::record_progress` opens the
    /// `som-srv` cache FILE on disk the first time it sees a given id,
    /// and that file is written by the daemon asynchronously; if `Terminal::
    /// ensure_rich_content_srv_subscription` calls it before the daemon
    /// has created the file yet, `record_progress` fails and — without
    /// this — the metadata `take_metadata` already consumed would be lost
    /// forever (it's a one-shot `Option`), permanently stranding this
    /// placement: no `RichContentCache` entry ever gets created, so
    /// `rich_content_markdown_placements`/equivalents never see it.
    /// Confirmed live: a markdown file whose placeholder grid scrolled
    /// off-screen before the daemon's cache file existed hit exactly this
    /// race and never rendered.
    pub fn restore_metadata(&self, metadata: (ContentType, ContentMetadata)) {
        *self.lock_metadata() = Some(metadata);
    }

    fn lock_metadata(&self) -> MutexGuard<'_, Option<(ContentType, ContentMetadata)>> {
        self.metadata.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Takes (clears) the stop-playback flag — `true` at most once per
    /// `SrvResponse::StopPlayback` push, mirroring `take_metadata`'s own
    /// "consume, don't just peek" shape so the caller's teardown logic
    /// runs exactly once per request, not on every subsequent paint pass.
    pub fn take_stop_playback_requested(&self) -> bool {
        self.stop_playback_requested.swap(false, Ordering::AcqRel)
    }
}

fn to_terminal_content_type(content_type: som_srv::protocol::ContentType) -> ContentType {
    use som_srv::protocol::ContentType as T;
    match content_type {
        T::Gif => ContentType::Gif,
        T::Audio => ContentType::Audio,
        T::Markdown => ContentType::Markdown,
        T::Video => ContentType::Video,
        T::Jpeg => ContentType::Jpeg,
        T::Png => ContentType::Png,
    }
}

fn to_terminal_metadata(metadata: som_srv::protocol::ContentMetadata) -> ContentMetadata {
    use som_srv::protocol::ContentMetadata as M;
    match metadata {
        M::Image { width_px, height_px, color_bits, is_animated } => {
            ContentMetadata::Image { width_px, height_px, color_bits, is_animated }
        },
        M::Audio { sample_rate, channels, bits_per_sample, duration_ms, extension } => {
            ContentMetadata::Audio { sample_rate, channels, bits_per_sample, duration_ms, extension }
        },
        M::Video { width_px, height_px, fps_numerator, fps_denominator, codec, audio_stream_index, subtitle_stream_index, extension } => {
            ContentMetadata::Video {
                width_px,
                height_px,
                fps_numerator,
                fps_denominator,
                codec: to_terminal_video_codec(codec),
                audio_stream_index,
                subtitle_stream_index,
                extension,
            }
        },
        M::Markdown => ContentMetadata::Markdown,
    }
}

/// Pulls the source file's own extension out of `metadata`, if this
/// content type carries one (`Video`/`Audio` — see `ContentMetadata::
/// Video::extension`'s own doc comment) — empty string for every other
/// content type, matching the "empty means unknown" convention `Video`/
/// `Audio`'s own `extension` field already uses for a sender that
/// genuinely doesn't know it.
fn extension_from_metadata(metadata: &som_srv::protocol::ContentMetadata) -> String {
    use som_srv::protocol::ContentMetadata as M;
    match metadata {
        M::Video { extension, .. } | M::Audio { extension, .. } => extension.clone(),
        M::Image { .. } | M::Markdown => String::new(),
    }
}

fn to_terminal_video_codec(codec: som_srv::protocol::VideoCodec) -> crate::rich_content_transport::VideoCodec {
    use crate::rich_content_transport::VideoCodec as C;
    use som_srv::protocol::VideoCodec as T;
    match codec {
        T::Unknown => C::Unknown,
        T::H264 => C::H264,
        T::H265 => C::H265,
        T::Vp9 => C::Vp9,
        T::Av1 => C::Av1,
        T::Mpeg4 => C::Mpeg4,
    }
}

/// Spawns the background thread for one `(session_id, file_id)`
/// placement: connects to the local `som-srv` daemon, sends
/// `SrvRequest::SubscribeProgress`, then loops reading `SrvResponse::
/// Progress` pushes and applying them to `state` for as long as the
/// connection stays open and `state.stop()` hasn't been called.
///
/// Best-effort, same tolerance principle every other rich-content decode
/// path in this codebase already uses: a connection failure (daemon not
/// running, or it goes away mid-transfer) just ends this thread quietly
/// — `state`'s last-known values stay put, and Som already has no
/// stronger guarantee than "no more progress updates for this file" to
/// offer to callers in that case (mirrors what a dropped `Query` on the
/// OLD PTY path already tolerated).
pub fn spawn_progress_listener(session_id: u32, file_id: u32) -> Arc<SrvProgressState> {
    let state = Arc::new(SrvProgressState::default());
    let thread_state = state.clone();
    std::thread::spawn(move || {
        if let Err(err) = run(session_id, file_id, &thread_state) {
            log::debug!("som-srv progress listener for {session_id:#x}:{file_id:#x} ended: {err:#}");
        }
    });
    state
}

/// Sends `SrvRequest::RequestByteRange` on a FRESH, one-shot connection
/// to `som-srv` — deliberately not reusing `spawn_progress_listener`'s
/// long-lived subscription connection, since that connection's whole
/// thread is parked in a blocking read waiting for `Progress` pushes and
/// has no opportunity to also write a request without a second thread's
/// worth of coordination for a request that's sent rarely (only on an
/// explicit seek). `som-srv` accepts `RequestByteRange` on any `Srv`-kind
/// connection, not just the one that sent `SubscribeProgress` (see
/// `server::handle_srv_request`'s match arms — the two aren't tied to
/// the same connection at the protocol level), so a short-lived
/// connection that sends one message and disconnects is enough.
///
/// Best-effort: an error connecting/sending is logged and swallowed, same
/// as the OLD `Query`-based mechanism's identical tolerance for a request
/// that just never gets answered.
///
/// Spawns the actual connect+send onto its own background thread rather
/// than doing it inline — this function is called directly from
/// `Terminal::seek_rich_content_video_playback`/`request_audio_byte_
/// range`, which are themselves called synchronously from GPUI's mouse-
/// click dispatch (`Terminal::mouse_down`), on the SAME thread that pumps
/// the OS message loop and drives every repaint. `PipeConnection::connect`
/// plus the handshake's `read_message`/`write_message` are blocking OS
/// I/O with no timeout — confirmed live as a real bug: clicking a video's
/// seek bar could freeze the ENTIRE window (not just the video, ALL
/// repaints stopped) for minutes at a time, because the click handler
/// itself was blocked inside this call, never returning control to GPUI.
/// Fire-and-forget on a background thread is correct here specifically
/// because this whole mechanism is already "best-effort" by design (the
/// caller has no return value to wait for — the answer arrives later as
/// an ordinary `PutChunk`/`Progress` push on the existing subscription
/// connection, not as a reply to this call).
pub fn request_byte_range(session_id: u32, file_id: u32, offset: u64, len: u64) {
    std::thread::spawn(move || {
        if let Err(err) = try_request_byte_range(session_id, file_id, offset, len) {
            log::debug!("failed to send som-srv RequestByteRange for {session_id:#x}:{file_id:#x}: {err:#}");
        }
    });
}

fn try_request_byte_range(session_id: u32, file_id: u32, offset: u64, len: u64) -> anyhow::Result<()> {
    let connection = connect_and_handshake()?;
    send(&connection, &SrvRequest::RequestByteRange { session_id, file_id, offset, len })?;
    Ok(())
}

/// Sends `SrvRequest::EndPlayback` on a fresh, one-shot connection —
/// Som's own signal (mirroring [`request_byte_range`]'s exact shape and
/// same fire-and-forget-on-a-background-thread reasoning, see that
/// function's own doc comment for why this can't be inline on the
/// caller's thread) that a `(session_id, file_id)` playback has
/// definitively ended: natural EOF, or the widget's own stop icon. The
/// daemon forwards this straight to whichever `somcat` process registered
/// itself as this key's range responder, which reacts by exiting — see
/// `SrvRequest::EndPlayback`'s own doc comment for the full reasoning
/// (`somcat` otherwise has no way to know playback ended and would sit
/// forever holding the terminal in the foreground).
pub fn end_playback(session_id: u32, file_id: u32) {
    std::thread::spawn(move || {
        if let Err(err) = try_end_playback(session_id, file_id) {
            log::debug!("failed to send som-srv EndPlayback for {session_id:#x}:{file_id:#x}: {err:#}");
        }
    });
}

fn try_end_playback(session_id: u32, file_id: u32) -> anyhow::Result<()> {
    let connection = connect_and_handshake()?;
    send(&connection, &SrvRequest::EndPlayback { session_id, file_id })?;
    Ok(())
}

fn run(session_id: u32, file_id: u32, state: &SrvProgressState) -> anyhow::Result<()> {
    let connection = connect_and_handshake()?;
    send(&connection, &SrvRequest::SubscribeProgress { session_id, file_id })?;

    loop {
        if state.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let message = connection.read_message()?;
        let response: SrvResponse = serde_json::from_slice(&message)?;
        match response {
            SrvResponse::Progress { session_id: response_session, file_id: response_file, contiguous_len, tail_available_from, pending_ranges, total_size, content_type, metadata, chunk_offset, chunk_data }
                if response_session == session_id && response_file == file_id =>
            {
                state.contiguous_len.store(contiguous_len, Ordering::Release);
                state.tail_available_from.store(tail_available_from, Ordering::Release);
                *state.pending_ranges.lock().unwrap_or_else(|p| p.into_inner()) = pending_ranges;
                state.total_size.store(total_size, Ordering::Release);
                state.append_chunk(chunk_offset, &chunk_data);
                let mut extension_guard = state.extension.lock().unwrap_or_else(|p| p.into_inner());
                if extension_guard.is_none() {
                    *extension_guard = Some(extension_from_metadata(&metadata));
                }
                drop(extension_guard);
                let mut guard = state.lock_metadata();
                if guard.is_none() {
                    *guard = Some((to_terminal_content_type(content_type), to_terminal_metadata(metadata)));
                }
            },
            SrvResponse::StopPlayback { session_id: response_session, file_id: response_file }
                if response_session == session_id && response_file == file_id =>
            {
                state.stop_playback_requested.store(true, Ordering::Release);
            },
            _ => continue, // unrelated response — ignore, keep waiting
        }
    }
}

fn connect_and_handshake() -> anyhow::Result<som_srv::pipe::PipeConnection> {
    // Same spawn-if-not-running convention every other `som-srv` client
    // in this codebase uses (`somcat::srv_channel::SrvChannel::connect`,
    // `som_srv::relay`'s own RELAY-side connect) — the daemon binary is
    // expected next to Som's own executable (see `som_srv::daemon::
    // binary_path_next_to_current_exe`'s doc comment for why this one
    // function is shared rather than three independent copies).
    let daemon_binary = som_srv::daemon::binary_path_next_to_current_exe()?;
    let connection = som_srv::daemon::connect_or_spawn(&daemon_binary)?;
    ConnectionKind::Srv.write_to(&connection)?;
    send(&connection, &SrvRequest::Handshake(HandshakeInfo::current()))?;
    let message = connection.read_message()?;
    match serde_json::from_slice::<SrvResponse>(&message)? {
        SrvResponse::Handshake(_) => Ok(connection),
        other => anyhow::bail!("expected Handshake as som-srv's first reply, got {other:?}"),
    }
}

fn send(connection: &som_srv::pipe::PipeConnection, message: &SrvRequest) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(message)?;
    connection.write_message(&payload)?;
    Ok(())
}
