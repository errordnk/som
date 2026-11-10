//! A client connection to `somsrv`'s binary side channel (`ConnectionKind::Srv`):
//! the file's bytes go here as `SrvRequest::PutChunk`s, while the placement
//! itself (the placeholder cells) goes through the PTY.
//!
//! [`SrvChannel::connect`] only reaches a daemon that already runs: a program
//! inside a Som tab lives in one of that daemon's sessions, and starting or
//! replacing a daemon would end them. `somsrp`, which may run before any
//! daemon, starts one itself (`somsrv::daemon::connect_or_spawn`) and then
//! uses [`SrvChannel::over`].

use crate::pipe::PipeConnection;
use crate::protocol::{
    ConnectionKind, ContentMetadata, ContentType, CueSheet, HandshakeInfo, NextTrack, RadioEvent, RadioTrack, SrvRequest,
    SrvResponse, TrackTags, daemon_socket_path,
};

/// What arrives on a connection that sends `PutChunk`s: the daemon's own
/// answers (`SrvResponse`) and the requests it forwards from Som verbatim
/// (`SrvRequest::RequestByteRange`, `EndPlayback`, `GrowMarkdownRows`...). The
/// framing is the same, so a client reads either at any time.
pub enum Incoming {
    Response(SrvResponse),
    Request(SrvRequest),
}

impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Incoming::Response(response) => write!(f, "Incoming::Response({response:?})"),
            Incoming::Request(request) => write!(f, "Incoming::Request({request:?})"),
        }
    }
}

pub struct SrvChannel {
    connection: PipeConnection,
    // `PipeConnection::write_message` writes the length and the payload in two
    // calls; two threads sending on one channel (the sequential transfer and the
    // answers to a seek's `RequestByteRange`) would interleave them and break the
    // framing for good. Reads need no lock: one thread reads.
    write_lock: std::sync::Mutex<()>,
}

impl SrvChannel {
    /// Connects to the daemon running on this machine (the test one when
    /// `SOMSRV_INSTANCE` is set), as a client that does not carry a build id.
    pub fn connect() -> Result<Self, String> {
        let address = daemon_socket_path();
        let connection = PipeConnection::connect(&address).map_err(|err| format!("no somsrv at {address}: {err:#}"))?;
        Self::over(connection, HandshakeInfo::new(env!("CARGO_PKG_VERSION"), 0))
    }

    /// Makes a fresh `connection` to the daemon a side channel: tags it `Srv`
    /// and exchanges handshakes.
    pub fn over(connection: PipeConnection, handshake: HandshakeInfo) -> Result<Self, String> {
        ConnectionKind::Srv.write_to(&connection).map_err(|err| format!("failed to tag connection as Srv: {err:#}"))?;
        let channel = Self { connection, write_lock: std::sync::Mutex::new(()) };
        channel.send(&SrvRequest::Handshake(handshake))?;
        match channel.read_incoming()? {
            Incoming::Response(SrvResponse::Handshake(_)) => Ok(channel),
            other => Err(format!("expected Handshake as somsrv's first reply, got {other:?}")),
        }
    }

    pub fn send(&self, message: &SrvRequest) -> Result<(), String> {
        let payload = serde_json::to_vec(message).map_err(|err| format!("failed to encode SrvRequest: {err}"))?;
        let _guard = self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.connection.write_message(&payload).map_err(|err| format!("failed to write to somsrv: {err:#}"))
    }

    /// The next message, whichever of the two kinds it is.
    pub fn read_incoming(&self) -> Result<Incoming, String> {
        let message = self.read_raw()?;
        if let Ok(response) = serde_json::from_slice::<SrvResponse>(&message) {
            return Ok(Incoming::Response(response));
        }
        serde_json::from_slice::<SrvRequest>(&message)
            .map(Incoming::Request)
            .map_err(|err| format!("failed to decode message from somsrv as either SrvResponse or SrvRequest: {err}"))
    }

    /// The next message as it came.
    pub fn read_raw(&self) -> Result<Vec<u8>, String> {
        self.connection.read_message().map_err(|err| format!("failed to read from somsrv: {err:#}"))
    }

    /// Ends the connection; a read blocked on it in another thread returns an error.
    pub fn shutdown(&self) {
        self.connection.shutdown();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put_chunk(
        &self,
        session_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
        total_size: u64,
        content_type: ContentType,
        metadata: ContentMetadata,
    ) -> Result<(), String> {
        self.send(&SrvRequest::PutChunk { session_id, file_id, offset, data, total_size, content_type, metadata })
    }

    /// Som's `RequestByteRange` for this placement come to this connection.
    pub fn register_range_responder(&self, session_id: u32, file_id: u32) -> Result<(), String> {
        self.send(&SrvRequest::RegisterRangeResponder { session_id, file_id })
    }

    /// Som's `GrowMarkdownRows` for this placement come to this connection.
    pub fn register_markdown_grower(&self, session_id: u32, file_id: u32) -> Result<(), String> {
        self.send(&SrvRequest::RegisterMarkdownGrower { session_id, file_id })
    }

    /// The daemon fetches `target` (a URL) itself and streams it as this placement.
    pub fn request_fetch_resource(&self, session_id: u32, file_id: u32, target: String) -> Result<(), String> {
        self.send(&SrvRequest::FetchResource { session_id, file_id, target, base_dir: None })
    }

    /// The next tracks of a radio station, and the batch they belong to.
    pub fn radio_tracks(&self, service: &str, station: &str, queue: Option<&str>) -> Result<(String, Vec<RadioTrack>), String> {
        self.send(&SrvRequest::RadioTracks { service: service.to_string(), station: station.to_string(), queue: queue.map(str::to_string) })?;
        loop {
            match self.read_incoming()? {
                Incoming::Response(SrvResponse::RadioTracks { batch_id, tracks }) => return Ok((batch_id, tracks)),
                Incoming::Response(SrvResponse::RadioFailed { reason }) => return Err(reason),
                _ => continue,
            }
        }
    }

    pub fn radio_feedback(
        &self,
        service: &str,
        station: &str,
        batch_id: &str,
        event: RadioEvent,
        track_id: &str,
        played_seconds: f32,
    ) -> Result<(), String> {
        self.send(&SrvRequest::RadioFeedback {
            service: service.to_string(),
            station: station.to_string(),
            batch_id: batch_id.to_string(),
            event,
            track_id: track_id.to_string(),
            played_seconds,
        })
    }

    pub fn radio_like(&self, service: &str, track_id: &str, liked: bool) -> Result<(), String> {
        self.send(&SrvRequest::RadioLike { service: service.to_string(), track_id: track_id.to_string(), liked })
    }

    pub fn announce_radio_state(
        &self,
        session_id: u32,
        file_id: u32,
        liked: bool,
        track_wave: bool,
        wave_label: Option<String>,
        station: Option<crate::protocol::RadioStation>,
    ) -> Result<(), String> {
        self.send(&SrvRequest::AnnounceRadioState { session_id, file_id, liked, track_wave, wave_label, station })
    }

    pub fn stop_playback(&self, session_id: u32, file_id: u32) -> Result<(), String> {
        self.send(&SrvRequest::StopPlayback { session_id, file_id })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn announce_track(
        &self,
        session_id: u32,
        file_id: u32,
        title: Option<String>,
        index: u32,
        len: u32,
        target: String,
        tags: Option<TrackTags>,
        looping: bool,
        current: Option<NextTrack>,
        next: Option<NextTrack>,
    ) -> Result<(), String> {
        self.send(&SrvRequest::AnnounceTrack { session_id, file_id, title, index, len, target, tags, looping, current, next })
    }

    pub fn announce_cue(&self, session_id: u32, file_id: u32, sheet: CueSheet) -> Result<(), String> {
        self.send(&SrvRequest::AnnounceCue { session_id, file_id, sheet })
    }
}
