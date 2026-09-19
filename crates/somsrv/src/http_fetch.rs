//! `somsrv`'s own source of `PutChunk`s for a `SrvRequest::FetchResource`
//! — a second case (alongside `crate::lua`) where the daemon originates
//! chunks itself rather than only relaying ones an external client
//! already sent. See `FetchResource`'s own doc comment for the full
//! design context (built ahead of a future markdown-embedded-media
//! feature, no real caller exists yet).
//!
//! Deliberately a plain function, not a struct/cache object — like
//! `crate::lua::run_and_stream`, there is no state to keep between calls:
//! each `FetchResource` is classified (network vs local path), read
//! start to finish, and streamed into `SrvCache::put_chunk` in one go.

use crate::srv_cache::SrvCache;
use somsrv::protocol::{ContentMetadata, ContentType};
use std::io::Read;
use std::path::Path;

/// Same chunk size every other `PutChunk` sender in this project uses
/// (`somsrp::CHUNK_SIZE`, `crate::lua`'s own `CHUNK_SIZE`) — no reason
/// for the fetch-originated path to pick a different number.
const CHUNK_SIZE: usize = 65536;

/// Resolves `target` and streams its bytes into `cache.put_chunk(...)` —
/// see `SrvRequest::FetchResource`'s own doc comment for the classification
/// rule (`http://`/`https://` prefix → network fetch; anything else → a
/// local filesystem path, joined against `base_dir` if `target` is
/// relative) and for why there is deliberately no path-sandboxing beyond
/// the OS's own file permissions.
///
/// Returns `Err` on any failure (DNS/connect/TLS/non-2xx for a URL;
/// not-found/permission-denied/unreadable for a local path; an
/// unrecognized `Content-Type`/extension) — the caller (`server::
/// handle_srv_request`) turns this into a `SrvResponse::FetchFailed`
/// reply, unlike `crate::lua::run_and_stream`'s failure path (logged
/// server-side only).
pub fn fetch_and_stream(cache: &SrvCache, session_id: u32, file_id: u32, target: &str, base_dir: Option<&str>) -> anyhow::Result<()> {
    if target.starts_with("http://") || target.starts_with("https://") {
        fetch_network(cache, session_id, file_id, target)
    } else {
        fetch_local(cache, session_id, file_id, target, base_dir)
    }
}

fn fetch_network(cache: &SrvCache, session_id: u32, file_id: u32, url: &str) -> anyhow::Result<()> {
    let response = ureq::get(url).call()?;

    let content_type_header = response.headers().get(http::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or("");
    let content_type = content_type_from_mime(content_type_header, url)
        .ok_or_else(|| anyhow::anyhow!("unrecognized content type {content_type_header:?} for {url}"))?;

    let total_size = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);

    let mut body = response.into_body();
    let reader = body.as_reader();
    stream_reader_into_cache(cache, session_id, file_id, reader, total_size, content_type)
}

fn fetch_local(cache: &SrvCache, session_id: u32, file_id: u32, target: &str, base_dir: Option<&str>) -> anyhow::Result<()> {
    let path = Path::new(target);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match base_dir {
            Some(base_dir) => Path::new(base_dir).join(path),
            None => path.to_path_buf(),
        }
    };

    let content_type = content_type_from_extension(&resolved)
        .ok_or_else(|| anyhow::anyhow!("unrecognized extension for {}", resolved.display()))?;

    let file = std::fs::File::open(&resolved).map_err(|err| anyhow::anyhow!("opening {}: {err}", resolved.display()))?;
    let total_size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);

    stream_reader_into_cache(cache, session_id, file_id, file, total_size, content_type)
}

/// Reads `reader` to completion in `CHUNK_SIZE` pieces, pushing each
/// through `cache.put_chunk` — shared by both the network and local-path
/// branches once each has produced a plain `impl Read` and already knows
/// `total_size`/`content_type`.
fn stream_reader_into_cache(
    cache: &SrvCache,
    session_id: u32,
    file_id: u32,
    mut reader: impl Read,
    total_size: u64,
    content_type: ContentType,
) -> anyhow::Result<()> {
    let metadata = metadata_for(content_type);
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut offset = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        cache.put_chunk(session_id, file_id, offset, &buf[..n], total_size, content_type, metadata.clone());
        offset += n as u64;
    }
    // An empty body still needs ONE put_chunk call (offset 0, zero-length
    // data) so `total_size` reaches the cache and Som's own `contiguous_
    // len` watermark can reach it — mirrors `crate::lua::run_and_stream`'s
    // identical empty-result handling.
    if offset == 0 {
        cache.put_chunk(session_id, file_id, 0, &[], total_size, content_type, metadata);
    }
    Ok(())
}

/// `ContentMetadata`'s already-existing "unknown" convention (zero-valued
/// numeric fields) for every content type except `Markdown` (which has
/// none) — `somsrv` has no FFmpeg/image-decoding dependency to probe real
/// pixel dimensions/duration/codec from fetched bytes, and won't gain one
/// for this. An accepted, explicit gap for this pass (see this module's
/// own doc comment) — a future markdown-embedded-media feature decides
/// whether real probing belongs here or client-side.
fn metadata_for(content_type: ContentType) -> ContentMetadata {
    match content_type {
        ContentType::Gif | ContentType::Jpeg | ContentType::Png => {
            ContentMetadata::Image { width_px: 0, height_px: 0, color_bits: 0, is_animated: false }
        },
        ContentType::Audio => ContentMetadata::Audio { sample_rate: 0, channels: 0, bits_per_sample: 0, duration_ms: 0, extension: String::new() },
        ContentType::Video => ContentMetadata::Video {
            width_px: 0,
            height_px: 0,
            fps_numerator: 0,
            fps_denominator: 0,
            codec: somsrv::protocol::VideoCodec::Unknown,
            audio_stream_index: None,
            subtitle_stream_index: None,
            extension: String::new(),
        },
        // A fetched markdown document's own base_dir is genuinely
        // unknown to `somsrv` (nested markdown-in-markdown is out of
        // scope) — empty per the "empty means unknown" convention.
        ContentType::Markdown => ContentMetadata::Markdown { base_dir: String::new() },
    }
}

/// Maps an HTTP response's `Content-Type` header onto `ContentType` — see
/// `SrvRequest::FetchResource`'s own doc comment for the exact mapping
/// table. Falls back to the URL's own extension when the header is
/// missing or too generic to be useful (`text/plain`, `application/
/// octet-stream`) to distinguish markdown from anything else, mirroring
/// how a browser's own "save as" dialog falls back to the URL when the
/// server doesn't send a useful `Content-Type`.
fn content_type_from_mime(mime: &str, url: &str) -> Option<ContentType> {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match mime.as_str() {
        "image/gif" => return Some(ContentType::Gif),
        "image/png" => return Some(ContentType::Png),
        "image/jpeg" => return Some(ContentType::Jpeg),
        "text/markdown" => return Some(ContentType::Markdown),
        _ => {},
    }
    if mime.starts_with("video/") {
        return Some(ContentType::Video);
    }
    if mime.starts_with("audio/") {
        return Some(ContentType::Audio);
    }
    if matches!(mime.as_str(), "" | "text/plain" | "application/octet-stream") {
        let url_path = url.split(['?', '#']).next().unwrap_or(url);
        if url_path.to_ascii_lowercase().ends_with(".md") {
            return Some(ContentType::Markdown);
        }
    }
    None
}

/// Maps a local file's extension onto `ContentType` — same mapping
/// `somsrp::main::stream_file` already uses for its own local-file
/// sending path, kept in sync by hand (no shared helper — `somsrp`
/// depends on `crate::terminal::rich_content_transport::ContentType`,
/// `somsrv` deliberately does not depend on that GPUI-pulling crate at
/// all, see `protocol.rs`'s own doc comment on `ContentType` for why
/// these two enums are separate, hand-mirrored copies).
fn content_type_from_extension(path: &Path) -> Option<ContentType> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "gif" => Some(ContentType::Gif),
        "jpg" | "jpeg" => Some(ContentType::Jpeg),
        "png" => Some(ContentType::Png),
        "mp3" | "flac" => Some(ContentType::Audio),
        "mp4" | "mkv" | "avi" => Some(ContentType::Video),
        "md" => Some(ContentType::Markdown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use somsrv::protocol::SrvResponse;
    use std::sync::{Arc, Mutex};

    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("somsrv_http_fetch_test_{}_{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Subscribes a fresh `SrvCache` to `(1, 2)` and returns the observed
    /// `(chunk_offset, chunk_data)` pairs plus the final `total_size`/
    /// `content_type` seen — enough to assert byte-for-byte delivery
    /// without needing a real connection.
    fn subscribe_and_collect(cache: &SrvCache) -> Arc<Mutex<Vec<SrvResponse>>> {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_clone = observed.clone();
        cache.subscribe(1, 2, Arc::new(move |response| {
            observed_clone.lock().unwrap().push(response);
            Ok(())
        }));
        observed
    }

    #[test]
    fn local_path_streams_file_bytes_with_correct_content_type() {
        let path = temp_file("image.png", b"fake png bytes");
        let cache = SrvCache::new();
        let observed = subscribe_and_collect(&cache);

        fetch_and_stream(&cache, 1, 2, path.to_str().unwrap(), None).unwrap();

        let observed = observed.lock().unwrap();
        let mut chunk_data = Vec::new();
        for response in observed.iter() {
            if let SrvResponse::Progress { chunk_data: data, content_type, total_size, .. } = response {
                chunk_data.extend_from_slice(data);
                assert_eq!(*content_type, ContentType::Png);
                assert_eq!(*total_size, 14);
            }
        }
        assert_eq!(chunk_data, b"fake png bytes");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn local_path_relative_to_base_dir_resolves_correctly() {
        let dir = std::env::temp_dir().join(format!("somsrv_http_fetch_test_dir_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("notes.md");
        std::fs::write(&file_path, b"# hello").unwrap();

        let cache = SrvCache::new();
        let observed = subscribe_and_collect(&cache);

        fetch_and_stream(&cache, 1, 2, "notes.md", Some(dir.to_str().unwrap())).unwrap();

        let observed = observed.lock().unwrap();
        let mut chunk_data = Vec::new();
        for response in observed.iter() {
            if let SrvResponse::Progress { chunk_data: data, content_type, .. } = response {
                chunk_data.extend_from_slice(data);
                assert_eq!(*content_type, ContentType::Markdown);
            }
        }
        assert_eq!(chunk_data, b"# hello");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_local_file_is_an_error_and_puts_no_chunks() {
        let cache = SrvCache::new();
        let observed = subscribe_and_collect(&cache);

        let result = fetch_and_stream(&cache, 1, 2, "/definitely/does/not/exist.png", None);
        assert!(result.is_err());

        assert!(observed.lock().unwrap().is_empty(), "no put_chunk call should have happened for a failed open");
    }

    #[test]
    fn unrecognized_local_extension_is_an_error() {
        let path = temp_file("mystery.xyz", b"whatever");
        let cache = SrvCache::new();

        let result = fetch_and_stream(&cache, 1, 2, path.to_str().unwrap(), None);
        assert!(result.is_err());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn content_type_from_mime_maps_known_types() {
        assert_eq!(content_type_from_mime("image/gif", "http://x/y.gif"), Some(ContentType::Gif));
        assert_eq!(content_type_from_mime("image/png", "http://x/y"), Some(ContentType::Png));
        assert_eq!(content_type_from_mime("image/jpeg; charset=binary", "http://x/y"), Some(ContentType::Jpeg));
        assert_eq!(content_type_from_mime("video/mp4", "http://x/y"), Some(ContentType::Video));
        assert_eq!(content_type_from_mime("audio/mpeg", "http://x/y"), Some(ContentType::Audio));
        assert_eq!(content_type_from_mime("text/markdown", "http://x/y"), Some(ContentType::Markdown));
    }

    #[test]
    fn content_type_from_mime_falls_back_to_url_extension_for_generic_types() {
        assert_eq!(content_type_from_mime("text/plain", "http://x/readme.md"), Some(ContentType::Markdown));
        assert_eq!(content_type_from_mime("application/octet-stream", "http://x/readme.md?raw=1"), Some(ContentType::Markdown));
        assert_eq!(content_type_from_mime("", "http://x/readme.md"), Some(ContentType::Markdown));
    }

    #[test]
    fn content_type_from_mime_rejects_unrecognized_types() {
        assert_eq!(content_type_from_mime("application/pdf", "http://x/doc.pdf"), None);
        assert_eq!(content_type_from_mime("text/plain", "http://x/notes.txt"), None);
    }

    #[test]
    fn network_unreachable_host_is_an_error() {
        let cache = SrvCache::new();
        // Port 1 is reserved (tcpmux) and virtually never has a listener
        // — a fast, deterministic connection-refused without needing a
        // real DNS lookup to fail first.
        let result = fetch_and_stream(&cache, 1, 2, "http://127.0.0.1:1/whatever", None);
        assert!(result.is_err());
    }

    /// Binds an ephemeral local port, replies to exactly one HTTP/1.1
    /// request with `body` under `content_type`, then exits — enough to
    /// exercise `fetch_network`'s real header-parsing/streaming path
    /// without pulling in an HTTP server crate for a single test. Returns
    /// the bound `http://127.0.0.1:<port>/` URL.
    fn serve_one_response(content_type: &'static str, body: &'static [u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Drain the request so the client isn't left waiting on a
            // half-closed write side — content doesn't matter, just needs
            // to be read past the request line/headers before responding.
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            use std::io::Write;
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });
        format!("http://{addr}/")
    }

    #[test]
    fn network_fetch_delivers_image_bytes_with_correct_content_type() {
        let url = serve_one_response("image/png", b"fake png bytes from network");
        let cache = SrvCache::new();
        let observed = subscribe_and_collect(&cache);

        fetch_and_stream(&cache, 1, 2, &url, None).unwrap();

        let observed = observed.lock().unwrap();
        let mut chunk_data = Vec::new();
        for response in observed.iter() {
            if let SrvResponse::Progress { chunk_data: data, content_type, total_size, .. } = response {
                chunk_data.extend_from_slice(data);
                assert_eq!(*content_type, ContentType::Png);
                assert_eq!(*total_size, b"fake png bytes from network".len() as u64);
            }
        }
        assert_eq!(chunk_data, b"fake png bytes from network");
    }

    #[test]
    fn network_fetch_delivers_markdown_bytes_with_correct_content_type() {
        let url = serve_one_response("text/markdown", b"# hello from network");
        let cache = SrvCache::new();
        let observed = subscribe_and_collect(&cache);

        fetch_and_stream(&cache, 1, 2, &url, None).unwrap();

        let observed = observed.lock().unwrap();
        let mut chunk_data = Vec::new();
        for response in observed.iter() {
            if let SrvResponse::Progress { chunk_data: data, content_type, .. } = response {
                chunk_data.extend_from_slice(data);
                assert_eq!(*content_type, ContentType::Markdown);
            }
        }
        assert_eq!(chunk_data, b"# hello from network");
    }

    #[test]
    fn network_fetch_unrecognized_content_type_is_an_error() {
        let url = serve_one_response("application/pdf", b"%PDF-1.4");
        let cache = SrvCache::new();

        let result = fetch_and_stream(&cache, 1, 2, &url, None);
        assert!(result.is_err());
    }

    /// Confirms the piece `server::handle_srv_request`'s `FetchResource`
    /// arm is responsible for wiring up: a failed fetch's error message
    /// reaches a `SrvCache` subscriber (the requester's own
    /// `SubscribeProgress` connection) via `notify_fetch_failed`, not
    /// just returned as an `Err` to whoever called `fetch_and_stream`
    /// directly — mirroring exactly what `server.rs`'s real dispatch does
    /// with this function's `Err` result.
    #[test]
    fn failed_fetch_reaches_a_subscriber_not_just_the_requesting_connection() {
        let cache = SrvCache::new();
        let observed = subscribe_and_collect(&cache);

        let result = fetch_and_stream(&cache, 1, 2, "/definitely/does/not/exist.png", None);
        let err = result.expect_err("missing file must fail");
        cache.notify_fetch_failed(1, 2, err.to_string());

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert!(matches!(&observed[0], SrvResponse::FetchFailed { session_id: 1, file_id: 2, .. }));
    }
}
