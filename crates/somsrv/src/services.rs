//! The streaming services the daemon can play from (Yandex Music today).
//! Each one turns its links into what the player needs - an audio stream,
//! the next tracks of an endless radio - behind one
//! interface, so nothing else in the daemon knows which service it is
//! talking to. A new service is one more implementation here and one more
//! module in `somsrv::service_link` (how its links are recognized).

use somsrv::protocol::{RadioEvent, RadioTrack};
use std::io::Read;

/// What a service knows about a track, for the player's widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub year: Option<u32>,
    pub duration_ms: u32,
    /// Cover link, already sized for a large widget.
    pub cover_url: Option<String>,
}

/// A track opened for playing: its (decrypted) audio, its size (`0` when
/// unknown) and what the service knows about it.
pub struct OpenedTrack {
    pub reader: Box<dyn Read + Send>,
    pub total_size: u64,
    pub info: TrackInfo,
}

pub trait StreamingService: Sync {
    /// Opens the track a link of this service points at.
    fn open_track(&self, url: &str) -> anyhow::Result<OpenedTrack>;

    /// The next batch of the endless radio `station`, continuing after the
    /// track `queue`: the batch's id and its tracks.
    fn radio_tracks(&self, station: &str, queue: Option<&str>) -> anyhow::Result<(String, Vec<RadioTrack>)>;

    /// Tells the radio what the listener did to a track, so it adapts.
    fn radio_feedback(&self, station: &str, batch_id: &str, event: RadioEvent, track_id: &str, played_seconds: f32) -> anyhow::Result<()>;

    /// Likes the track, or (`liked == false`) takes the like back.
    fn set_like(&self, track_id: &str, liked: bool) -> anyhow::Result<()>;
}

/// The service with this id (`somsrv::service_link::track_service`).
pub fn by_id(id: &str) -> Option<&'static dyn StreamingService> {
    match id {
        somsrv::service_link::yandex::ID => Some(&crate::yandex_music::YandexMusic),
        _ => None,
    }
}
