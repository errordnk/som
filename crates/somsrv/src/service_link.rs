//! Links to streaming services (Yandex Music today; Spotify, Tidal, Deezer
//! and the like would be one more module each): which service a track link
//! belongs to. Plain string parsing, no network, so
//! `somsrp` (which decides how to play a target) and `somsrv` (which talks
//! to the service) share it.

pub mod yandex;

/// The id of the service a track link belongs to, or `None` for any other
/// target. (A radio is never named by a link: a session starts from a
/// playlist file whose line names the service.)
pub fn track_service(target: &str) -> Option<&'static str> {
    yandex::parse_track_url(target).map(|_| yandex::ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_link_is_told_from_any_other_target() {
        assert_eq!(track_service("https://music.yandex.ru/album/9158345/track/10994777"), Some("yandex"));
        assert_eq!(track_service("https://music.yandex.ru/album/9158345"), None);
        assert_eq!(track_service("yandex:wave"), None);
        assert_eq!(track_service("https://music.yandex.ru/"), None);
        assert_eq!(track_service("https://stream.radioparadise.com/flacm"), None);
        assert_eq!(track_service("/home/user/song.mp3"), None);
    }
}
