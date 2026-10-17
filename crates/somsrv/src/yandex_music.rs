//! Yandex Music tracks for `FetchResource`: a `music.yandex.*` track link
//! is resolved through the service's (unofficial) mobile API to the
//! encrypted audio stream, which is decrypted on the fly, so the cache
//! receives a plain FLAC/MP3 stream like for any other audio URL.
//!
//! The account is the user's own: the OAuth token comes from
//! `music.yandex.token` in `settings.json` (an optional `music.yandex.proxy`
//! routes only these requests, for a network the service refuses).
//!
//! Parsing, signing and decryption are plain functions over strings and
//! bytes, unit-tested without a network.

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use crate::services::{OpenedTrack, StreamingService, TrackInfo};
use somsrv::protocol::{RadioEvent, RadioTrack};
use somsrv::service_link::yandex::{TrackRef, parse_track_url};
use std::io::Read;
use std::time::Duration;

const API_BASE: &str = "https://api.music.yandex.net";
/// The mobile client the API expects to be spoken to by. The server
/// refuses a request whose signing key does not belong to this client, so
/// the two change together.
const CLIENT_HEADER: &str = "YandexMusicAndroid/24023621";
const USER_AGENT: &str = "Yandex-Music-API";
/// The key this client signs `get-file-info` requests with.
const SIGN_KEY: &str = "p93jhgh689SBReK6ghtw62";
const TRANSPORT: &str = "encraw";
/// The best quality, and the codecs the audio player can decode: the
/// server answers with the best stream the account is entitled to, which
/// may be a plain MP3 whatever is asked.
const QUALITY: &str = "lossless";
const CODECS: [&str; 2] = ["flac", "mp3"];

/// The signature of a `get-file-info` request: base64 of an HMAC-SHA256
/// over the concatenated parameters, without the padding.
pub fn sign(timestamp: u64, track_id: &str, quality: &str, codecs: &[&str]) -> anyhow::Result<String> {
    let data = format!("{timestamp}{track_id}{quality}{}{TRANSPORT}", codecs.concat());
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(SIGN_KEY.as_bytes())?;
    mac.update(data.as_bytes());
    let encoded = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    Ok(encoded.trim_end_matches('=').to_string())
}

/// What `get-file-info` answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub url: String,
    /// Hex AES-128 key, present when the stream is encrypted.
    pub key: Option<String>,
    pub codec: String,
}

pub fn parse_file_info(json: &str) -> anyhow::Result<FileInfo> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let info = value
        .pointer("/result/downloadInfo")
        .ok_or_else(|| anyhow::anyhow!("no downloadInfo in the reply: {}", api_error(&value)))?;
    let url = info.get("url").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("downloadInfo has no url"))?;
    Ok(FileInfo {
        url: url.to_string(),
        key: info.get("key").and_then(|v| v.as_str()).filter(|key| !key.is_empty()).map(str::to_string),
        codec: info.get("codec").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    })
}

fn api_error(value: &serde_json::Value) -> String {
    value
        .pointer("/error/name")
        .or_else(|| value.pointer("/error/message"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown error")
        .to_string()
}

pub fn parse_track_meta(json: &str) -> anyhow::Result<TrackInfo> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let track = value
        .pointer("/result/0")
        .ok_or_else(|| anyhow::anyhow!("no track in the reply: {}", api_error(&value)))?;
    let text = |pointer: &str| track.pointer(pointer).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut title = text("/title");
    let version = text("/version");
    if !version.is_empty() {
        title = format!("{title} ({version})");
    }
    let artist = track
        .get("artists")
        .and_then(|v| v.as_array())
        .map(|artists| artists.iter().filter_map(|a| a.get("name").and_then(|n| n.as_str())).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let cover = track.get("coverUri").and_then(|v| v.as_str()).or_else(|| track.pointer("/albums/0/coverUri").and_then(|v| v.as_str()));
    Ok(TrackInfo {
        title,
        artist,
        album: text("/albums/0/title"),
        year: track.pointer("/albums/0/year").and_then(|v| v.as_u64()).map(|year| year as u32),
        duration_ms: track.get("durationMs").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        cover_url: cover.map(cover_link),
    })
}

/// A catalogue cover is `host/path/%%`; `%%` stands for the size.
fn cover_link(uri: &str) -> String {
    format!("https://{}", uri.replace("%%", "1000x1000"))
}

/// AES-128 in CTR mode with a 128-bit big-endian counter starting at zero
/// (the service's stream encryption), applied while reading.
pub struct CtrReader<R> {
    inner: R,
    cipher: Aes128,
    counter: u128,
    keystream: [u8; 16],
    /// How many bytes of `keystream` are used up; 16 means "make the next".
    used: usize,
}

impl<R: Read> CtrReader<R> {
    pub fn new(inner: R, key_hex: &str) -> anyhow::Result<Self> {
        let key = hex::decode(key_hex)?;
        let cipher = Aes128::new_from_slice(&key).map_err(|_| anyhow::anyhow!("the stream key is {} bytes, not 16", key.len()))?;
        Ok(Self { inner, cipher, counter: 0, keystream: [0; 16], used: 16 })
    }
}

impl<R: Read> Read for CtrReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        for byte in &mut buf[..n] {
            if self.used == 16 {
                let mut block = GenericArray::clone_from_slice(&self.counter.to_be_bytes());
                self.cipher.encrypt_block(&mut block);
                self.keystream.copy_from_slice(&block);
                self.counter = self.counter.wrapping_add(1);
                self.used = 0;
            }
            *byte ^= self.keystream[self.used];
            self.used += 1;
        }
        Ok(n)
    }
}

/// What `somsrv` reads settings for.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Account {
    pub token: Option<String>,
    pub proxy: Option<String>,
}

/// `music.yandex.token` / `music.yandex.proxy` of a settings document.
pub fn account_from_settings(settings: &str) -> Account {
    let Ok(value) = serde_json_lenient::from_str::<serde_json::Value>(settings) else {
        return Account::default();
    };
    let field = |name: &str| {
        value
            .pointer(&format!("/music/yandex/{name}"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    Account { token: field("token"), proxy: field("proxy") }
}

fn load_account() -> anyhow::Result<Account> {
    let path = paths::settings_file();
    let text = std::fs::read_to_string(path).map_err(|err| anyhow::anyhow!("reading {}: {err}", path.display()))?;
    Ok(account_from_settings(&text))
}

/// The account's token and a client set up for it (with its proxy).
fn signed_in_agent() -> anyhow::Result<(String, ureq::Agent)> {
    let account = load_account()?;
    let token = account
        .token
        .ok_or_else(|| anyhow::anyhow!("no Yandex Music token: set music.yandex.token in settings.json"))?;
    Ok((token, agent(account.proxy.as_deref())?))
}

fn agent(proxy: Option<&str>) -> anyhow::Result<ureq::Agent> {
    // A refusal carries the service's own explanation in its body, which
    // `api_get` reports - so a status is not turned into an error here.
    let mut builder = ureq::Agent::config_builder().timeout_connect(Some(Duration::from_secs(10))).http_status_as_error(false);
    if let Some(proxy) = proxy {
        builder = builder.proxy(Some(ureq::Proxy::new(proxy)?));
    }
    Ok(ureq::Agent::new_with_config(builder.build()))
}

fn api_get(agent: &ureq::Agent, token: &str, what: &str, url: &str) -> anyhow::Result<String> {
    let mut response = agent
        .get(url)
        .header("Authorization", format!("OAuth {token}"))
        .header("X-Yandex-Music-Client", CLIENT_HEADER)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|err| anyhow::anyhow!("Yandex Music {what} request failed: {err}"))?;
    let status = response.status();
    let body = response.body_mut().read_to_string()?;
    if !status.is_success() {
        let said: String = body.chars().take(300).collect();
        anyhow::bail!("Yandex Music {what} answered {status}: {said}");
    }
    Ok(body)
}

fn api_post(agent: &ureq::Agent, token: &str, what: &str, url: &str, content_type: &str, body: &str) -> anyhow::Result<String> {
    let mut response = agent
        .post(url)
        .header("Authorization", format!("OAuth {token}"))
        .header("X-Yandex-Music-Client", CLIENT_HEADER)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", content_type)
        .send(body)
        .map_err(|err| anyhow::anyhow!("Yandex Music {what} request failed: {err}"))?;
    let status = response.status();
    let body = response.body_mut().read_to_string()?;
    if !status.is_success() {
        let said: String = body.chars().take(300).collect();
        anyhow::bail!("Yandex Music {what} answered {status}: {said}");
    }
    Ok(body)
}

/// The stream of the best quality the account is entitled to.
fn file_info(agent: &ureq::Agent, token: &str, track: &TrackRef) -> anyhow::Result<FileInfo> {
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let signature = sign(timestamp, &track.track_id, QUALITY, &CODECS)?;
    let query = format!(
        "ts={timestamp}&trackId={}&quality={QUALITY}&codecs={}&transports={TRANSPORT}&sign={}",
        track.track_id,
        CODECS.join(","),
        percent_encoding::utf8_percent_encode(&signature, percent_encoding::NON_ALPHANUMERIC),
    );
    parse_file_info(&api_get(agent, token, "file info", &format!("{API_BASE}/get-file-info?{query}"))?)
}

/// One track of a radio batch: its id, the "artist - title" to show, the
/// link that plays it and its album's year. `None` for a track the service
/// marks unavailable.
fn batch_track(track: &serde_json::Value) -> Option<(String, String, String, Option<u32>)> {
    if track.get("available").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let id = match track.get("id")? {
        serde_json::Value::String(id) => id.clone(),
        other => other.as_u64()?.to_string(),
    };
    let title = track.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let artist = track
        .get("artists")
        .and_then(|v| v.as_array())
        .map(|artists| artists.iter().filter_map(|a| a.get("name").and_then(|n| n.as_str())).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let album_id = track
        .pointer("/albums/0/id")
        .and_then(|v| v.as_u64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_string)));
    let target = match album_id {
        Some(album) => format!("https://music.yandex.ru/album/{album}/track/{id}"),
        None => format!("https://music.yandex.ru/track/{id}"),
    };
    let title = if artist.is_empty() { title } else { format!("{artist} - {title}") };
    let year = track.pointer("/albums/0/year").and_then(|v| v.as_u64()).and_then(|year| u32::try_from(year).ok());
    Some((id, title, target, year))
}

/// A station is `<type>:<tag>` (`user:onyourwave`, `track:38526`); nothing
/// else may reach the URL path it is put into.
fn valid_station(station: &str) -> anyhow::Result<()> {
    let plain = !station.is_empty() && station.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'));
    if !plain || !station.contains(':') {
        anyhow::bail!("not a station name: {station:?}");
    }
    Ok(())
}

/// The batch of a `rotor/station/.../tracks` answer: its id and tracks.
pub fn parse_radio(json: &str) -> anyhow::Result<(String, Vec<RadioTrack>)> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let result = value.get("result").ok_or_else(|| anyhow::anyhow!("no batch in the reply: {}", api_error(&value)))?;
    let batch_id = result.get("batchId").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tracks = result
        .get("sequence")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let (id, title, target, year) = batch_track(item.get("track")?)?;
            Some(RadioTrack { id, title, target, liked: item.get("liked").and_then(|v| v.as_bool()).unwrap_or(false), year })
        })
        .collect::<Vec<_>>();
    if tracks.is_empty() {
        anyhow::bail!("the station gave no tracks");
    }
    Ok((batch_id, tracks))
}

/// The next batch of a wave station, continuing after `queue`.
pub fn radio_tracks(station: &str, queue: Option<&str>) -> anyhow::Result<(String, Vec<RadioTrack>)> {
    valid_station(station)?;
    if let Some(queue) = queue
        && !queue.bytes().all(|byte| byte.is_ascii_digit())
    {
        anyhow::bail!("not a track id: {queue:?}");
    }
    let (token, agent) = signed_in_agent()?;
    let mut url = format!("{API_BASE}/rotor/station/{station}/tracks?settings2=true");
    if let Some(queue) = queue {
        url.push_str(&format!("&queue={queue}"));
    }
    parse_radio(&api_get(&agent, &token, "radio", &url)?)
}

/// `2026-10-16T08:12:30.123Z` for `unix_millis`: the time format the rotor
/// feedback is dated in.
pub fn iso_timestamp(unix_millis: u64) -> String {
    let seconds = unix_millis / 1000;
    let (days, in_day) = (seconds / 86400, seconds % 86400);
    // Civil date from a day count (Howard Hinnant's algorithm).
    let shifted = days as i64 + 719468;
    let era = shifted.div_euclid(146097);
    let day_of_era = shifted.rem_euclid(146097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60,
        unix_millis % 1000
    )
}

/// The JSON a feedback event is sent as.
pub fn feedback_body(event: RadioEvent, track_id: &str, played_seconds: f32, timestamp: &str) -> String {
    let body = match event {
        RadioEvent::RadioStarted => serde_json::json!({"type": "radioStarted", "timestamp": timestamp, "from": "mobile-radio-user-onyourwave-default"}),
        RadioEvent::TrackStarted => serde_json::json!({"type": "trackStarted", "timestamp": timestamp, "trackId": track_id}),
        RadioEvent::TrackFinished => {
            serde_json::json!({"type": "trackFinished", "timestamp": timestamp, "trackId": track_id, "totalPlayedSeconds": played_seconds})
        },
        RadioEvent::Skip => serde_json::json!({"type": "skip", "timestamp": timestamp, "trackId": track_id, "totalPlayedSeconds": played_seconds}),
    };
    body.to_string()
}

/// Tells a wave station what happened to a track.
pub fn radio_feedback(station: &str, batch_id: &str, event: RadioEvent, track_id: &str, played_seconds: f32) -> anyhow::Result<()> {
    valid_station(station)?;
    if !batch_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')) {
        anyhow::bail!("not a batch id: {batch_id:?}");
    }
    let (token, agent) = signed_in_agent()?;
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let body = feedback_body(event, track_id, played_seconds, &iso_timestamp(millis));
    let url = format!("{API_BASE}/rotor/station/{station}/feedback?batch-id={batch_id}");
    api_post(&agent, &token, "radio feedback", &url, "application/json", &body)?;
    Ok(())
}

/// The account's id out of an `account/status` answer.
pub fn parse_uid(json: &str) -> anyhow::Result<String> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    match value.pointer("/result/account/uid") {
        Some(serde_json::Value::Number(uid)) => Ok(uid.to_string()),
        Some(serde_json::Value::String(uid)) if !uid.is_empty() && uid.bytes().all(|byte| byte.is_ascii_digit()) => Ok(uid.clone()),
        _ => anyhow::bail!("no account id in the reply: {}", api_error(&value)),
    }
}

/// Likes the track, or takes the like back.
pub fn set_like(track_id: &str, liked: bool) -> anyhow::Result<()> {
    if track_id.is_empty() || !track_id.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::bail!("not a track id: {track_id:?}");
    }
    let (token, agent) = signed_in_agent()?;
    let uid = parse_uid(&api_get(&agent, &token, "account", &format!("{API_BASE}/account/status"))?)?;
    let action = if liked { "add-multiple" } else { "remove" };
    let url = format!("{API_BASE}/users/{uid}/likes/tracks/{action}");
    api_post(&agent, &token, "like", &url, "application/x-www-form-urlencoded", &format!("track-ids={track_id}"))?;
    Ok(())
}

/// Asks the service for the track's tags and stream and opens the stream.
fn open(track: &TrackRef) -> anyhow::Result<OpenedTrack> {
    let (token, agent) = signed_in_agent()?;

    let meta = parse_track_meta(&api_get(&agent, &token, "track tags", &format!("{API_BASE}/tracks?track-ids={}", track.track_id))?)?;

    let info = file_info(&agent, &token, track)?;

    let response = agent.get(&info.url).call().map_err(|err| anyhow::anyhow!("Yandex Music stream request failed: {err}"))?;
    if !response.status().is_success() {
        anyhow::bail!("Yandex Music stream answered {}", response.status());
    }
    let total_size = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let body = response.into_body().into_reader();
    let reader: Box<dyn Read + Send> = match &info.key {
        Some(key) => Box::new(CtrReader::new(body, key)?),
        None => Box::new(body),
    };
    log::info!("Yandex Music track {}: {} {:?}", track.track_id, info.codec, meta.title);
    Ok(OpenedTrack { reader, total_size, info: meta })
}

/// Yandex Music as a [`StreamingService`].
pub struct YandexMusic;

impl StreamingService for YandexMusic {
    fn open_track(&self, url: &str) -> anyhow::Result<OpenedTrack> {
        let track = parse_track_url(url).ok_or_else(|| anyhow::anyhow!("not a Yandex Music track link: {url}"))?;
        open(&track)
    }

    fn radio_tracks(&self, station: &str, queue: Option<&str>) -> anyhow::Result<(String, Vec<RadioTrack>)> {
        radio_tracks(station, queue)
    }

    fn radio_feedback(&self, station: &str, batch_id: &str, event: RadioEvent, track_id: &str, played_seconds: f32) -> anyhow::Result<()> {
        radio_feedback(station, batch_id, event, track_id, played_seconds)
    }

    fn set_like(&self, track_id: &str, liked: bool) -> anyhow::Result<()> {
        set_like(track_id, liked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_unpadded_base64_and_stable() {
        let signature = sign(1700000000, "10994777", QUALITY, &CODECS).expect("signing");
        assert_eq!(signature.len(), 43, "a SHA-256 MAC is 43 base64 characters without padding");
        assert!(!signature.contains('='));
        assert_eq!(signature, sign(1700000000, "10994777", QUALITY, &CODECS).expect("signing"));
        assert_ne!(signature, sign(1700000001, "10994777", QUALITY, &CODECS).expect("signing"));
    }

    #[test]
    fn ctr_matches_the_nist_vector() {
        // NIST SP 800-38A F.5.1 with the counter block 0: AES-128 of an
        // all-zero counter is the first key-stream block, so encrypting
        // zeros gives it back.
        let key = "2b7e151628aed2a6abf7158809cf4f3c";
        let mut reader = CtrReader::new(std::io::Cursor::new(vec![0u8; 20]), key).expect("reader");
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read");
        let aes = Aes128::new_from_slice(&hex::decode(key).expect("hex")).expect("key");
        let mut block = GenericArray::clone_from_slice(&0u128.to_be_bytes());
        aes.encrypt_block(&mut block);
        assert_eq!(&out[..16], block.as_slice());
        let mut second = GenericArray::clone_from_slice(&1u128.to_be_bytes());
        aes.encrypt_block(&mut second);
        assert_eq!(&out[16..], &second.as_slice()[..4]);
    }

    #[test]
    fn ctr_round_trips_in_odd_sized_reads() {
        let key = "000102030405060708090a0b0c0d0e0f";
        let plain: Vec<u8> = (0..1000u32).map(|n| (n % 251) as u8).collect();
        let mut encrypted = Vec::new();
        CtrReader::new(std::io::Cursor::new(plain.clone()), key).expect("reader").read_to_end(&mut encrypted).expect("read");
        assert_ne!(encrypted, plain);
        let mut reader = CtrReader::new(std::io::Cursor::new(encrypted), key).expect("reader");
        let mut decrypted = Vec::new();
        let mut piece = [0u8; 7];
        loop {
            let n = reader.read(&mut piece).expect("read");
            if n == 0 {
                break;
            }
            decrypted.extend_from_slice(&piece[..n]);
        }
        assert_eq!(decrypted, plain);
    }

    #[test]
    fn a_wrong_key_length_is_an_error() {
        assert!(CtrReader::new(std::io::Cursor::new(Vec::<u8>::new()), "0011").is_err());
        assert!(CtrReader::new(std::io::Cursor::new(Vec::<u8>::new()), "zz").is_err());
    }

    #[test]
    fn file_info_is_parsed() {
        let json = r#"{"result":{"downloadInfo":{"trackId":"1","quality":"lossless","codec":"flac","transport":"encraw","key":"00112233445566778899aabbccddeeff","url":"https://strm.example/x"}}}"#;
        let info = parse_file_info(json).expect("parse");
        assert_eq!(info.url, "https://strm.example/x");
        assert_eq!(info.key.as_deref(), Some("00112233445566778899aabbccddeeff"));
        assert_eq!(info.codec, "flac");
    }

    #[test]
    fn a_radio_batch_is_parsed() {
        let json = r#"{"result":{"batchId":"1792-362","sequence":[
            {"type":"track","liked":true,"track":{"id":"38526","title":"Roads","artists":[{"name":"Portishead"}],"albums":[{"id":3193,"year":1994}]}},
            {"type":"track","liked":false,"track":{"id":80041888,"title":"Take Me Away","artists":[{"name":"Monolink"}],"albums":[{"id":14617746}]}},
            {"type":"track","track":{"id":"9","title":"Gone","available":false}}]}}"#;
        let (batch, tracks) = parse_radio(json).expect("batch");
        assert_eq!(batch, "1792-362");
        assert_eq!(tracks.len(), 2, "an unavailable track is left out");
        assert_eq!(
            tracks[0],
            RadioTrack {
                id: "38526".into(),
                title: "Portishead - Roads".into(),
                target: "https://music.yandex.ru/album/3193/track/38526".into(),
                liked: true,
                year: Some(1994),
            }
        );
        assert_eq!(tracks[1].id, "80041888");
        assert!(!tracks[1].liked);
        assert!(parse_radio(r#"{"result":{"batchId":"b","sequence":[]}}"#).is_err());
    }

    #[test]
    fn only_station_names_reach_the_url() {
        assert!(valid_station("user:onyourwave").is_ok());
        assert!(valid_station("track:38526").is_ok());
        assert!(valid_station("").is_err());
        assert!(valid_station("noseparator").is_err());
        assert!(valid_station("user:../account").is_err());
        assert!(valid_station("user:a?b=c").is_err());
    }

    #[test]
    fn the_feedback_timestamp_is_iso_utc() {
        assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_timestamp(951_782_400_123), "2000-02-29T00:00:00.123Z");
        assert_eq!(iso_timestamp(1_792_139_758_049), "2026-10-16T08:35:58.049Z");
    }

    #[test]
    fn feedback_bodies_carry_what_the_station_needs() {
        let started: serde_json::Value = serde_json::from_str(&feedback_body(RadioEvent::TrackStarted, "38526", 0.0, "T")).expect("json");
        assert_eq!((started["type"].as_str(), started["trackId"].as_str()), (Some("trackStarted"), Some("38526")));
        let skipped: serde_json::Value = serde_json::from_str(&feedback_body(RadioEvent::Skip, "38526", 12.5, "T")).expect("json");
        assert_eq!(skipped["type"], "skip");
        assert_eq!(skipped["totalPlayedSeconds"], 12.5);
        let radio: serde_json::Value = serde_json::from_str(&feedback_body(RadioEvent::RadioStarted, "", 0.0, "T")).expect("json");
        assert_eq!(radio["type"], "radioStarted");
        assert!(radio.get("trackId").is_none());
    }

    #[test]
    fn the_account_id_is_read_as_number_or_text() {
        assert_eq!(parse_uid(r#"{"result":{"account":{"uid":12259205}}}"#).expect("uid"), "12259205");
        assert_eq!(parse_uid(r#"{"result":{"account":{"uid":"77"}}}"#).expect("uid"), "77");
        assert!(parse_uid(r#"{"result":{"account":{}}}"#).is_err());
        assert!(parse_uid(r#"{"result":{"account":{"uid":"../x"}}}"#).is_err());
    }

    #[test]
    fn a_refusal_is_reported_by_name() {
        let err = parse_file_info(r#"{"error":{"name":"Unavailable For Legal Reasons"}}"#).expect_err("refusal");
        assert!(err.to_string().contains("Unavailable For Legal Reasons"), "{err}");
    }

    #[test]
    fn track_meta_is_parsed() {
        let json = r#"{"result":[{"title":"Time","version":"2011 Remaster","durationMs":413000,
            "artists":[{"name":"Pink Floyd"},{"name":"Guest"}],
            "coverUri":"avatars.yandex.net/get-music-content/1/abc/%%",
            "albums":[{"title":"The Dark Side of the Moon","year":1973}]}]}"#;
        let meta = parse_track_meta(json).expect("parse");
        assert_eq!(meta.title, "Time (2011 Remaster)");
        assert_eq!(meta.artist, "Pink Floyd, Guest");
        assert_eq!(meta.album, "The Dark Side of the Moon");
        assert_eq!(meta.year, Some(1973));
        assert_eq!(meta.duration_ms, 413000);
        assert_eq!(meta.cover_url.as_deref(), Some("https://avatars.yandex.net/get-music-content/1/abc/1000x1000"));
    }

    #[test]
    fn the_account_is_read_from_lenient_settings() {
        let settings = r#"{
            // comment
            "music": { "yandex": { "token": " y0_abc ", "proxy": "" } },
        }"#;
        let account = account_from_settings(settings);
        assert_eq!(account.token.as_deref(), Some("y0_abc"));
        assert_eq!(account.proxy, None);
        assert_eq!(account_from_settings("not json"), Account::default());
    }
}
