//! Links to streaming services (Yandex Music today; Spotify, Tidal, Deezer
//! and the like would be one more module each): what a target - a URL or a
//! `<service>:` name - stands for. Plain string parsing, no network, so
//! `somsrp` (which decides how to play a target) and `somsrv` (which talks
//! to the service) share it.

pub mod yandex;

/// What a link to a service stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkKind {
    /// One track.
    Track,
    /// An endless radio the service picks the tracks of (Yandex Music's
    /// "My Wave"); `station` is the service's own name of it.
    Radio { station: String },
}

/// A target recognized as belonging to a streaming service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLink {
    /// The service's id (`yandex`), as the daemon's registry knows it.
    pub service: &'static str,
    pub kind: LinkKind,
}

/// The service and kind of `target`, or `None` for anything else.
pub fn classify(target: &str) -> Option<ServiceLink> {
    if let Some(station) = yandex::radio_station(target) {
        return Some(ServiceLink { service: yandex::ID, kind: LinkKind::Radio { station } });
    }
    if yandex::parse_track_url(target).is_some() {
        return Some(ServiceLink { service: yandex::ID, kind: LinkKind::Track });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_told_by_what_it_stands_for() {
        let yandex = |kind| Some(ServiceLink { service: "yandex", kind });
        assert_eq!(classify("https://music.yandex.ru/album/9158345/track/10994777"), yandex(LinkKind::Track));
        assert_eq!(classify("https://music.yandex.ru/album/9158345"), None);
        assert_eq!(classify("yandex:wave"), yandex(LinkKind::Radio { station: "user:onyourwave".into() }));
        assert_eq!(classify("https://music.yandex.ru/"), yandex(LinkKind::Radio { station: "user:onyourwave".into() }));
        assert_eq!(classify("https://stream.radioparadise.com/flacm"), None);
        assert_eq!(classify("/home/user/song.mp3"), None);
    }
}
