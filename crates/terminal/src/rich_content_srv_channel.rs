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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

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
}

impl SrvProgressState {
    pub fn contiguous_len(&self) -> u64 {
        self.contiguous_len.load(Ordering::Acquire)
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

    /// Takes the pending `(ContentType, ContentMetadata)` out, if any has
    /// arrived since the last call — `None` on every call after the
    /// first (there is only ever one to report, see this struct's own
    /// doc comment).
    pub fn take_metadata(&self) -> Option<(ContentType, ContentMetadata)> {
        self.lock_metadata().take()
    }

    fn lock_metadata(&self) -> MutexGuard<'_, Option<(ContentType, ContentMetadata)>> {
        self.metadata.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
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
        M::Audio { sample_rate, channels, bits_per_sample, duration_ms } => {
            ContentMetadata::Audio { sample_rate, channels, bits_per_sample, duration_ms }
        },
        M::Video { width_px, height_px, fps_numerator, fps_denominator, codec, audio_stream_index, subtitle_stream_index } => {
            ContentMetadata::Video {
                width_px,
                height_px,
                fps_numerator,
                fps_denominator,
                codec: to_terminal_video_codec(codec),
                audio_stream_index,
                subtitle_stream_index,
            }
        },
        M::Markdown => ContentMetadata::Markdown,
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
            SrvResponse::Progress { session_id: response_session, file_id: response_file, contiguous_len, tail_available_from, pending_ranges, total_size, content_type, metadata }
                if response_session == session_id && response_file == file_id =>
            {
                state.contiguous_len.store(contiguous_len, Ordering::Release);
                state.tail_available_from.store(tail_available_from, Ordering::Release);
                *state.pending_ranges.lock().unwrap_or_else(|p| p.into_inner()) = pending_ranges;
                state.total_size.store(total_size, Ordering::Release);
                let mut guard = state.lock_metadata();
                if guard.is_none() {
                    *guard = Some((to_terminal_content_type(content_type), to_terminal_metadata(metadata)));
                }
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
