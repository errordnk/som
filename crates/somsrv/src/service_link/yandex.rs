//! Yandex Music links: `music.yandex.*` pages and the `yandex:` names.

/// The service's id.
pub const ID: &str = "yandex";

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

/// The "wave" station a target asks for: `yandex:wave` and the service's
/// home page are "My Wave"; `yandex:track:<id>` is the wave built from one
/// track.
pub fn radio_station(target: &str) -> Option<String> {
    if target == "yandex:wave" {
        return Some("user:onyourwave".to_string());
    }
    if let Some(id) = target.strip_prefix("yandex:track:") {
        return (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| format!("track:{id}"));
    }
    let path = music_path(target)?;
    (path.is_empty() || path == ["home"]).then(|| "user:onyourwave".to_string())
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
    fn waves_are_asked_for_by_name_or_by_the_home_page() {
        assert_eq!(radio_station("yandex:wave").as_deref(), Some("user:onyourwave"));
        assert_eq!(radio_station("yandex:track:38526").as_deref(), Some("track:38526"));
        assert_eq!(radio_station("https://music.yandex.ru/").as_deref(), Some("user:onyourwave"));
        assert_eq!(radio_station("https://music.yandex.ru/home?x=1").as_deref(), Some("user:onyourwave"));
        assert_eq!(radio_station("https://music.yandex.com").as_deref(), Some("user:onyourwave"));
        assert_eq!(radio_station("yandex:track:"), None);
        assert_eq!(radio_station("yandex:track:../x"), None);
        assert_eq!(radio_station("https://music.yandex.ru/album/1"), None);
        assert_eq!(radio_station("https://example.com/"), None);
    }
}
