//! Backing store for `SrvRequest::PutChunk` — the binary side-channel's
//! own on-disk cache, independent of (but path/naming-convention-
//! compatible with) `crates/terminal`'s `RichContentCache`. `som-srv` has
//! no GPUI dependency and cannot call `RichContentCache::apply_chunk`
//! directly on Som's behalf (that type lives in a crate that pulls GPUI
//! in) — so this module duplicates just the gap-tolerant contiguous-
//! length tracking `apply_chunk` already does, writing to the SAME cache
//! directory/file-naming convention so Som's decoders (which open the
//! file directly, e.g. `GrowingFileStream` for video) find it in the
//! expected place regardless of which path (PTY or this side-channel)
//! actually wrote the bytes.
//!
//! Also owns the progress-subscriber registry `SrvRequest::
//! SubscribeProgress` populates — see `server::handle_srv_request` for
//! how a `PutChunk` connection and a `SubscribeProgress` connection (two
//! separate connections, possibly on two separate threads) both reach
//! into the SAME `SrvCache` to write chunks and push progress
//! respectively.

use som_srv::protocol::{ContentMetadata, ContentType, SrvRequest, SrvResponse};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

/// `(session_id, file_id)` — same key shape `RichContentCache` uses.
type CacheKey = (u32, u32);

/// MUST match `crates/terminal/src/rich_content_cache.rs`'s
/// `RichContentCache::extension_for` byte-for-byte — see `put_chunk`'s
/// call site for why.
fn extension_for(content_type: ContentType) -> &'static str {
    match content_type {
        ContentType::Gif => "gif",
        ContentType::Audio => "audio",
        ContentType::Markdown => "md",
        ContentType::Video => "video",
        ContentType::Jpeg => "jpg",
        ContentType::Png => "png",
    }
}

struct CacheEntry {
    file: std::fs::File,
    contiguous_len: u64,
    /// The lowest offset such that everything from here through
    /// `total_size` has arrived — see `SrvResponse::Progress::
    /// tail_available_from`'s own doc comment for why this exists
    /// alongside `contiguous_len` instead of being folded into it.
    /// Starts at `total_size` (nothing confirmed) and only ever shrinks.
    tail_available_from: u64,
    total_size: u64,
    content_type: ContentType,
    metadata: ContentMetadata,
    /// Out-of-order chunks that landed AHEAD of `contiguous_len` — same
    /// "absorb once the watermark catches up" shape `RichContentCache`
    /// already uses, kept small in practice (a real sender streams
    /// mostly-in-order; large reorderings would need a smarter structure,
    /// not expected here any more than in the existing PTY path).
    pending_ranges: Vec<(u64, u64)>,
}

/// One progress subscriber: a `SrvResponse::Progress` push goes to
/// EVERY subscriber currently registered for a given `(session_id,
/// file_id)`, not just the first — in practice there's usually exactly
/// one (Som's own subscription), but nothing here assumes that.
type ProgressSender = Arc<dyn Fn(SrvResponse) -> anyhow::Result<()> + Send + Sync>;

/// The connection that first sent a `PutChunk` for a given `(session_id,
/// file_id)` — the ONE place a later `SrvRequest::RequestByteRange` can
/// be forwarded to for more bytes. See `route_byte_range_request`'s doc
/// comment for why there's exactly one route per key (not a list like
/// `ProgressSender`): only the client actually holding the file can
/// answer a byte-range request, unlike progress, which any number of
/// subscribers can independently want to observe.
type SenderRoute = Arc<dyn Fn(SrvRequest) -> anyhow::Result<()> + Send + Sync>;

#[derive(Default)]
struct Inner {
    entries: HashMap<CacheKey, CacheEntry>,
    subscribers: HashMap<CacheKey, Vec<ProgressSender>>,
    sender_routes: HashMap<CacheKey, SenderRoute>,
    /// Populated only by an explicit `SrvRequest::RegisterRangeResponder`
    /// — see that variant's own doc comment for why a byte-range
    /// response needs a connection separate from `sender_routes`' (the
    /// sequential `PutChunk` stream's own connection, which can be
    /// saturated with outgoing chunks for minutes on a large file).
    /// `route_byte_range_request` checks this FIRST, falling back to
    /// `sender_routes` only if no responder has registered (keeps this
    /// backward-compatible with any sender that never sends `Register
    /// RangeResponder` at all).
    range_response_routes: HashMap<CacheKey, SenderRoute>,
}

/// Shared, thread-safe handle — cloned into every `Srv`-kind connection
/// handler thread, same sharing pattern `SessionRegistry` already uses
/// for PTY sessions.
#[derive(Clone, Default)]
pub struct SrvCache {
    inner: Arc<Mutex<Inner>>,
}

impl SrvCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `~/.config/som/media_cache/` — the SAME path
    /// `crates/terminal/src/terminal.rs`'s `rich_content_cache_dir()`
    /// already writes to over the PTY path (`paths::config_dir().join(
    /// "media_cache")`), so a decoder on the Som side finds a file here
    /// regardless of which path actually wrote it.
    /// `SOM_RICH_CONTENT_CACHE_DIR` overrides the real
    /// `paths::config_dir()/media_cache` default when set — the ONLY way
    /// a headless `#[gpui::test]` (real `som-srv`/`somcat` child
    /// processes, `crates/terminal`'s own `rich_content_cache_dir()`
    /// pointed at a `std::env::temp_dir()`-based test directory instead
    /// of the real config dir) can make `som-srv` — a separate compiled
    /// binary with no `cfg(test)` visibility into `crates/terminal`'s
    /// test-only override — agree on the SAME directory. Without this,
    /// `som-srv` writes chunks to the real user config dir while
    /// `RichContentCache::record_progress` looks for them under
    /// `temp_dir()`, and every `record_progress` call fails with "file
    /// not found" (confirmed the hard way as every rich-content
    /// `#[gpui::test]` failing this exact way once the binary
    /// side-channel replaced the old APC/PTY path, where this discrepancy
    /// never mattered — the old path never had a SECOND process's own
    /// cache-dir default to keep in sync with).
    pub fn default_cache_dir() -> std::path::PathBuf {
        if let Ok(dir) = std::env::var("SOM_RICH_CONTENT_CACHE_DIR") {
            return std::path::PathBuf::from(dir);
        }
        paths::config_dir().join("media_cache")
    }

    /// Applies one `PutChunk`: opens (or reuses) the cache file for
    /// `(session_id, file_id)` under `cache_dir`, writes `data` at
    /// `offset`, advances the contiguous-length watermark exactly the
    /// way `RichContentCache::apply_chunk` already does (gap-tolerant,
    /// absorbing previously-out-of-order ranges as the watermark catches
    /// up), then pushes `SrvResponse::Progress` (carrying `content_type`/
    /// `metadata` — see `PutChunk`'s own doc comment for why these
    /// travel on every chunk) to every subscriber for this key if the
    /// watermark actually moved.
    #[allow(clippy::too_many_arguments)]
    pub fn put_chunk(
        &self,
        cache_dir: &std::path::Path,
        session_id: u32,
        file_id: u32,
        offset: u64,
        data: &[u8],
        total_size: u64,
        content_type: ContentType,
        metadata: ContentMetadata,
    ) -> anyhow::Result<()> {
        let key = (session_id, file_id);
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        if !inner.entries.contains_key(&key) {
            std::fs::create_dir_all(cache_dir)?;
            // MUST match `crates/terminal/src/rich_content_cache.rs`'s
            // `RichContentCache::extension_for` byte-for-byte — a decoder
            // on the Som side opens this exact path by its OWN
            // `{session:08x}-{file:08x}.<ext>` naming convention, and the
            // two caches need to agree on `<ext>` since they now write to
            // (and read from) the SAME directory for the SAME transfer.
            let path = cache_dir.join(format!("{session_id:08x}-{file_id:08x}.{}", extension_for(content_type)));
            let file = OpenOptions::new().create(true).write(true).truncate(true).open(&path)?;
            inner.entries.insert(
                key,
                CacheEntry {
                    file,
                    contiguous_len: 0,
                    tail_available_from: total_size,
                    total_size,
                    content_type,
                    metadata,
                    pending_ranges: Vec::new(),
                },
            );
        }

        let entry = inner.entries.get_mut(&key).expect("just inserted above if absent");
        entry.total_size = total_size;
        entry.content_type = content_type;
        entry.metadata = metadata;
        entry.file.seek(SeekFrom::Start(offset))?;
        entry.file.write_all(data)?;

        let chunk_end = offset + data.len() as u64;
        let watermark_before = entry.contiguous_len;
        let tail_before = entry.tail_available_from;
        let pending_before = entry.pending_ranges.clone();
        if offset <= entry.contiguous_len {
            entry.contiguous_len = entry.contiguous_len.max(chunk_end);
            loop {
                let Some(pos) = entry.pending_ranges.iter().position(|&(start, _)| start <= entry.contiguous_len) else {
                    break;
                };
                let (start, end) = entry.pending_ranges.remove(pos);
                entry.contiguous_len = entry.contiguous_len.max(end.max(start));
            }
        } else {
            entry.pending_ranges.push((offset, chunk_end));
        }
        // Same gap-tolerant merge as `contiguous_len` above, but growing
        // BACKWARD from `total_size` instead of forward from 0 — see
        // `tail_available_from`'s own doc comment for why this needs to
        // exist independently rather than being derived from
        // `contiguous_len`. Deliberately NON-destructive (unlike the
        // front merge above, this loop never calls `pending_ranges.
        // remove`) — `pending_ranges` is the SAME list the front merge
        // reads, and a range can be exactly what BOTH watermarks need to
        // advance through (a chunk that happens to bridge toward the
        // tail today might just as easily be the missing piece a later,
        // still-arriving chunk needs to bridge `contiguous_len` through
        // from the front) — removing it here would make it invisible to
        // that later front-side merge. Confirmed live as a real bug: an
        // out-of-order tail chunk's own `put_chunk` call consumed the
        // pending range a SUBSEQUENT front-filling chunk needed to see,
        // leaving `contiguous_len` stuck one merge short.
        if chunk_end >= entry.tail_available_from {
            entry.tail_available_from = entry.tail_available_from.min(offset);
            loop {
                let shrink =
                    entry.pending_ranges.iter().find(|&&(start, end)| end >= entry.tail_available_from && start < entry.tail_available_from).map(|&(start, _)| start);
                let Some(start) = shrink else {
                    break;
                };
                entry.tail_available_from = start;
            }
        }

        if entry.contiguous_len != watermark_before
            || entry.tail_available_from != tail_before
            || entry.pending_ranges != pending_before
        {
            let contiguous_len = entry.contiguous_len;
            let tail_available_from = entry.tail_available_from;
            let pending_ranges = entry.pending_ranges.clone();
            let total_size = entry.total_size;
            if let Some(subscribers) = inner.subscribers.get(&key) {
                for subscriber in subscribers {
                    let _ = subscriber(SrvResponse::Progress {
                        session_id,
                        file_id,
                        contiguous_len,
                        tail_available_from,
                        pending_ranges: pending_ranges.clone(),
                        total_size,
                        content_type,
                        metadata,
                    });
                }
            }
        }

        Ok(())
    }

    /// Registers `send` to receive every future `SrvResponse::Progress`
    /// push for `(session_id, file_id)`, AND immediately replays the
    /// current watermark if any chunk for this key has already landed —
    /// a subscriber arriving after the fact (a real race, not a
    /// hypothetical one: confirmed live for small, non-keep-alive
    /// transfers like a single image/GIF, where `somcat` can finish
    /// streaming the ENTIRE file and close its connection before Som's
    /// side ever gets around to sending `SubscribeProgress` for a
    /// placement it only just noticed in the placeholder grid) still
    /// needs to learn the true current state, since nothing else will
    /// ever push it again once the sender's connection is gone.
    pub fn subscribe(&self, session_id: u32, file_id: u32, send: ProgressSender) {
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (session_id, file_id);
        if let Some(entry) = inner.entries.get(&key) {
            let _ = send(SrvResponse::Progress {
                session_id,
                file_id,
                contiguous_len: entry.contiguous_len,
                tail_available_from: entry.tail_available_from,
                pending_ranges: entry.pending_ranges.clone(),
                total_size: entry.total_size,
                content_type: entry.content_type,
                metadata: entry.metadata,
            });
        }
        inner.subscribers.entry(key).or_default().push(send);
    }

    /// Registers `send` as the ONE way to reach the client currently
    /// holding `(session_id, file_id)`'s file — called the first time a
    /// connection's `PutChunk` for a given key is seen (see `server::
    /// handle_srv_request`'s call site). Later `PutChunk`s for the same
    /// key from the SAME connection don't re-register (harmless either
    /// way — `insert` just overwrites with an equivalent closure — but
    /// the call site only does this once for clarity). If a DIFFERENT
    /// connection later sends `PutChunk`s for the same key (e.g. a retry
    /// after the original sender crashed), this naturally replaces the
    /// stale route with the new one, since there is only ever one entry
    /// per key.
    pub fn register_sender_route(&self, session_id: u32, file_id: u32, send: SenderRoute) {
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.sender_routes.insert((session_id, file_id), send);
    }

    /// Registers `send` as the DEDICATED route for `SrvRequest::
    /// RequestByteRange` forwarding — see `SrvRequest::
    /// RegisterRangeResponder`'s own doc comment for why this exists
    /// separately from `register_sender_route`.
    pub fn register_range_responder_route(&self, session_id: u32, file_id: u32, send: SenderRoute) {
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.range_response_routes.insert((session_id, file_id), send);
    }

    /// Forwards `request` to `(session_id, file_id)`'s registered range-
    /// responder route if one was explicitly registered (see `SrvRequest::
    /// RegisterRangeResponder`'s own doc comment for why that's preferred
    /// — it's on a connection dedicated to range responses, uncontended
    /// by the sequential `PutChunk` stream), falling back to the plain
    /// sender route otherwise (a sender that never registers a dedicated
    /// responder — images/GIF, or an older `somcat` build — still gets a
    /// working, just non-prioritized, byte-range reply). A miss on BOTH
    /// (no route registered at all, or the registered route's underlying
    /// connection has since closed and its `send` call fails) is silently
    /// swallowed, not an error: see `SrvRequest::RequestByteRange`'s own
    /// doc comment for why an undeliverable range request is an accepted
    /// gap, not a new failure mode, mirroring the OLD PTY `Query`
    /// mechanism's identical tolerance for a query that simply never
    /// gets answered.
    pub fn route_byte_range_request(&self, session_id: u32, file_id: u32, request: SrvRequest) {
        let inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (session_id, file_id);
        if let Some(route) = inner.range_response_routes.get(&key) {
            let _ = route(request);
            return;
        }
        if let Some(route) = inner.sender_routes.get(&key) {
            let _ = route(request);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_byte_range_request_forwards_to_the_registered_sender() {
        let cache = SrvCache::new();
        let observed = Arc::new(Mutex::new(Vec::new()));

        {
            let observed = observed.clone();
            cache.register_sender_route(
                1,
                2,
                Arc::new(move |request| {
                    observed.lock().unwrap().push(request);
                    Ok(())
                }),
            );
        }

        let request = SrvRequest::RequestByteRange { session_id: 1, file_id: 2, offset: 100, len: 50 };
        cache.route_byte_range_request(1, 2, request.clone());

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert!(matches!(observed[0], SrvRequest::RequestByteRange { session_id: 1, file_id: 2, offset: 100, len: 50 }));
    }

    #[test]
    fn route_byte_range_request_is_a_silent_no_op_with_no_registered_sender() {
        let cache = SrvCache::new();
        // No `register_sender_route` call at all — must not panic or error.
        cache.route_byte_range_request(99, 99, SrvRequest::RequestByteRange { session_id: 99, file_id: 99, offset: 0, len: 10 });
    }

    #[test]
    fn a_later_sender_route_registration_replaces_the_earlier_one() {
        let cache = SrvCache::new();
        let first_calls = Arc::new(Mutex::new(0));
        let second_calls = Arc::new(Mutex::new(0));

        {
            let first_calls = first_calls.clone();
            cache.register_sender_route(
                1,
                2,
                Arc::new(move |_| {
                    *first_calls.lock().unwrap() += 1;
                    Ok(())
                }),
            );
        }
        {
            let second_calls = second_calls.clone();
            cache.register_sender_route(
                1,
                2,
                Arc::new(move |_| {
                    *second_calls.lock().unwrap() += 1;
                    Ok(())
                }),
            );
        }

        cache.route_byte_range_request(1, 2, SrvRequest::RequestByteRange { session_id: 1, file_id: 2, offset: 0, len: 1 });

        assert_eq!(*first_calls.lock().unwrap(), 0, "the replaced route must never be called");
        assert_eq!(*second_calls.lock().unwrap(), 1, "only the latest registered route must be called");
    }

    fn test_metadata() -> ContentMetadata {
        ContentMetadata::Image { width_px: 1, height_px: 1, color_bits: 32, is_animated: false }
    }

    #[test]
    fn put_chunk_writes_bytes_at_the_given_offset() {
        let dir = std::env::temp_dir().join(format!("som-srv-cache-test-{}", uuid::Uuid::new_v4()));
        let cache = SrvCache::new();

        cache.put_chunk(&dir, 1, 2, 0, b"hello", 10, ContentType::Gif, test_metadata()).unwrap();
        cache.put_chunk(&dir, 1, 2, 5, b"world", 10, ContentType::Gif, test_metadata()).unwrap();

        let path = dir.join("00000001-00000002.gif");
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"helloworld");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn put_chunk_tracks_contiguous_len_across_out_of_order_chunks() {
        let dir = std::env::temp_dir().join(format!("som-srv-cache-test-{}", uuid::Uuid::new_v4()));
        let cache = SrvCache::new();
        let observed = Arc::new(Mutex::new(Vec::new()));

        {
            let observed = observed.clone();
            cache.subscribe(
                1,
                2,
                Arc::new(move |response| {
                    // A `Progress` push now fires whenever EITHER
                    // watermark moves (see `tail_available_from`'s own
                    // doc comment) — only record a NEW `contiguous_len`
                    // value here, not every push, so this test can stay
                    // focused on `contiguous_len`'s own forward-only
                    // watermark without being tripped up by a push that
                    // fired purely because `tail_available_from` moved.
                    if let SrvResponse::Progress { contiguous_len, .. } = response {
                        let mut observed = observed.lock().unwrap();
                        if observed.last().copied().unwrap_or(0) != contiguous_len {
                            observed.push(contiguous_len);
                        }
                    }
                    Ok(())
                }),
            );
        }

        // Second chunk arrives first — out of order relative to
        // `contiguous_len` (which only ever grows from 0), though NOT
        // out of order relative to `tail_available_from` (which grows
        // from `total_size` backward) — this chunk happens to also be
        // the file's tail, so a `Progress` push DOES fire, just not one
        // that advances `contiguous_len`; this test only asserts on
        // `contiguous_len`, see `put_chunk_tracks_tail_available_from_
        // independently_of_contiguous_len` for the tail watermark's own
        // coverage.
        cache.put_chunk(&dir, 1, 2, 5, b"world", 10, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), Vec::<u64>::new(), "an out-of-order chunk must not advance contiguous_len");

        cache.put_chunk(&dir, 1, 2, 0, b"hello", 10, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![10], "the watermark must jump straight to 10 once the gap is filled");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Companion to `put_chunk_tracks_contiguous_len_across_out_of_order_
    /// chunks` — covers `tail_available_from`'s own independent
    /// backward-growing watermark, added for the live-confirmed MKV bug
    /// (`SrvResponse::Progress::tail_available_from`'s own doc comment
    /// has the full story): a chunk landing at the file's tail must
    /// advance `tail_available_from` immediately, even though the exact
    /// same chunk, being out of order from the FRONT, correctly does
    /// nothing to `contiguous_len`.
    #[test]
    fn put_chunk_tracks_tail_available_from_independently_of_contiguous_len() {
        let dir = std::env::temp_dir().join(format!("som-srv-cache-test-{}", uuid::Uuid::new_v4()));
        let cache = SrvCache::new();
        let observed = Arc::new(Mutex::new(Vec::new()));

        {
            let observed = observed.clone();
            cache.subscribe(
                1,
                2,
                Arc::new(move |response| {
                    // Same dedup as the companion test above — a push now
                    // fires whenever EITHER watermark moves, so only
                    // record a NEW `tail_available_from` value, not every
                    // push (the leading chunk below legitimately advances
                    // `contiguous_len` without touching `tail_available_
                    // from`, which would otherwise show up here as a
                    // spurious repeat of the unchanged value).
                    if let SrvResponse::Progress { tail_available_from, total_size, .. } = response {
                        let mut observed = observed.lock().unwrap();
                        if observed.last().copied().unwrap_or(total_size) != tail_available_from {
                            observed.push(tail_available_from);
                        }
                    }
                    Ok(())
                }),
            );
        }

        // Leading chunk first — advances `contiguous_len` (offset 0),
        // but doesn't touch the tail (chunk_end=5 != total_size=15), so
        // `tail_available_from` itself (starting at total_size=15) must
        // not move.
        cache.put_chunk(&dir, 1, 2, 0, b"hello", 15, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), Vec::<u64>::new(), "a leading chunk must not advance tail_available_from");

        // Tail chunk arrives NEXT (with a gap still open in the middle,
        // offset 5..10 unwritten) — offset 10, length 5, chunk_end ==
        // total_size (15) — must advance tail_available_from to 10
        // immediately, a sequential send starting from 0 would still be
        // nowhere near this offset.
        cache.put_chunk(&dir, 1, 2, 10, b"!!!!!", 15, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![10], "a chunk touching the tail must advance tail_available_from immediately");

        // Middle chunk connects the leading and tail chunks already on
        // disk — since it's contiguous with BOTH `contiguous_len` (which
        // reaches offset 5) and the tail chunk (which starts at offset
        // 10), it advances `tail_available_from` down to its own start
        // (5) directly — there's no gap left in `pending_ranges` to
        // additionally merge through here, since the earlier tail chunk
        // already got folded in when IT arrived (see the assertion just
        // above). (`contiguous_len` itself also reaches 15 here — not
        // this test's concern, see the companion test.)
        cache.put_chunk(&dir, 1, 2, 5, b"world", 15, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![10, 5], "tail_available_from must advance to the connecting chunk's own start");

        std::fs::remove_dir_all(&dir).ok();
    }
}
