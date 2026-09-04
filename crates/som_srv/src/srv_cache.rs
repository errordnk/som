//! Backing store for `SrvRequest::PutChunk` — the binary side-channel's
//! own in-memory watermark tracker for a transfer in progress. `som-srv`
//! deliberately does NOT persist chunk bytes anywhere (no disk file, no
//! backing store beyond the duration of a single `put_chunk` call) — it
//! only tracks the gap-tolerant contiguous-length bookkeeping (mirroring
//! what `crates/terminal`'s `RichContentCache::apply_chunk` used to also
//! do on the PTY path) and forwards each chunk's own bytes, unchanged,
//! to every `SubscribeProgress` subscriber as part of the SAME
//! `SrvResponse::Progress` push that already carries the watermark
//! numbers (see that variant's own `chunk_offset`/`chunk_data` fields).
//! A subscriber (Som's `SrvProgressState`) is the only place these bytes
//! end up materialized anywhere durable — its own bounded, forward-only
//! in-memory buffer, never a disk file. This replaced an earlier design
//! that wrote every chunk to `~/.config/som/media_cache/` as a full,
//! never-evicted on-disk copy of whatever was streaming — confirmed live
//! as the actual cause of a 16GB video failing to play at all once that
//! cache directory (having never been cleaned up across many sessions)
//! filled the disk. `som-srv` has no GPUI dependency and cannot call
//! into `crates/terminal` directly on Som's behalf (that crate pulls
//! GPUI in), which is why this bookkeeping is duplicated here rather
//! than shared — it was always going to be a separate implementation of
//! the same watermark logic, disk-backed or not.
//!
//! Also owns the progress-subscriber registry `SrvRequest::
//! SubscribeProgress` populates — see `server::handle_srv_request` for
//! how a `PutChunk` connection and a `SubscribeProgress` connection (two
//! separate connections, possibly on two separate threads) both reach
//! into the SAME `SrvCache` to write chunks and push progress
//! respectively.

use som_srv::protocol::{ContentMetadata, ContentType, SrvRequest, SrvResponse};
use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

/// How many of the most-recently-forwarded bytes `CacheEntry::recent_
/// bytes` keeps around for `Video`/`Audio` (see that field's own doc
/// comment for why). Small on purpose — this is NOT a return to the old
/// unbounded-disk-cache design (the whole point of Part 0's redesign was
/// escaping that), just enough of a trailing window that a `RequestByteRange`
/// answering a stuck reader is very likely to land inside it. 8MB comfortably
/// covers a `GrowingFileStream`/`GrowingSrvStream` on-demand fetch's own
/// `ON_DEMAND_RANGE_LEN` (4MB) with room to spare.
const RECENT_BYTES_WINDOW: usize = 8 * 1024 * 1024;

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
    /// `None` for `Video`/`Audio` — those two content types are never
    /// written to disk at all (see this module's own doc comment for
    /// the 371GB-media_cache incident that motivated it). `Some` for
    /// every other content type (image/GIF/markdown), which stay
    /// disk-backed: `crates/terminal`'s `RichContentCache::record_
    /// progress` opens this SAME file by the same naming convention to
    /// read decoded pixel/text data back out — confirmed live as a real
    /// regression when this was made unconditionally `None` for every
    /// content type: `record_progress` then failed to `open()` a file
    /// that was never created, silently breaking even a plain PNG/GIF
    /// placement (not just video/audio, which were the only content
    /// types actually in scope for going memory-only).
    file: Option<std::fs::File>,
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
    /// A bounded (`RECENT_BYTES_WINDOW`) trailing window of the most
    /// recently forwarded bytes, `Video`/`Audio` only (`None` for other
    /// content types, which stay disk-backed via `file` above and don't
    /// need this). Exists to answer a `RequestByteRange` DIRECTLY when no
    /// sender is still connected to ask instead — confirmed live as a
    /// real gap: a short/fast transfer (e.g. a 1-second audio fixture)
    /// can have `somcat` finish streaming and exit before Som even
    /// subscribes, so `subscribe()`'s own watermark-only replay (see its
    /// doc comment) leaves the receiver's buffer permanently empty with
    /// no live sender left to answer an on-demand fetch — `contiguous_
    /// len`/`total_size` say every byte "arrived," but none of them
    /// actually did. This is a genuinely small, bounded amount of memory
    /// (not a return to the old unbounded-disk-cache design that
    /// motivated removing `file` for these two content types in the
    /// first place) — see `RECENT_BYTES_WINDOW`'s own doc comment.
    /// `(first_byte_offset, bytes)` — `first_byte_offset` is the absolute
    /// file offset `bytes`'s own first byte corresponds to.
    recent_bytes: Option<(u64, VecDeque<u8>)>,
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

    /// `~/.config/som/media_cache/` — the SAME path `crates/terminal`'s
    /// `rich_content_cache_dir()` already writes to, so `RichContentCache::
    /// record_progress` finds a file here for whichever content types
    /// `put_chunk` still creates one for (image/GIF/markdown — see
    /// `CacheEntry::file`'s own doc comment for why video/audio don't).
    /// `SOM_RICH_CONTENT_CACHE_DIR` overrides the real path for tests.
    pub fn default_cache_dir() -> std::path::PathBuf {
        if let Ok(dir) = std::env::var("SOM_RICH_CONTENT_CACHE_DIR") {
            return std::path::PathBuf::from(dir);
        }
        paths::config_dir().join("media_cache")
    }

    /// Applies one `PutChunk`: for `Video`/`Audio`, only advances the
    /// contiguous-length watermark and forwards this chunk's bytes to
    /// subscribers — no disk file at all (see `CacheEntry::file`'s own
    /// doc comment for the incident that motivated this). For every
    /// OTHER content type (image/GIF/markdown), also opens/writes a
    /// disk file under `cache_dir` exactly like `RichContentCache::
    /// apply_chunk` used to on the PTY path — `crates/terminal`'s
    /// `RichContentCache::record_progress` reads decoded pixel/text data
    /// back out of that same file, it never consults `SrvProgressState`'s
    /// in-memory buffer the way video/audio's decoders do. Either way,
    /// pushes `SrvResponse::Progress` (carrying `content_type`/`metadata`
    /// — see `PutChunk`'s own doc comment for why these travel on every
    /// chunk) to every subscriber for this key; the push ALWAYS carries
    /// this chunk's own `data` (via `chunk_offset`/`chunk_data`)
    /// regardless of whether either watermark moved — a video/audio
    /// subscriber's in-memory buffer needs every chunk's bytes
    /// delivered, not just the ones that happen to advance
    /// `contiguous_len`/`tail_available_from`.
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
            let file = if matches!(content_type, ContentType::Video | ContentType::Audio) {
                None
            } else {
                std::fs::create_dir_all(cache_dir)?;
                // MUST match `crates/terminal/src/rich_content_cache.rs`'s
                // `RichContentCache::extension_for` byte-for-byte — a
                // decoder on the Som side opens this exact path by its
                // OWN `{session:08x}-{file:08x}.<ext>` naming convention.
                let path = cache_dir.join(format!("{session_id:08x}-{file_id:08x}.{}", extension_for(content_type)));
                Some(OpenOptions::new().create(true).write(true).truncate(true).open(&path)?)
            };
            let recent_bytes = if matches!(content_type, ContentType::Video | ContentType::Audio) {
                Some((0, VecDeque::new()))
            } else {
                None
            };
            inner.entries.insert(
                key,
                CacheEntry {
                    file,
                    contiguous_len: 0,
                    tail_available_from: total_size,
                    total_size,
                    content_type,
                    metadata: metadata.clone(),
                    pending_ranges: Vec::new(),
                    recent_bytes,
                },
            );
        }

        let entry = inner.entries.get_mut(&key).expect("just inserted above if absent");
        entry.total_size = total_size;
        entry.content_type = content_type;
        entry.metadata = metadata.clone();
        if let Some(file) = entry.file.as_mut() {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(data)?;
        }
        if let Some((window_start, window)) = entry.recent_bytes.as_mut() {
            // Only track a chunk that lands contiguously at (or behind)
            // this window's own tail — an out-of-order chunk landing
            // ahead has nowhere correct to go in a plain trailing window
            // (same simplification `SrvProgressState::append_chunk`
            // makes on the Som side for its own ahead-of-tail case); this
            // window only needs to cover MOST recent bytes well enough to
            // answer a stuck on-demand fetch, not be a perfect record.
            let window_end = *window_start + window.len() as u64;
            if offset == window_end {
                window.extend(data.iter().copied());
            } else if offset < window_end {
                // Overlaps/retransmits part of what's already tracked —
                // harmless no-op, same tolerance `SrvProgressState::
                // append_chunk`'s own `chunk_offset < tail` case documents.
            } else if window.is_empty() {
                *window_start = offset;
                window.extend(data.iter().copied());
            }
            if window.len() > RECENT_BYTES_WINDOW {
                let drop_count = window.len() - RECENT_BYTES_WINDOW;
                window.drain(..drop_count);
                *window_start += drop_count as u64;
            }
        }

        let chunk_end = offset + data.len() as u64;
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
                    metadata: metadata.clone(),
                    chunk_offset: offset,
                    chunk_data: data.to_vec(),
                });
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
    /// The replay below carries the watermark numbers only, `chunk_data`
    /// always empty (`chunk_offset: 0, chunk_data: Vec::new()`) — this
    /// cache no longer retains ANY chunk's actual bytes once `put_chunk`
    /// returns (see this module's own doc comment), so there is nothing
    /// to replay for a late subscriber beyond the watermark state itself.
    /// This is not a gap in practice: a subscriber that sees `contiguous_
    /// len > 0`/`tail_available_from < total_size` with an empty local
    /// buffer already has exactly the tool it needs to catch up —
    /// `SrvRequest::RequestByteRange` — the SAME mechanism a genuinely
    /// uncached forward/backward seek already relies on. `GrowingFileStream::
    /// read`'s existing "stuck at a position the buffer doesn't cover yet"
    /// branch fires identically whether the gap is because a chunk hasn't
    /// arrived YET or because it arrived before this subscription existed
    /// and was already forwarded-then-dropped — both look the same from
    /// the reader's point of view: bytes it wants aren't in the buffer,
    /// ask for them.
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
                metadata: entry.metadata.clone(),
                chunk_offset: 0,
                chunk_data: Vec::new(),
            });
        }
        inner.subscribers.entry(key).or_default().push(send);
    }

    /// Pushes `SrvResponse::StopPlayback` to every current subscriber of
    /// `(session_id, file_id)` — see `SrvRequest::StopPlayback`'s own doc
    /// comment for the full rationale. Unlike `push_chunk`'s `Progress`
    /// push, this is NOT gated on any watermark actually changing — it
    /// fires unconditionally, once, whenever a `StopPlayback` request
    /// arrives, since there's no "already sent this" state to compare
    /// against the way there is for progress (which naturally
    /// deduplicates via the watermark-changed check). A subscriber whose
    /// `send` call fails (connection already closed) is silently skipped,
    /// same tolerance every other best-effort push in this cache already
    /// has.
    pub fn notify_stop_playback(&self, session_id: u32, file_id: u32) {
        let inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(subscribers) = inner.subscribers.get(&(session_id, file_id)) {
            for subscriber in subscribers {
                let _ = subscriber(SrvResponse::StopPlayback { session_id, file_id });
            }
        }
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
        let SrvRequest::RequestByteRange { offset, len, .. } = request else {
            return;
        };
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (session_id, file_id);
        // A route being PRESENT in either map doesn't mean its underlying
        // connection is still alive — neither map is cleaned up on
        // disconnect (see `CacheEntry::recent_bytes`'s own doc comment
        // for the confirmed-live scenario this matters for), so `route(..)`
        // failing is the actual signal a sender is really gone, not just
        // "no entry" — checking the `Result` here (unlike every other
        // call site's `let _ = ...`, which doesn't need to react to
        // failure) is what lets this fall through to the in-memory
        // window below instead of the request silently vanishing.
        if let Some(route) = inner.range_response_routes.get(&key)
            && route(request.clone()).is_ok()
        {
            return;
        }
        if let Some(route) = inner.sender_routes.get(&key)
            && route(request).is_ok()
        {
            return;
        }
        // No live sender left to ask — answer directly from the trailing
        // window if the request falls inside it, exactly as if a real
        // sender had replied with an ordinary out-of-order `PutChunk`.
        self.serve_from_recent_bytes(&mut inner, session_id, file_id, offset, len);
    }

    /// Forwards `SrvRequest::EndPlayback` to `(session_id, file_id)`'s
    /// registered range-responder route — the ONLY route this can go to
    /// (unlike `route_byte_range_request`, there's no `sender_routes`/
    /// recent-bytes fallback: `EndPlayback` only means anything to a
    /// pull-model responder still alive and listening; if none is, the
    /// source is already gone and there's nothing to tell). Silently a
    /// no-op on a miss, same tolerance every other best-effort message
    /// here has — see `SrvRequest::EndPlayback`'s own doc comment.
    pub fn route_end_playback(&self, session_id: u32, file_id: u32) {
        let inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (session_id, file_id);
        if let Some(route) = inner.range_response_routes.get(&key) {
            let _ = route(SrvRequest::EndPlayback { session_id, file_id });
        }
    }

    fn serve_from_recent_bytes(&self, inner: &mut Inner, session_id: u32, file_id: u32, offset: u64, len: u64) {
        let key = (session_id, file_id);
        let Some(entry) = inner.entries.get(&key) else { return };
        let Some((window_start, window)) = entry.recent_bytes.as_ref() else { return };
        if offset < *window_start {
            return; // Already evicted from the window — nothing to serve.
        }
        let skip = (offset - window_start) as usize;
        if skip >= window.len() {
            return; // Past what's been buffered so far — nothing to serve yet.
        }
        let available = (window.len() - skip).min(len as usize);
        let chunk_data: Vec<u8> = window.iter().skip(skip).take(available).copied().collect();
        let contiguous_len = entry.contiguous_len;
        let tail_available_from = entry.tail_available_from;
        let pending_ranges = entry.pending_ranges.clone();
        let total_size = entry.total_size;
        let content_type = entry.content_type;
        let metadata = entry.metadata.clone();
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
                    metadata: metadata.clone(),
                    chunk_offset: offset,
                    chunk_data: chunk_data.clone(),
                });
            }
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

    #[test]
    fn notify_stop_playback_pushes_to_every_subscriber() {
        let cache = SrvCache::new();
        let observed = Arc::new(Mutex::new(Vec::new()));

        {
            let observed = observed.clone();
            cache.subscribe(1, 2, Arc::new(move |response| {
                observed.lock().unwrap().push(response);
                Ok(())
            }));
        }

        cache.notify_stop_playback(1, 2);

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert!(matches!(observed[0], SrvResponse::StopPlayback { session_id: 1, file_id: 2 }));
    }

    #[test]
    fn notify_stop_playback_is_a_silent_no_op_with_no_subscriber() {
        let cache = SrvCache::new();
        // No `subscribe` call at all — must not panic or error.
        cache.notify_stop_playback(99, 99);
    }

    #[test]
    fn route_byte_range_request_serves_from_recent_bytes_once_the_sender_route_fails() {
        // Regression test for a real, confirmed-live gap: a short/fast
        // Video/Audio transfer can finish and disconnect before Som ever
        // subscribes — `subscribe()`'s watermark-only replay leaves the
        // receiver's own buffer empty with no live sender left to answer
        // an on-demand `RequestByteRange`. `CacheEntry::recent_bytes`
        // exists to answer directly in exactly this case.
        let cache = SrvCache::new();
        let cache_dir = test_cache_dir();

        // A sender route IS registered (mirrors a real `PutChunk`
        // connection having existed), but its `send` closure now fails —
        // simulating the underlying connection having already closed,
        // which is the actual, confirmed-live scenario (neither
        // `sender_routes` nor `range_response_routes` gets cleaned up on
        // disconnect).
        cache.register_sender_route(1, 2, Arc::new(|_| anyhow::bail!("connection closed")));

        cache
            .put_chunk(&cache_dir, 1, 2, 0, b"hello world", 11, ContentType::Audio, ContentMetadata::Audio {
                sample_rate: 44100,
                channels: 2,
                bits_per_sample: 16,
                duration_ms: 1000,
                extension: "flac".to_string(),
            })
            .unwrap();

        let observed = Arc::new(Mutex::new(Vec::new()));
        {
            let observed = observed.clone();
            cache.subscribe(1, 2, Arc::new(move |response| {
                observed.lock().unwrap().push(response);
                Ok(())
            }));
        }
        // `subscribe`'s own initial replay (empty `chunk_data`, see that
        // method's doc comment) is the first push — clear it so this
        // test only asserts on the `route_byte_range_request` reply.
        observed.lock().unwrap().clear();

        cache.route_byte_range_request(1, 2, SrvRequest::RequestByteRange { session_id: 1, file_id: 2, offset: 0, len: 11 });

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1, "the in-memory window must answer once the registered route fails");
        let SrvResponse::Progress { chunk_offset, chunk_data, .. } = &observed[0] else {
            panic!("expected a Progress push, got {:?}", observed[0]);
        };
        assert_eq!(*chunk_offset, 0);
        assert_eq!(chunk_data, b"hello world");
    }

    #[test]
    fn route_byte_range_request_does_not_serve_recent_bytes_for_disk_backed_content_types() {
        // Image/GIF/Markdown keep their disk file even after the sender
        // disconnects (`RichContentCache::path()` still finds it) — they
        // don't need `recent_bytes` at all, and it must stay `None` for
        // them so this fallback correctly stays a no-op.
        let cache = SrvCache::new();
        let cache_dir = test_cache_dir();
        cache.register_sender_route(1, 2, Arc::new(|_| anyhow::bail!("connection closed")));
        cache.put_chunk(&cache_dir, 1, 2, 0, b"hello world", 11, ContentType::Png, test_metadata()).unwrap();

        let observed = Arc::new(Mutex::new(Vec::new()));
        {
            let observed = observed.clone();
            cache.subscribe(1, 2, Arc::new(move |response| {
                observed.lock().unwrap().push(response);
                Ok(())
            }));
        }
        observed.lock().unwrap().clear();

        cache.route_byte_range_request(1, 2, SrvRequest::RequestByteRange { session_id: 1, file_id: 2, offset: 0, len: 11 });

        assert!(observed.lock().unwrap().is_empty(), "image/GIF/markdown must not be served from recent_bytes");
    }

    fn test_metadata() -> ContentMetadata {
        ContentMetadata::Image { width_px: 1, height_px: 1, color_bits: 32, is_animated: false }
    }

    /// A fresh, per-test temp directory for `put_chunk`'s `cache_dir`
    /// parameter — most tests here don't care about disk state at all
    /// (they only exercise the in-memory watermark/subscriber logic), so
    /// this just needs to be a valid, isolated path each call can write
    /// under without colliding with another test or a real `~/.config/
    /// som/media_cache/`.
    fn test_cache_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("som-srv-cache-test-{}", uuid::Uuid::new_v4()))
    }

    /// Replaces the old `put_chunk_writes_bytes_at_the_given_offset` (which
    /// asserted a cache file on disk contained the concatenated bytes) —
    /// `som-srv` no longer persists anything to disk at all (see this
    /// module's own doc comment); the equivalent guarantee now is that
    /// each chunk's own bytes are forwarded, byte-for-byte, to a
    /// subscriber via the `Progress` push's `chunk_offset`/`chunk_data`
    /// fields.
    #[test]
    fn put_chunk_forwards_each_chunks_bytes_to_subscribers_via_the_progress_push() {
        let cache = SrvCache::new();
        let observed = Arc::new(Mutex::new(Vec::new()));

        {
            let observed = observed.clone();
            cache.subscribe(
                1,
                2,
                Arc::new(move |response| {
                    if let SrvResponse::Progress { chunk_offset, chunk_data, .. } = response {
                        observed.lock().unwrap().push((chunk_offset, chunk_data));
                    }
                    Ok(())
                }),
            );
        }

        let cache_dir = test_cache_dir();
        cache.put_chunk(&cache_dir, 1, 2, 0, b"hello", 10, ContentType::Gif, test_metadata()).unwrap();
        cache.put_chunk(&cache_dir, 1, 2, 5, b"world", 10, ContentType::Gif, test_metadata()).unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(*observed, vec![(0, b"hello".to_vec()), (5, b"world".to_vec())]);
    }

    /// `Video`/`Audio` must never write a chunk to disk anywhere —
    /// confirmed directly against a real config-dir-shaped temp path, not
    /// just by the absence of file-writing code on that branch, since
    /// this is the exact regression (a full on-disk copy per playback,
    /// never cleaned up) that filled a real disk and blocked a 16GB video
    /// from playing at all — see this module's own doc comment for the
    /// full incident. Every OTHER content type (image/GIF/markdown) is
    /// the opposite case, covered by `put_chunk_still_writes_non_video_
    /// audio_content_types_to_disk` below — this pass only took video/
    /// audio out of disk-cache scope, not everything.
    #[test]
    fn put_chunk_never_writes_video_or_audio_to_disk() {
        let cache_dir = test_cache_dir();
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache = SrvCache::new();

        cache.put_chunk(&cache_dir, 1, 2, 0, b"hello", 10, ContentType::Video, test_metadata()).unwrap();
        cache.put_chunk(&cache_dir, 1, 2, 5, b"world", 10, ContentType::Video, test_metadata()).unwrap();

        assert!(!cache_dir.exists(), "put_chunk must never create a cache directory or file on disk for Video/Audio");
    }

    /// Companion to `put_chunk_never_writes_video_or_audio_to_disk` — a
    /// regression test for the opposite direction: `record_progress`
    /// (`crates/terminal`'s `RichContentCache`) opens a file at this
    /// SAME path/naming convention for image/GIF/markdown placements,
    /// expecting `som-srv` to have created it — confirmed live as a real
    /// bug when `put_chunk` was made unconditionally disk-free for every
    /// content type: a plain PNG/GIF placement silently never rendered,
    /// since `record_progress`'s `open()` call failed against a file that
    /// was never written.
    #[test]
    fn put_chunk_still_writes_non_video_audio_content_types_to_disk() {
        let cache_dir = test_cache_dir();
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache = SrvCache::new();

        cache.put_chunk(&cache_dir, 1, 2, 0, b"hello", 10, ContentType::Png, test_metadata()).unwrap();
        cache.put_chunk(&cache_dir, 1, 2, 5, b"world", 10, ContentType::Png, test_metadata()).unwrap();

        let expected_path = cache_dir.join("00000001-00000002.png");
        assert_eq!(
            std::fs::read(&expected_path).expect("put_chunk should have written a file at the expected path"),
            b"helloworld",
            "the file's contents should be the two chunks written at their respective offsets"
        );
    }

    #[test]
    fn put_chunk_tracks_contiguous_len_across_out_of_order_chunks() {
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
        let cache_dir = test_cache_dir();
        cache.put_chunk(&cache_dir, 1, 2, 5, b"world", 10, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), Vec::<u64>::new(), "an out-of-order chunk must not advance contiguous_len");

        cache.put_chunk(&cache_dir, 1, 2, 0, b"hello", 10, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![10], "the watermark must jump straight to 10 once the gap is filled");
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
        let cache_dir = test_cache_dir();
        cache.put_chunk(&cache_dir, 1, 2, 0, b"hello", 15, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), Vec::<u64>::new(), "a leading chunk must not advance tail_available_from");

        // Tail chunk arrives NEXT (with a gap still open in the middle,
        // offset 5..10 unwritten) — offset 10, length 5, chunk_end ==
        // total_size (15) — must advance tail_available_from to 10
        // immediately, a sequential send starting from 0 would still be
        // nowhere near this offset.
        cache.put_chunk(&cache_dir, 1, 2, 10, b"!!!!!", 15, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![10], "a chunk touching the tail must advance tail_available_from immediately");

        // Middle chunk connects the leading and tail chunks already
        // seen — since it's contiguous with BOTH `contiguous_len` (which
        // reaches offset 5) and the tail chunk (which starts at offset
        // 10), it advances `tail_available_from` down to its own start
        // (5) directly — there's no gap left in `pending_ranges` to
        // additionally merge through here, since the earlier tail chunk
        // already got folded in when IT arrived (see the assertion just
        // above). (`contiguous_len` itself also reaches 15 here — not
        // this test's concern, see the companion test.)
        cache.put_chunk(&cache_dir, 1, 2, 5, b"world", 15, ContentType::Gif, test_metadata()).unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![10, 5], "tail_available_from must advance to the connecting chunk's own start");
    }
}
