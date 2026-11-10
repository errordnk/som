//! Radio Paradise as a [`StreamingService`]: its three main stations, each an
//! endless run of songs. The service's `get_block` call answers with a block of
//! songs and, for each, the link of its own FLAC file (`gapless_url`), so a
//! song is a finite track like any other - the player can skip it, seek in it
//! and have the next one ready ahead. The block after one is asked for with
//! `event=<end event of the block>`.

use crate::now_playing::{HttpGet, UreqHttp};
use crate::services::{OpenedTrack, StreamingService};
use somsrv::protocol::{RadioEvent, RadioTrack};

/// The service's id, as a playlist line names it.
pub const ID: &str = "paradise";

/// The stations: Radio Paradise's own slug and channel number.
const STATIONS: [(&str, u8); 3] = [("main-mix", 0), ("mellow", 1), ("rock", 2)];

/// FLAC, the best the service has.
const BITRATE: u8 = 4;

/// Blocks made only of station jingles are passed over, but not forever.
const MAX_BLOCKS_PER_ASK: usize = 4;

pub struct RadioParadise;

fn channel(station: &str) -> anyhow::Result<u8> {
    STATIONS
        .iter()
        .find(|(slug, _)| *slug == station)
        .map(|(_, chan)| *chan)
        .ok_or_else(|| anyhow::anyhow!("Radio Paradise has no station {station:?}"))
}

/// What one `get_block` answer holds: the songs worth playing and the event the
/// block after it starts from.
#[derive(Debug, PartialEq)]
struct Block {
    block_id: String,
    end_event: String,
    tracks: Vec<RadioTrack>,
}

/// A track's id carries the end event of its block, so that the ask for what
/// follows it needs nothing but the id: `<event>@<end event>`.
fn end_event_of(track_id: &str) -> Option<&str> {
    track_id.split_once('@').map(|(_, end)| end).filter(|end| !end.is_empty())
}

fn text(value: &serde_json::Value, key: &str) -> String {
    match value.get(key) {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

fn parse_block(body: &str) -> anyhow::Result<Block> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    let end_event = text(&value, "end_event");
    anyhow::ensure!(!end_event.is_empty(), "the answer names no end event");
    let image_base = match text(&value, "image_base") {
        base if base.is_empty() => "//img.radioparadise.com/".to_string(),
        base => base,
    };
    let songs = value.get("song").and_then(|songs| songs.as_object()).ok_or_else(|| anyhow::anyhow!("the answer has no songs"))?;
    // the songs are keyed "0", "1"...
    let mut ordered: Vec<(usize, &serde_json::Value)> = songs.iter().filter_map(|(key, song)| Some((key.parse().ok()?, song))).collect();
    ordered.sort_by_key(|(index, _)| *index);
    let mut tracks = Vec::new();
    for (_, song) in ordered {
        // "M" is music; the rest are the station's jingles
        if text(song, "type") != "M" {
            continue;
        }
        let target = text(song, "gapless_url");
        if target.is_empty() {
            continue;
        }
        let artist = text(song, "artist");
        let name = text(song, "title");
        let cover = text(song, "cover_large");
        tracks.push(RadioTrack {
            id: format!("{}@{end_event}", text(song, "event")),
            title: format!("{artist} - {name}"),
            target,
            liked: false,
            year: text(song, "year").parse().ok(),
            artist,
            name,
            album: text(song, "album"),
            duration_ms: song.get("duration").and_then(|duration| duration.as_u64()).and_then(|ms| u32::try_from(ms).ok()).unwrap_or(0),
            cover_url: (!cover.is_empty()).then(|| format!("https:{image_base}{cover}")),
        });
    }
    Ok(Block { block_id: text(&value, "block_id"), end_event, tracks })
}

fn radio_tracks(http: &dyn HttpGet, station: &str, queue: Option<&str>) -> anyhow::Result<(String, Vec<RadioTrack>)> {
    let chan = channel(station)?;
    let mut event = queue.and_then(end_event_of).map(str::to_string);
    for _ in 0..MAX_BLOCKS_PER_ASK {
        let mut url = format!("https://api.radioparadise.com/api/get_block?bitrate={BITRATE}&info=true&chan={chan}");
        if let Some(event) = &event {
            url.push_str(&format!("&event={event}"));
        }
        let block = parse_block(&http.get_text(&url)?)?;
        if !block.tracks.is_empty() {
            return Ok((block.block_id, block.tracks));
        }
        event = Some(block.end_event);
    }
    anyhow::bail!("Radio Paradise gave no songs on {station}")
}

impl StreamingService for RadioParadise {
    fn open_track(&self, url: &str) -> anyhow::Result<OpenedTrack> {
        anyhow::bail!("Radio Paradise songs are plain links the daemon fetches, not tracks to open: {url}")
    }

    fn radio_tracks(&self, station: &str, queue: Option<&str>) -> anyhow::Result<(String, Vec<RadioTrack>)> {
        radio_tracks(&UreqHttp::new(), station, queue)
    }

    fn radio_feedback(&self, _: &str, _: &str, _: RadioEvent, _: &str, _: f32) -> anyhow::Result<()> {
        Ok(())
    }

    fn set_like(&self, _: &str, _: bool) -> anyhow::Result<()> {
        anyhow::bail!("Radio Paradise has no likes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const BLOCK: &str = r#"{
        "event": "2945529", "block_id": "1643", "end_event": "2945534", "image_base": "\/\/img.radioparadise.com\/",
        "song": {
            "1": {"type": "M", "event": "2945530", "artist": "Dave Matthews", "title": "Too High", "year": "1994", "album": "Under the Table",
                  "duration": 329562, "cover_large": "covers\/l\/7962.jpg", "gapless_url": "https:\/\/audio.radioparadise.stream\/g\/1643-2.flac"},
            "0": {"type": "M", "event": "2945529", "artist": "Akshin Alizadeh", "title": "Southern Man", "year": "2014", "album": "Street",
                  "duration": 234969, "cover_large": "covers\/l\/18649.jpg", "gapless_url": "https:\/\/audio.radioparadise.stream\/g\/1643-1.flac"},
            "2": {"type": "P", "event": "2945531", "artist": "Commercial-free", "title": "Listener-supported", "duration": 44932,
                  "gapless_url": "https:\/\/audio.radioparadise.stream\/promos\/1278.flac"}
        }
    }"#;

    const JINGLE_ONLY: &str = r#"{"event": "2945535", "block_id": "0", "end_event": "2945535",
        "song": {"0": {"type": "P", "event": "2945535", "artist": "Commercial-free", "title": "Listener-supported", "gapless_url": "https:\/\/x\/1278.flac"}}}"#;

    struct Fake(Vec<(&'static str, &'static str)>, RefCell<Vec<String>>);

    impl HttpGet for Fake {
        fn get_text(&self, url: &str) -> anyhow::Result<String> {
            self.1.borrow_mut().push(url.to_string());
            self.0.iter().find(|(needle, _)| url.ends_with(needle)).map(|(_, body)| body.to_string()).ok_or_else(|| anyhow::anyhow!("no answer for {url}"))
        }
        fn exists(&self, _: &str) -> bool {
            true
        }
    }

    #[test]
    fn a_block_gives_its_songs_in_order_without_the_jingles() {
        let block = parse_block(BLOCK).expect("parses");
        assert_eq!(block.block_id, "1643");
        assert_eq!(block.end_event, "2945534");
        let titles: Vec<&str> = block.tracks.iter().map(|track| track.title.as_str()).collect();
        assert_eq!(titles, ["Akshin Alizadeh - Southern Man", "Dave Matthews - Too High"]);
        let first = &block.tracks[0];
        assert_eq!(first.id, "2945529@2945534");
        assert_eq!(first.target, "https://audio.radioparadise.stream/g/1643-1.flac");
        assert_eq!((first.year, first.duration_ms, first.name.as_str(), first.album.as_str()), (Some(2014), 234969, "Southern Man", "Street"));
        assert_eq!(first.cover_url.as_deref(), Some("https://img.radioparadise.com/covers/l/18649.jpg"));
    }

    #[test]
    fn the_next_batch_asks_for_the_block_after_the_last_track() {
        let http = Fake(vec![("chan=2&event=2945534", BLOCK)], RefCell::new(Vec::new()));
        let (batch, tracks) = radio_tracks(&http, "rock", Some("2945530@2945534")).expect("answers");
        assert_eq!((batch.as_str(), tracks.len()), ("1643", 2));
        assert_eq!(http.1.borrow().as_slice(), ["https://api.radioparadise.com/api/get_block?bitrate=4&info=true&chan=2&event=2945534"]);
    }

    #[test]
    fn a_block_of_jingles_only_is_passed_over() {
        let http = Fake(vec![("chan=1", JINGLE_ONLY), ("chan=1&event=2945535", BLOCK)], RefCell::new(Vec::new()));
        let (_, tracks) = radio_tracks(&http, "mellow", None).expect("answers");
        assert_eq!(tracks.len(), 2);
        assert_eq!(http.1.borrow().len(), 2);
    }

    #[test]
    fn an_unknown_station_is_an_error() {
        let http = Fake(Vec::new(), RefCell::new(Vec::new()));
        assert!(radio_tracks(&http, "jazz", None).is_err());
    }
}
