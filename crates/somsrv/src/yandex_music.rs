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

/// A track a link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRef {
    pub track_id: String,
}

/// `https://music.yandex.ru/album/9158345/track/10994777` or
/// `https://music.yandex.ru/track/10994777` (any `music.yandex.*` host).
pub fn parse_track_url(url: &str) -> Option<TrackRef> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    if !host.starts_with("music.yandex.") {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
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

/// The tags of a track as the catalogue knows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub year: Option<u32>,
    pub duration_ms: u32,
    /// Cover link, already sized for a large widget.
    pub cover_url: Option<String>,
}

pub fn parse_track_meta(json: &str) -> anyhow::Result<TrackMeta> {
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
    Ok(TrackMeta {
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

/// An opened track: its (decrypted) audio, size and tags.
pub struct OpenedTrack {
    pub reader: Box<dyn Read + Send>,
    pub total_size: u64,
    pub meta: TrackMeta,
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

/// Asks the service for the track's tags and stream and opens the stream.
pub fn open_track(track: &TrackRef) -> anyhow::Result<OpenedTrack> {
    let account = load_account()?;
    let token = account
        .token
        .ok_or_else(|| anyhow::anyhow!("no Yandex Music token: set music.yandex.token in settings.json"))?;
    let agent = agent(account.proxy.as_deref())?;

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
    Ok(OpenedTrack { reader, total_size, meta })
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
