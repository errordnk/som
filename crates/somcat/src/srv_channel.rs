//! Binary side-channel client — connects to the `som-srv` daemon already
//! running on THIS machine (`som_srv::protocol::daemon_socket_path()`) and
//! streams file bytes as `SrvRequest::PutChunk` messages, replacing the
//! old base91/APC-over-PTY path entirely for this content. See the
//! `rich_content_transport` module doc comment (`crates/terminal`) for why
//! that encoding exists at all on the PTY path — this channel never
//! touches a real ConPTY-backed stdout, so none of that applies here.
//!
//! Deliberately fails hard (returns `Err`, `stream_file`'s caller prints it
//! and exits non-zero) rather than silently falling back to the slower
//! PTY/APC path if `som-srv` isn't reachable — a missing/undeployed daemon
//! is a real setup problem worth surfacing immediately, not a degraded
//! mode worth hiding behind automatic fallback (see this crate's git
//! history for the explicit decision behind this).

use som_srv::pipe::PipeConnection;
use som_srv::protocol::{ConnectionKind, HandshakeInfo, SrvRequest, SrvResponse, daemon_socket_path};

/// A `Srv`-kind connection carries `SrvRequest`s one direction (client ->
/// daemon: `PutChunk`, `SubscribeProgress`, admin commands) and
/// `SrvResponse`s the other (daemon -> client: `Handshake`, `Sessions`,
/// `Progress` pushes) — EXCEPT for `RequestByteRange`, which the daemon
/// forwards to a `PutChunk`-sending connection VERBATIM as a raw
/// `SrvRequest` (see `som_srv::server::forward_srv_request`'s doc
/// comment) — the daemon has no reason to wrap it in a `SrvResponse`
/// variant of its own, since the wire framing (length-prefixed JSON) is
/// identical either way. So a client that sends `PutChunk`s must be
/// ready to read EITHER type off the same connection at any time. This
/// enum is that "either type" — `read_any` tries `SrvResponse` first
/// (the common case for any other request), then `SrvRequest`, since
/// `serde_json` cleanly rejects a value that doesn't match a given
/// enum's shape rather than silently misinterpreting it.
pub enum Incoming {
    Response(SrvResponse),
    Request(SrvRequest),
}

pub struct SrvChannel {
    connection: PipeConnection,
    // Guards `PipeConnection::write_message` specifically — `PutChunk`
    // messages reach this connection from TWO different OS threads: the
    // main sequential-send loop (`stream_file_from_disk`) AND the
    // byte-range-responder thread (`spawn_byte_range_responder_from_disk`,
    // triggered by a real user seek), both calling `put_chunk` on the
    // SAME `Arc<SrvChannel>`. `write_message` itself does two separate
    // `write_all` calls (a 4-byte length prefix, then the payload) with
    // no internal locking — two threads racing here can interleave their
    // length-prefix and payload bytes on the wire, corrupting BOTH
    // messages' framing (and everything sent afterward on this
    // connection, since framing never resynchronizes). Confirmed live as
    // the actual cause of "seeking does nothing" on a video still
    // mid-download: the seek's `RequestByteRange` reply raced the
    // sequential sender's own `PutChunk`s and got silently lost in the
    // corrupted stream. Reads never race (only the responder thread ever
    // reads), so only the write path needs this.
    write_lock: std::sync::Mutex<()>,
}

impl SrvChannel {
    /// Connects to the local daemon and completes its handshake. Returns a
    /// descriptive error (not a bare I/O error) on failure — this is the
    /// message a user actually sees when `som-srv` isn't running, so it
    /// needs to say what's wrong and hint at the fix, not just "connection
    /// refused".
    pub fn connect() -> Result<Self, String> {
        // `som_srv::daemon::connect_or_spawn` handles the common case
        // (daemon not running yet) by spawning it itself, next to this
        // `somcat` binary's own executable — same deploy convention
        // `terminal_view::terminal_panel::som_srv_binary_path` expects
        // Som proper to find it under. Only a genuinely broken deploy
        // (binary missing entirely) surfaces as an error here.
        let daemon_binary = som_srv::daemon::binary_path_next_to_current_exe().map_err(|err| {
            format!(
                "{err:#}\n\
                 (som-srv should be deployed next to somcat's own executable — \
                 check `~/.local/bin/som-srv` exists and is executable)"
            )
        })?;
        let connection = som_srv::daemon::connect_or_spawn(&daemon_binary).map_err(|err| {
            format!("failed to connect to the som-srv daemon at {:?}: {err:#}", daemon_socket_path())
        })?;
        ConnectionKind::Srv.write_to(&connection).map_err(|err| format!("failed to tag connection as Srv: {err:#}"))?;

        send(&connection, &SrvRequest::Handshake(HandshakeInfo::current()))?;
        match read_any(&connection)? {
            Incoming::Response(SrvResponse::Handshake(_)) => {},
            other => return Err(format!("expected Handshake as som-srv's first reply, got {other:?}")),
        }

        Ok(Self { connection, write_lock: std::sync::Mutex::new(()) })
    }

    /// Sends one `PutChunk` — fire-and-forget from this side (no
    /// per-chunk acknowledgement; backpressure comes from the underlying
    /// pipe/socket write itself blocking when the daemon falls behind).
    #[allow(clippy::too_many_arguments)]
    pub fn put_chunk(
        &self,
        session_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
        total_size: u64,
        content_type: som_srv::protocol::ContentType,
        metadata: som_srv::protocol::ContentMetadata,
    ) -> Result<(), String> {
        let _guard = self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        send(&self.connection, &SrvRequest::PutChunk { session_id, file_id, offset, data, total_size, content_type, metadata })
    }

    /// Blocks until the daemon sends something on this connection — either
    /// a forwarded `SrvRequest::RequestByteRange` (see `Incoming`'s own
    /// doc comment) or, in principle, an `SrvResponse` (not expected once
    /// past the initial handshake for a pure `PutChunk` sender, but not
    /// rejected outright either — a future admin-style response arriving
    /// here would just be reported as `Incoming::Response` for the caller
    /// to decide what to do with).
    pub fn read_incoming(&self) -> Result<Incoming, String> {
        read_any(&self.connection)
    }

    /// Registers this connection as the DEDICATED target for `SrvRequest::
    /// RequestByteRange` forwarding for `(session_id, file_id)` — see
    /// `som_srv::protocol::SrvRequest::RegisterRangeResponder`'s own doc
    /// comment for why this needs to be a connection separate from
    /// whichever one is sending the sequential `PutChunk` stream.
    pub fn register_range_responder(&self, session_id: u32, file_id: u32) -> Result<(), String> {
        send(&self.connection, &SrvRequest::RegisterRangeResponder { session_id, file_id })
    }
}

impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Incoming::Response(response) => write!(f, "Incoming::Response({response:?})"),
            Incoming::Request(request) => write!(f, "Incoming::Request({request:?})"),
        }
    }
}

/// Converts `crates/terminal`'s `ContentType` to `som_srv::protocol`'s
/// own separate (but field-for-field identical) copy — see that type's
/// own doc comment for why `som_srv` can't just depend on
/// `crates/terminal` directly (GPUI).
pub fn to_srv_content_type(content_type: terminal::rich_content_transport::ContentType) -> som_srv::protocol::ContentType {
    use terminal::rich_content_transport::ContentType as T;
    match content_type {
        T::Gif => som_srv::protocol::ContentType::Gif,
        T::Audio => som_srv::protocol::ContentType::Audio,
        T::Markdown => som_srv::protocol::ContentType::Markdown,
        T::Video => som_srv::protocol::ContentType::Video,
        T::Jpeg => som_srv::protocol::ContentType::Jpeg,
        T::Png => som_srv::protocol::ContentType::Png,
    }
}

/// Converts `crates/terminal`'s `ContentMetadata` to `som_srv::protocol`'s
/// own separate (but field-for-field identical) copy — see
/// `to_srv_content_type`'s doc comment for why these are two distinct
/// types rather than one shared one.
pub fn to_srv_metadata(metadata: terminal::rich_content_transport::ContentMetadata) -> som_srv::protocol::ContentMetadata {
    use terminal::rich_content_transport::ContentMetadata as M;
    match metadata {
        M::Image { width_px, height_px, color_bits, is_animated } => {
            som_srv::protocol::ContentMetadata::Image { width_px, height_px, color_bits, is_animated }
        },
        M::Audio { sample_rate, channels, bits_per_sample, duration_ms, extension } => {
            som_srv::protocol::ContentMetadata::Audio { sample_rate, channels, bits_per_sample, duration_ms, extension }
        },
        M::Video { width_px, height_px, fps_numerator, fps_denominator, codec, audio_stream_index, subtitle_stream_index, extension } => {
            som_srv::protocol::ContentMetadata::Video {
                width_px,
                height_px,
                fps_numerator,
                fps_denominator,
                codec: to_srv_video_codec(codec),
                audio_stream_index,
                subtitle_stream_index,
                extension,
            }
        },
        M::Markdown => som_srv::protocol::ContentMetadata::Markdown,
    }
}

fn to_srv_video_codec(codec: terminal::rich_content_transport::VideoCodec) -> som_srv::protocol::VideoCodec {
    use terminal::rich_content_transport::VideoCodec as C;
    match codec {
        C::Unknown => som_srv::protocol::VideoCodec::Unknown,
        C::H264 => som_srv::protocol::VideoCodec::H264,
        C::H265 => som_srv::protocol::VideoCodec::H265,
        C::Vp9 => som_srv::protocol::VideoCodec::Vp9,
        C::Av1 => som_srv::protocol::VideoCodec::Av1,
        C::Mpeg4 => som_srv::protocol::VideoCodec::Mpeg4,
    }
}

fn send(connection: &PipeConnection, message: &SrvRequest) -> Result<(), String> {
    let payload = serde_json::to_vec(message).map_err(|err| format!("failed to encode SrvRequest: {err}"))?;
    connection.write_message(&payload).map_err(|err| format!("failed to write to som-srv: {err:#}"))
}

fn read_any(connection: &PipeConnection) -> Result<Incoming, String> {
    let message = connection.read_message().map_err(|err| format!("failed to read from som-srv: {err:#}"))?;
    if let Ok(response) = serde_json::from_slice::<SrvResponse>(&message) {
        return Ok(Incoming::Response(response));
    }
    serde_json::from_slice::<SrvRequest>(&message)
        .map(Incoming::Request)
        .map_err(|err| format!("failed to decode message from som-srv as either SrvResponse or SrvRequest: {err}"))
}
