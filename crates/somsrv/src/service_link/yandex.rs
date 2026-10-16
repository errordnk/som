//! Yandex Music links: `music.yandex.*` pages and the `yandex:` names.

/// The service's id.
pub const ID: &str = "yandex";

/// The station of "My Wave": the radio built from the account's tastes.
pub const MY_WAVE: &str = "user:onyourwave";

/// The path segments of a `music.yandex.*` URL (query and fragment cut
/// off), or `None` for any other target.
fn music_path(url: &str) -> Option<Vec<&str>> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !host.strip_prefix("www.").unwrap_or(host).starts_with("music.yandex.") {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    Some(path.split('/').filter(|segment| !segment.is_empty()).collect())
}

/// A track a link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRef {
    pub track_id: String,
}

/// `https://music.yandex.ru/album/9158345/track/10994777` or
/// `https://music.yandex.ru/track/10994777` (any `music.yandex.*` host).
pub fn parse_track_url(url: &str) -> Option<TrackRef> {
    let path = music_path(url)?;
    let mut segments = path.into_iter();
    let mut track_id = None;
    while let Some(segment) = segments.next() {
        if segment == "track" {
            track_id = segments.next();
        }
    }
    let track_id = track_id?;
    // Album-qualified ids look like `10994777:9158345` in the API; a link
    // carries them apart and the bare id is enough.
    if track_id.is_empty() || !track_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(TrackRef { track_id: track_id.to_string() })
}

/// Whether a target asks for "My Wave": `yandex:wave` or the service's
/// home page. A session always starts there; the wave of a track is
/// something done within a session, not a way to start one.
pub fn is_my_wave(target: &str) -> bool {
    if target == "yandex:wave" {
        return true;
    }
    music_path(target).is_some_and(|path| path.is_empty() || path == ["home"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_links_are_recognized() {
        let expected = Some(TrackRef { track_id: "10994777".to_string() });
        assert_eq!(parse_track_url("https://music.yandex.ru/album/9158345/track/10994777"), expected);
        assert_eq!(parse_track_url("https://music.yandex.com/track/10994777?utm=1"), expected);
        assert_eq!(parse_track_url("http://www.music.yandex.ru/track/10994777#x"), expected);
    }

    #[test]
    fn other_links_are_not_tracks() {
        assert_eq!(parse_track_url("https://music.yandex.ru/album/9158345"), None);
        assert_eq!(parse_track_url("https://music.yandex.ru/track/abc"), None);
        assert_eq!(parse_track_url("https://example.com/track/1"), None);
        assert_eq!(parse_track_url("/home/user/track/1"), None);
    }

    #[test]
    fn my_wave_is_asked_for_by_name_or_by_the_home_page() {
        assert!(is_my_wave("yandex:wave"));
        assert!(is_my_wave("https://music.yandex.ru/"));
        assert!(is_my_wave("https://music.yandex.ru/home?x=1"));
        assert!(is_my_wave("https://music.yandex.com"));
        assert!(!is_my_wave("yandex:track:38526"));
        assert!(!is_my_wave("https://music.yandex.ru/album/1"));
        assert!(!is_my_wave("https://example.com/"));
    }
}
