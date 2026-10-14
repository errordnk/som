//! Live "now playing" metadata for an internet-radio stream fetched by
//! `somsrv` (`SrvRequest::FetchResource`): artist, title, album, year,
//! how long the current track still runs, and where its cover art is.
//!
//! A radio stream itself carries almost none of this (ICY gives one
//! `Artist - Title` string, Ogg-FLAC nothing at connect time), so a
//! station is recognized by its URL and asked through its own public API.
//! Only Radio Paradise is wired up; [`find_cover`] is the station-
//! independent cover lookup (MusicBrainz / Cover Art Archive for an older
//! album, Deezer for a new release) used when a station's own API gives
//! no cover.
//!
//! Everything here is plain HTTP + JSON through the small [`HttpGet`]
//! trait so the parsing and the source-selection rules are unit-tested
//! without a network.

use crate::srv_cache::SrvCache;
use somsrv::protocol::{NowPlayingInfo, TrackTags};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often a station's API is asked what is playing. The remaining time
/// it reports is interpolated by the consumer between two answers.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// MusicBrainz asks API clients to identify themselves and stay at about
/// one request per second; a lookup here needs one search call.
const USER_AGENT: &str = concat!("Som/", env!("CARGO_PKG_VERSION"), " (https://github.com/errordnk/som)");

/// The widest cover that is ever shown; a source's own URL is asked for
/// this size where it lets the caller choose.
const COVER_PIXELS: u32 = 600;

/// Minimal blocking HTTP surface used by every lookup in this module.
pub trait HttpGet {
    /// Body of a successful (2xx) GET as text.
    fn get_text(&self, url: &str) -> anyhow::Result<String>;
    /// `true` if a GET of `url` (redirects followed) ends in a 2xx.
    fn exists(&self, url: &str) -> bool;
}

/// [`HttpGet`] over `ureq` with short timeouts - a slow metadata server
/// must never stall the audio fetch running next to it.
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .user_agent(USER_AGENT)
            .build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }
}

impl HttpGet for UreqHttp {
    fn get_text(&self, url: &str) -> anyhow::Result<String> {
        let mut response = self.agent.get(url).call()?;
        Ok(response.body_mut().read_to_string()?)
    }

    fn exists(&self, url: &str) -> bool {
        self.agent.get(url).call().is_ok()
    }
}

/// A station `somsrv` knows how to ask for what is playing.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Station {
    /// Radio Paradise; `chan` is the channel number of its public API.
    RadioParadise { chan: u8 },
}

/// Recognizes a station from a stream URL, or `None` for any other.
pub fn station_for_url(url: &str) -> Option<Station> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;
    if !host.eq_ignore_ascii_case("stream.radioparadise.com") {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    // The stream name starts with the mix: `mellow-flac`, `rock-320`,
    // `global-flac`/`world-etc-flac`, `beyond-flac`; the main mix has
    // none (`flac`, `320`, `aac-128`).
    let chan = if path.starts_with("mellow") {
        1
    } else if path.starts_with("rock") {
        2
    } else if path.starts_with("global") || path.starts_with("world-etc") {
        3
    } else if path.starts_with("beyond") {
        4
    } else {
        0
    };
    Some(Station::RadioParadise { chan })
}

/// What Radio Paradise's `now_playing` call said.
#[derive(Debug, PartialEq, Eq)]
struct RadioParadiseNow {
    artist: String,
    title: String,
    album: String,
    year: Option<u32>,
    cover: Option<String>,
    remaining_secs: u32,
}

fn parse_radio_paradise_now(body: &str) -> anyhow::Result<RadioParadiseNow> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    let text = |key: &str| value.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let title = text("title");
    if title.is_empty() {
        anyhow::bail!("now_playing carries no title");
    }
    let cover = Some(text("cover")).filter(|c| !c.is_empty());
    let remaining_secs = value.get("time").and_then(|v| v.as_u64()).unwrap_or(0).min(u32::MAX as u64) as u32;
    Ok(RadioParadiseNow {
        artist: text("artist"),
        title,
        album: text("album"),
        year: text("year").parse().ok(),
        cover,
        remaining_secs,
    })
}

/// Length in milliseconds of the track at the head of Radio Paradise's
/// `nowplaying_list_v2022` answer (the one playing now).
fn parse_radio_paradise_duration_ms(body: &str) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let duration = value.get("song")?.get(0)?.get("duration")?;
    let ms = match duration {
        serde_json::Value::String(s) => s.parse::<u64>().ok()?,
        other => other.as_u64()?,
    };
    u32::try_from(ms).ok()
}

fn query_radio_paradise(http: &dyn HttpGet, chan: u8) -> anyhow::Result<RadioParadiseNow> {
    parse_radio_paradise_now(&http.get_text(&format!("https://api.radioparadise.com/api/now_playing?chan={chan}"))?)
}

/// The cover found for the album last looked up, so the five-second poll
/// does not repeat the search while the same album plays.
#[derive(Default)]
struct CoverCache {
    album: Option<(String, String)>,
    url: Option<String>,
}

/// One full answer for a Radio Paradise channel, as a ready
/// [`NowPlayingInfo`] for `(session_id, file_id)`.
fn radio_paradise_info(
    http: &dyn HttpGet,
    chan: u8,
    session_id: u32,
    file_id: u32,
    current_year: u32,
    covers: &mut CoverCache,
) -> anyhow::Result<NowPlayingInfo> {
    let now = query_radio_paradise(http, chan)?;
    // The track length is a separate call; without it only the title
    // lines can be shown, which is still worth having.
    let duration_ms = http
        .get_text(&format!("https://api.radioparadise.com/api/nowplaying_list_v2022?chan={chan}&list_num=2"))
        .ok()
        .and_then(|body| parse_radio_paradise_duration_ms(&body))
        .unwrap_or(0);
    // The station's own picture is a centre-cropped square of the
    // artwork, so the full cover from MusicBrainz / Deezer is preferred
    // and the station's is the fallback.
    let album_key = (now.artist.clone(), now.album.clone());
    let cover_url = if covers.album.as_ref() == Some(&album_key) {
        covers.url.clone()
    } else {
        let found = find_cover(http, &now.artist, &now.album, now.year, current_year).or(now.cover.clone());
        covers.album = Some(album_key);
        covers.url = found.clone();
        found
    };
    Ok(NowPlayingInfo {
        session_id,
        file_id,
        artist: now.artist,
        title: now.title,
        album: now.album,
        year: now.year,
        duration_ms,
        remaining_ms: now.remaining_secs.saturating_mul(1000),
        cover_url,
        cover_lookup_pending: false,
    })
}

/// Starts the background poll for `station`, publishing every answer
/// through `cache.set_now_playing` until the fetch is cancelled or
/// `done` is set (the stream ended).
pub fn spawn_poller(cache: SrvCache, session_id: u32, file_id: u32, station: Station, done: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let http = UreqHttp::new();
        let Station::RadioParadise { chan } = station;
        let mut last: Option<(String, String)> = None;
        let mut covers = CoverCache::default();
        while !done.load(Ordering::Acquire) && !cache.is_fetch_cancelled(session_id, file_id) {
            match radio_paradise_info(&http, chan, session_id, file_id, current_year(), &mut covers) {
                Ok(info) => {
                    let track = (info.artist.clone(), info.title.clone());
                    if last.as_ref() != Some(&track) {
                        log::info!("now playing on channel {chan}: {} - {}", info.artist, info.title);
                        last = Some(track);
                    }
                    cache.set_now_playing(info);
                },
                Err(err) => log::warn!("now-playing lookup for channel {chan} failed: {err:#}"),
            }
            // Sleep in short steps so a stop is noticed promptly.
            let mut slept = Duration::ZERO;
            while slept < POLL_INTERVAL
                && !done.load(Ordering::Acquire)
                && !cache.is_fetch_cancelled(session_id, file_id)
            {
                std::thread::sleep(Duration::from_millis(100));
                slept += Duration::from_millis(100);
            }
        }
    });
}

fn current_year() -> u32 {
    // Days since the epoch -> civil year (Howard Hinnant's algorithm);
    // exact without a calendar dependency.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (year + i64::from(month <= 2)) as u32
}

// ---------------------------------------------------------------------
// Local files
// ---------------------------------------------------------------------

/// What the widget shows for a local file with `tags`: its own tags, no
/// station clock (the player's own length and position are used), and a
/// cover that is `pending` until the lookup answers.
fn file_info(session_id: u32, file_id: u32, tags: &TrackTags, cover_url: Option<String>, pending: bool) -> NowPlayingInfo {
    NowPlayingInfo {
        session_id,
        file_id,
        artist: tags.artist.clone(),
        title: tags.title.clone(),
        album: tags.album.clone(),
        year: tags.year,
        duration_ms: 0,
        remaining_ms: 0,
        cover_url,
        cover_lookup_pending: pending,
    }
}

/// The finished answer for a local file: its tags and the cover the
/// chain ([`find_cover`]) found by artist and album, if any.
fn resolve_file_info(
    http: &dyn HttpGet,
    session_id: u32,
    file_id: u32,
    tags: &TrackTags,
    current_year: u32,
) -> NowPlayingInfo {
    let cover = find_cover(http, &tags.artist, &tags.album, tags.year, current_year);
    file_info(session_id, file_id, tags, cover, false)
}

/// Shows a local file's `tags` at once and fills its cover in as soon as
/// the lookup (MusicBrainz / Cover Art Archive, Deezer, iTunes) answers.
pub fn announce_file_tags(cache: SrvCache, session_id: u32, file_id: u32, tags: TrackTags) {
    cache.set_now_playing(file_info(session_id, file_id, &tags, None, true));
    std::thread::spawn(move || {
        let info = resolve_file_info(&UreqHttp::new(), session_id, file_id, &tags, current_year());
        cache.set_now_playing(info);
    });
}

// ---------------------------------------------------------------------
// Any station: the title in the stream itself (ICY)
// ---------------------------------------------------------------------

/// `("Artist", "Title")` from the `Artist - Title` convention radio uses
/// for a stream title; with no ` - ` the whole text is the title.
pub fn split_stream_title(raw: &str) -> (String, String) {
    match raw.split_once(" - ") {
        Some((artist, title)) => (artist.trim().to_string(), title.trim().to_string()),
        None => (String::new(), raw.trim().to_string()),
    }
}

fn stream_info(
    session_id: u32,
    file_id: u32,
    artist: &str,
    title: &str,
    album: &str,
    cover_url: Option<String>,
    pending: bool,
) -> NowPlayingInfo {
    NowPlayingInfo {
        session_id,
        file_id,
        artist: artist.to_string(),
        title: title.to_string(),
        album: album.to_string(),
        year: None,
        duration_ms: 0,
        remaining_ms: 0,
        cover_url,
        cover_lookup_pending: pending,
    }
}

/// The finished answer for a streamed track: its artist and title and,
/// when a catalogue knows the track, the album and cover.
fn resolve_stream_info(http: &dyn HttpGet, session_id: u32, file_id: u32, artist: &str, title: &str) -> NowPlayingInfo {
    match find_track_cover(http, artist, title) {
        Some((album, cover)) => stream_info(session_id, file_id, artist, title, &album, Some(cover), false),
        None => stream_info(session_id, file_id, artist, title, "", None, false),
    }
}

/// The newest stream title announced per placement, so a slow lookup of an
/// older title never overwrites the answer for a newer one.
static LATEST_STREAM_TITLE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<(u32, u32), String>>> =
    std::sync::OnceLock::new();

/// Shows a station's new stream title at once and fills the album and
/// cover in when the lookup answers.
pub fn announce_stream_title(cache: &SrvCache, session_id: u32, file_id: u32, raw: &str) {
    let latest = LATEST_STREAM_TITLE.get_or_init(Default::default);
    latest.lock().unwrap_or_else(|p| p.into_inner()).insert((session_id, file_id), raw.to_string());

    let (artist, title) = split_stream_title(raw);
    log::info!("stream title for {session_id:08x}/{file_id:08x}: {artist:?} - {title:?}");
    cache.set_now_playing(stream_info(session_id, file_id, &artist, &title, "", None, !artist.is_empty()));
    if artist.is_empty() {
        return;
    }
    let cache = cache.clone();
    let raw = raw.to_string();
    std::thread::spawn(move || {
        let info = resolve_stream_info(&UreqHttp::new(), session_id, file_id, &artist, &title);
        let still_current = LATEST_STREAM_TITLE
            .get()
            .and_then(|latest| latest.lock().unwrap_or_else(|p| p.into_inner()).get(&(session_id, file_id)).cloned())
            .is_some_and(|newest| newest == raw);
        if still_current {
            cache.set_now_playing(info);
        }
    });
}

/// Looks a track up by artist and title - Deezer first, iTunes second -
/// returning `(album, cover)`. A stream title carries no album, so the
/// catalogue's own track entry supplies it.
fn find_track_cover(http: &dyn HttpGet, artist: &str, title: &str) -> Option<(String, String)> {
    if artist.trim().is_empty() || title.trim().is_empty() {
        return None;
    }
    deezer_track(http, artist, title).or_else(|| itunes_track(http, artist, title))
}

fn deezer_track(http: &dyn HttpGet, artist: &str, title: &str) -> Option<(String, String)> {
    let query = format!("artist:\"{}\" track:\"{}\"", artist.replace('"', ""), title.replace('"', ""));
    let url = format!("https://api.deezer.com/search?q={}&limit=5", encode_component(&query));
    let body = http.get_text(&url).ok()?;
    deezer_track_from_search(&body, artist, title)
}

fn deezer_track_from_search(body: &str, artist: &str, title: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for hit in value.get("data")?.as_array()? {
        let hit_title = hit.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let hit_artist = hit.get("artist").and_then(|a| a.get("name")).and_then(|v| v.as_str()).unwrap_or("");
        if !names_match(hit_title, title) || !names_match(hit_artist, artist) {
            continue;
        }
        let album = hit.get("album")?;
        let cover = album.get("cover_xl").and_then(|v| v.as_str())?;
        let album_title = album.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        return Some((album_title, cover.replace("1000x1000", &format!("{COVER_PIXELS}x{COVER_PIXELS}"))));
    }
    None
}

fn itunes_track(http: &dyn HttpGet, artist: &str, title: &str) -> Option<(String, String)> {
    let term = format!("{artist} {title}");
    let url = format!(
        "https://itunes.apple.com/search?term={}&media=music&entity=song&limit=5",
        encode_component(&term)
    );
    let body = http.get_text(&url).ok()?;
    itunes_track_from_search(&body, artist, title)
}

fn itunes_track_from_search(body: &str, artist: &str, title: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for hit in value.get("results")?.as_array()? {
        let hit_title = hit.get("trackName").and_then(|v| v.as_str()).unwrap_or("");
        let hit_artist = hit.get("artistName").and_then(|v| v.as_str()).unwrap_or("");
        if !names_match(hit_title, title) || !names_match(hit_artist, artist) {
            continue;
        }
        let small = hit.get("artworkUrl100").and_then(|v| v.as_str())?;
        let album = hit.get("collectionName").and_then(|v| v.as_str()).unwrap_or("").to_string();
        return Some((album, small.replace("100x100bb", &format!("{COVER_PIXELS}x{COVER_PIXELS}bb"))));
    }
    None
}

// ---------------------------------------------------------------------
// Cover lookup
// ---------------------------------------------------------------------

/// Looks a cover up by artist and album. An older album is searched in
/// MusicBrainz (cover from the Cover Art Archive) first, then Deezer, then
/// iTunes; a new release (this or last year) goes to Deezer and iTunes
/// first, since the archive rarely has it yet. `None` if none has it.
pub fn find_cover(http: &dyn HttpGet, artist: &str, album: &str, year: Option<u32>, current_year: u32) -> Option<String> {
    if album.trim().is_empty() {
        return None;
    }
    let is_new_release = year.is_some_and(|y| y + 1 >= current_year);
    if is_new_release {
        deezer_cover(http, artist, album)
            .or_else(|| itunes_cover(http, artist, album))
            .or_else(|| musicbrainz_cover(http, artist, album))
    } else {
        musicbrainz_cover(http, artist, album)
            .or_else(|| deezer_cover(http, artist, album))
            .or_else(|| itunes_cover(http, artist, album))
    }
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn encode_component(text: &str) -> String {
    const SET: &percent_encoding::AsciiSet =
        &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
    percent_encoding::utf8_percent_encode(text, SET).to_string()
}

/// Loose equality of two names: case, punctuation and spacing ignored,
/// and one containing the other counts (`"Album"` vs `"Album (Deluxe)"`).
fn names_match(a: &str, b: &str) -> bool {
    fn normalize(s: &str) -> String {
        s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
    }
    let (a, b) = (normalize(a), normalize(b));
    !a.is_empty() && !b.is_empty() && (a == b || a.contains(&b) || b.contains(&a))
}

fn musicbrainz_cover(http: &dyn HttpGet, artist: &str, album: &str) -> Option<String> {
    let artist = artist.replace('"', "");
    let album = album.replace('"', "");
    let release_query = format!("artist:\"{artist}\" AND release:\"{album}\"");
    let url = format!("https://musicbrainz.org/ws/2/release/?query={}&fmt=json&limit=5", encode_component(&release_query));
    if let Some(cover) =
        http.get_text(&url).ok().and_then(|body| musicbrainz_cover_from_search(http, &body, "releases", "release", &album))
    {
        return Some(cover);
    }
    // No release of that title has a cover, but the album as a whole
    // (a release group: all its editions) may.
    let group_query = format!("artist:\"{artist}\" AND releasegroup:\"{album}\"");
    let url = format!(
        "https://musicbrainz.org/ws/2/release-group/?query={}&fmt=json&limit=5",
        encode_component(&group_query)
    );
    let body = http.get_text(&url).ok()?;
    musicbrainz_cover_from_search(http, &body, "release-groups", "release-group", &album)
}

/// From a MusicBrainz search answer (`list_key` is `releases` or
/// `release-groups`, `kind` the matching Cover Art Archive path segment),
/// the first entry whose title matches `album` and that actually has a
/// front cover in the Cover Art Archive (many have none).
fn musicbrainz_cover_from_search(
    http: &dyn HttpGet,
    body: &str,
    list_key: &str,
    kind: &str,
    album: &str,
) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for release in value.get(list_key)?.as_array()? {
        let (Some(id), Some(title)) = (
            release.get("id").and_then(|v| v.as_str()),
            release.get("title").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        if !names_match(title, album) {
            continue;
        }
        // 500 px is the largest archive thumbnail not above the 600 px
        // cap that is always generated.
        let cover = format!("https://coverartarchive.org/{kind}/{id}/front-500");
        if http.exists(&cover) {
            return Some(cover);
        }
    }
    None
}

fn deezer_cover(http: &dyn HttpGet, artist: &str, album: &str) -> Option<String> {
    let query = format!("artist:\"{}\" album:\"{}\"", artist.replace('"', ""), album.replace('"', ""));
    let url = format!("https://api.deezer.com/search/album?q={}&limit=5", encode_component(&query));
    let body = http.get_text(&url).ok()?;
    deezer_cover_from_search(&body, artist, album)
}

fn deezer_cover_from_search(body: &str, artist: &str, album: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for hit in value.get("data")?.as_array()? {
        let title = hit.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let hit_artist = hit.get("artist").and_then(|a| a.get("name")).and_then(|v| v.as_str()).unwrap_or("");
        if !names_match(title, album) || !names_match(hit_artist, artist) {
            continue;
        }
        let cover = hit.get("cover_xl").and_then(|v| v.as_str())?;
        // Deezer's image CDN takes the size in the path.
        return Some(cover.replace("1000x1000", &format!("{COVER_PIXELS}x{COVER_PIXELS}")));
    }
    None
}

fn itunes_cover(http: &dyn HttpGet, artist: &str, album: &str) -> Option<String> {
    let term = format!("{artist} {album}");
    let url = format!(
        "https://itunes.apple.com/search?term={}&media=music&entity=album&limit=5",
        encode_component(&term)
    );
    let body = http.get_text(&url).ok()?;
    itunes_cover_from_search(&body, artist, album)
}

fn itunes_cover_from_search(body: &str, artist: &str, album: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for hit in value.get("results")?.as_array()? {
        let title = hit.get("collectionName").and_then(|v| v.as_str()).unwrap_or("");
        let hit_artist = hit.get("artistName").and_then(|v| v.as_str()).unwrap_or("");
        if !names_match(title, album) || !names_match(hit_artist, artist) {
            continue;
        }
        let small = hit.get("artworkUrl100").and_then(|v| v.as_str())?;
        // The artwork CDN takes the size in the file name.
        return Some(small.replace("100x100bb", &format!("{COVER_PIXELS}x{COVER_PIXELS}bb")));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Canned answers keyed by a substring of the URL; records every URL
    /// asked so a test can assert which sources were (not) consulted.
    struct Canned {
        answers: HashMap<&'static str, String>,
        existing: Vec<&'static str>,
        asked: Mutex<Vec<String>>,
    }

    impl Canned {
        fn new() -> Self {
            Self { answers: HashMap::new(), existing: Vec::new(), asked: Mutex::new(Vec::new()) }
        }
    }

    impl HttpGet for Canned {
        fn get_text(&self, url: &str) -> anyhow::Result<String> {
            self.asked.lock().unwrap().push(url.to_string());
            for (needle, body) in &self.answers {
                if url.contains(needle) {
                    return Ok(body.clone());
                }
            }
            anyhow::bail!("no canned answer for {url}")
        }

        fn exists(&self, url: &str) -> bool {
            self.asked.lock().unwrap().push(url.to_string());
            self.existing.iter().any(|needle| url.contains(needle))
        }
    }

    const RP_NOW: &str = r#"{"time":546,"artist":"MonoMono","title":"Tire Loma Da Nigbehin","album":"Dawn of Awareness","year":"2011","cover":"https:\/\/img.radioparadise.com\/covers\/l\/19593.jpg","cover_med":"x"}"#;
    const RP_LIST: &str = r#"{"song":[{"event":"1","duration":"889455","artist":"MonoMono"},{"event":"0","duration":"100"}]}"#;

    #[test]
    fn radio_paradise_streams_map_to_their_channel() {
        let chan = |url| match station_for_url(url) {
            Some(Station::RadioParadise { chan }) => Some(chan),
            None => None,
        };
        assert_eq!(chan("https://stream.radioparadise.com/world-etc-flac"), Some(3));
        assert_eq!(chan("https://stream.radioparadise.com/global-320"), Some(3));
        assert_eq!(chan("https://stream.radioparadise.com/mellow-flac"), Some(1));
        assert_eq!(chan("http://stream.radioparadise.com/rock-192?x=1"), Some(2));
        assert_eq!(chan("https://stream.radioparadise.com/beyond-flac"), Some(4));
        assert_eq!(chan("https://stream.radioparadise.com/flac"), Some(0));
        assert_eq!(chan("https://stream.radioparadise.com/flacm"), Some(0));
        assert_eq!(chan("https://stream.radioparadise.com/world-etc-flacm"), Some(3));
        assert_eq!(station_for_url("https://example.com/world-etc-flac"), None);
        assert_eq!(station_for_url("not a url"), None);
    }

    #[test]
    fn radio_paradise_now_playing_is_parsed() {
        let now = parse_radio_paradise_now(RP_NOW).unwrap();
        assert_eq!(now.artist, "MonoMono");
        assert_eq!(now.title, "Tire Loma Da Nigbehin");
        assert_eq!(now.album, "Dawn of Awareness");
        assert_eq!(now.year, Some(2011));
        assert_eq!(now.remaining_secs, 546);
        assert_eq!(now.cover.as_deref(), Some("https://img.radioparadise.com/covers/l/19593.jpg"));
    }

    #[test]
    fn answer_without_a_title_is_rejected() {
        assert!(parse_radio_paradise_now(r#"{"time":1,"artist":"x"}"#).is_err());
        assert!(parse_radio_paradise_now("not json").is_err());
    }

    #[test]
    fn duration_is_read_from_the_head_of_the_list() {
        assert_eq!(parse_radio_paradise_duration_ms(RP_LIST), Some(889_455));
        assert_eq!(parse_radio_paradise_duration_ms(r#"{"song":[{"duration":5000}]}"#), Some(5_000));
        assert_eq!(parse_radio_paradise_duration_ms(r#"{"song":[]}"#), None);
    }

    #[test]
    fn full_radio_paradise_answer_combines_both_calls() {
        let mut http = Canned::new();
        http.answers.insert("now_playing", RP_NOW.to_string());
        http.answers.insert("nowplaying_list_v2022", RP_LIST.to_string());

        let info = radio_paradise_info(&http, 3, 7, 9, 2026, &mut CoverCache::default()).unwrap();

        assert_eq!((info.session_id, info.file_id), (7, 9));
        assert_eq!(info.duration_ms, 889_455);
        assert_eq!(info.remaining_ms, 546_000);
        // Nothing found elsewhere (no canned answer): the station's own.
        assert_eq!(info.cover_url.as_deref(), Some("https://img.radioparadise.com/covers/l/19593.jpg"));
    }

    #[test]
    fn full_cover_from_the_archive_beats_the_stations_cropped_one() {
        let mut http = Canned::new();
        http.answers.insert("now_playing", RP_NOW.to_string());
        http.answers.insert("nowplaying_list_v2022", RP_LIST.to_string());
        http.answers.insert("musicbrainz.org", r#"{"releases":[{"id":"abc","title":"Dawn of Awareness"}]}"#.to_string());
        http.existing.push("release/abc/front-500");

        let info = radio_paradise_info(&http, 3, 1, 2, 2026, &mut CoverCache::default()).unwrap();

        assert_eq!(info.cover_url.as_deref(), Some("https://coverartarchive.org/release/abc/front-500"));
    }

    #[test]
    fn the_cover_is_not_searched_again_for_the_same_album() {
        let mut http = Canned::new();
        http.answers.insert("now_playing", RP_NOW.to_string());
        http.answers.insert("nowplaying_list_v2022", RP_LIST.to_string());
        let mut covers = CoverCache::default();

        radio_paradise_info(&http, 3, 1, 2, 2026, &mut covers).unwrap();
        let searches_after_first =
            http.asked.lock().unwrap().iter().filter(|u| u.contains("musicbrainz") || u.contains("deezer")).count();
        radio_paradise_info(&http, 3, 1, 2, 2026, &mut covers).unwrap();
        let searches_after_second =
            http.asked.lock().unwrap().iter().filter(|u| u.contains("musicbrainz") || u.contains("deezer")).count();

        assert!(searches_after_first > 0);
        assert_eq!(searches_after_first, searches_after_second);
    }

    #[test]
    fn missing_duration_does_not_lose_the_track() {
        let mut http = Canned::new();
        http.answers.insert("now_playing", RP_NOW.to_string());
        let info = radio_paradise_info(&http, 3, 1, 2, 2026, &mut CoverCache::default()).unwrap();
        assert_eq!(info.duration_ms, 0);
        assert_eq!(info.title, "Tire Loma Da Nigbehin");
    }

    #[test]
    fn old_album_prefers_musicbrainz_and_cover_archive() {
        let mut http = Canned::new();
        http.answers.insert(
            "musicbrainz.org",
            r#"{"releases":[{"id":"aaa","title":"Dawn of Awareness"}]}"#.to_string(),
        );
        http.existing.push("release/aaa/front-500");

        let cover = find_cover(&http, "MonoMono", "Dawn of Awareness", Some(2011), 2026);

        assert_eq!(cover.as_deref(), Some("https://coverartarchive.org/release/aaa/front-500"));
        assert!(!http.asked.lock().unwrap().iter().any(|u| u.contains("deezer")), "Deezer is only a fallback here");
    }

    #[test]
    fn new_release_goes_to_deezer_first() {
        let mut http = Canned::new();
        http.answers.insert(
            "deezer.com",
            r#"{"data":[{"title":"Fresh Album","artist":{"name":"Some Artist"},"cover_xl":"https://cdn.example/cover/abc/1000x1000-000000-80-0-0.jpg"}]}"#
                .to_string(),
        );

        let cover = find_cover(&http, "Some Artist", "Fresh Album", Some(2026), 2026);

        assert_eq!(cover.as_deref(), Some("https://cdn.example/cover/abc/600x600-000000-80-0-0.jpg"));
        assert!(!http.asked.lock().unwrap().iter().any(|u| u.contains("musicbrainz")));
    }

    #[test]
    fn falls_back_to_the_other_source() {
        let mut http = Canned::new();
        http.answers.insert("musicbrainz.org", r#"{"releases":[]}"#.to_string());
        http.answers.insert(
            "deezer.com",
            r#"{"data":[{"title":"Old Album","artist":{"name":"Band"},"cover_xl":"https://cdn.example/x/1000x1000-1.jpg"}]}"#
                .to_string(),
        );

        let cover = find_cover(&http, "Band", "Old Album", Some(1999), 2026);

        assert_eq!(cover.as_deref(), Some("https://cdn.example/x/600x600-1.jpg"));
    }

    const ITUNES_HIT: &str = r#"{"results":[{"collectionName":"Boy Alone","artistName":"Omah Lay","artworkUrl100":"https://is1-ssl.mzstatic.com/image/thumb/Music/abc/source/100x100bb.jpg"}]}"#;

    #[test]
    fn itunes_is_the_last_resort_for_an_old_album() {
        let mut http = Canned::new();
        http.answers.insert("musicbrainz.org", r#"{"releases":[]}"#.to_string());
        http.answers.insert("deezer.com", r#"{"data":[]}"#.to_string());
        http.answers.insert("itunes.apple.com", ITUNES_HIT.to_string());

        let cover = find_cover(&http, "Omah Lay", "Boy Alone", Some(2022), 2026);

        assert_eq!(
            cover.as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/Music/abc/source/600x600bb.jpg")
        );
    }

    #[test]
    fn a_new_release_asks_itunes_before_musicbrainz() {
        let mut http = Canned::new();
        http.answers.insert("deezer.com", r#"{"data":[]}"#.to_string());
        http.answers.insert("itunes.apple.com", ITUNES_HIT.to_string());

        let cover = find_cover(&http, "Omah Lay", "Boy Alone", Some(2026), 2026);

        assert!(cover.is_some());
        assert!(!http.asked.lock().unwrap().iter().any(|u| u.contains("musicbrainz")));
    }

    #[test]
    fn itunes_hit_for_another_album_is_rejected() {
        let body = r#"{"results":[{"collectionName":"Something Else","artistName":"Omah Lay","artworkUrl100":"https://x/100x100bb.jpg"}]}"#;
        assert_eq!(itunes_cover_from_search(body, "Omah Lay", "Boy Alone"), None);
    }

    #[test]
    fn release_group_is_tried_when_no_release_has_a_cover() {
        let mut http = Canned::new();
        http.answers.insert("ws/2/release/", r#"{"releases":[]}"#.to_string());
        http.answers.insert(
            "ws/2/release-group/",
            r#"{"release-groups":[{"id":"grp","title":"Livin on a High Note"}]}"#.to_string(),
        );
        http.existing.push("release-group/grp/front-500");

        let cover = find_cover(&http, "Mavis Staples", "Livin on a High Note", Some(2016), 2026);

        assert_eq!(cover.as_deref(), Some("https://coverartarchive.org/release-group/grp/front-500"));
    }

    #[test]
    fn release_without_a_front_cover_is_skipped() {
        let mut http = Canned::new();
        http.answers.insert(
            "musicbrainz.org",
            r#"{"releases":[{"id":"no-cover","title":"Album"},{"id":"has-cover","title":"Album"}]}"#.to_string(),
        );
        http.existing.push("release/has-cover/front-500");

        let cover = find_cover(&http, "A", "Album", Some(2000), 2026);

        assert_eq!(cover.as_deref(), Some("https://coverartarchive.org/release/has-cover/front-500"));
    }

    #[test]
    fn a_different_album_is_not_accepted() {
        let mut http = Canned::new();
        http.answers.insert(
            "deezer.com",
            r#"{"data":[{"title":"Something Else","artist":{"name":"Band"},"cover_xl":"https://cdn.example/y/1000x1000-1.jpg"}]}"#
                .to_string(),
        );
        http.answers.insert("musicbrainz.org", r#"{"releases":[]}"#.to_string());

        assert_eq!(find_cover(&http, "Band", "Wanted Album", Some(2000), 2026), None);
    }

    #[test]
    fn local_file_tags_are_shown_with_the_cover_the_chain_found() {
        let mut http = Canned::new();
        http.answers.insert("musicbrainz.org", r#"{"releases":[{"id":"abc","title":"Test Album"}]}"#.to_string());
        http.existing.push("release/abc/front-500");
        let tags = TrackTags { title: "Test Title".into(), artist: "Test Artist".into(), album: "Test Album".into(), year: Some(2011) };

        let pending = file_info(1, 2, &tags, None, true);
        let done = resolve_file_info(&http, 1, 2, &tags, 2026);

        assert!(pending.cover_lookup_pending && pending.cover_url.is_none());
        assert_eq!((done.artist.as_str(), done.title.as_str(), done.album.as_str()), ("Test Artist", "Test Title", "Test Album"));
        assert_eq!(done.year, Some(2011));
        assert_eq!(done.cover_url.as_deref(), Some("https://coverartarchive.org/release/abc/front-500"));
        assert!(!done.cover_lookup_pending, "the answer is final");
    }

    #[test]
    fn local_file_without_an_album_gets_no_cover_and_a_final_answer() {
        let http = Canned::new();
        let tags = TrackTags { title: "Song".into(), ..Default::default() };

        let done = resolve_file_info(&http, 1, 2, &tags, 2026);

        assert_eq!(done.cover_url, None);
        assert!(!done.cover_lookup_pending);
        assert!(http.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn stream_titles_split_into_artist_and_title() {
        assert_eq!(split_stream_title("Daft Punk - Get Lucky"), ("Daft Punk".to_string(), "Get Lucky".to_string()));
        assert_eq!(
            split_stream_title("Jay-Z - 99 Problems - Remix"),
            ("Jay-Z".to_string(), "99 Problems - Remix".to_string())
        );
        assert_eq!(split_stream_title("Station jingle"), (String::new(), "Station jingle".to_string()));
    }

    #[test]
    fn a_streamed_track_gets_its_album_and_cover_from_deezer() {
        let mut http = Canned::new();
        http.answers.insert(
            "api.deezer.com/search?",
            r#"{"data":[{"title":"Get Lucky","artist":{"name":"Daft Punk"},"album":{"title":"Random Access Memories","cover_xl":"https://cdn.example/c/1000x1000-000000-80-0-0.jpg"}}]}"#
                .to_string(),
        );

        let info = resolve_stream_info(&http, 1, 2, "Daft Punk", "Get Lucky");

        assert_eq!(info.album, "Random Access Memories");
        assert_eq!(info.cover_url.as_deref(), Some("https://cdn.example/c/600x600-000000-80-0-0.jpg"));
        assert!(!info.cover_lookup_pending);
    }

    #[test]
    fn itunes_answers_when_deezer_does_not_know_the_track() {
        let mut http = Canned::new();
        http.answers.insert("api.deezer.com/search?", r#"{"data":[]}"#.to_string());
        http.answers.insert(
            "itunes.apple.com",
            r#"{"results":[{"trackName":"Get Lucky","artistName":"Daft Punk","collectionName":"RAM","artworkUrl100":"https://x/100x100bb.jpg"}]}"#
                .to_string(),
        );

        let info = resolve_stream_info(&http, 1, 2, "Daft Punk", "Get Lucky");

        assert_eq!(info.album, "RAM");
        assert_eq!(info.cover_url.as_deref(), Some("https://x/600x600bb.jpg"));
    }

    #[test]
    fn an_unknown_track_keeps_its_title_without_a_cover() {
        let http = Canned::new();

        let info = resolve_stream_info(&http, 1, 2, "Nobody", "Nothing");

        assert_eq!((info.artist.as_str(), info.title.as_str()), ("Nobody", "Nothing"));
        assert_eq!(info.cover_url, None);
        assert!(!info.cover_lookup_pending, "the answer is final - the stand-in may show");
    }

    #[test]
    fn a_different_track_by_the_same_artist_is_not_accepted() {
        let body = r#"{"data":[{"title":"Another One","artist":{"name":"Daft Punk"},"album":{"title":"X","cover_xl":"https://c/1000x1000-1.jpg"}}]}"#;
        assert_eq!(deezer_track_from_search(body, "Daft Punk", "Get Lucky"), None);
    }

    #[test]
    fn empty_album_looks_nothing_up() {
        let http = Canned::new();
        assert_eq!(find_cover(&http, "Band", "  ", Some(2000), 2026), None);
        assert!(http.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn names_match_ignores_case_and_punctuation() {
        assert!(names_match("Dawn of Awareness", "dawn of awareness!"));
        assert!(names_match("Album (Deluxe Edition)", "Album"));
        assert!(!names_match("Album", "Other"));
        assert!(!names_match("", "Album"));
    }

    #[test]
    fn query_text_is_percent_encoded() {
        assert_eq!(encode_component("a b&c\"d"), "a%20b%26c%22d");
        assert_eq!(encode_component("AC-DC_1.0~"), "AC-DC_1.0~");
    }

    #[test]
    fn current_year_is_plausible() {
        let year = current_year();
        assert!((2024..2200).contains(&year), "got {year}");
    }
}
