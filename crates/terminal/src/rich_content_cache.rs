//! Progressive on-disk cache for Som's own rich-content protocol
//! ([`crate::rich_content_transport`]) — receives chunks (possibly
//! out of order, though [`crate::somcat`]'s streaming client always sends
//! them in offset order today) and writes them to a local file, tracking
//! how many leading bytes are contiguously present so a decoder can know
//! it's safe to read up to that point without hitting a hole.
//!
//! Deliberately separate from [`crate::kitty_graphics_store::ImageStore`]
//! — this protocol doesn't reuse Kitty's in-memory `ImageStore` at all
//! (see `rich_content_transport`'s module doc comment for why), and its
//! own state (per-file cache handle + watermark) has a different shape:
//! bytes-on-disk plus a byte offset, not a `HashMap<u32, DecodedImage>`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::rich_content_transport::{Chunk, ContentMetadata, ContentType};

/// One file's progressive-write state: the open handle plus how many
/// bytes starting from offset 0 are known to be present with no gaps.
///
/// `contiguous_len` is only ever advanced by a chunk landing EXACTLY at
/// the current watermark (`chunk.chunk_offset == contiguous_len`) — an
/// out-of-order or gapped chunk is still written to disk at its own
/// offset (so it doesn't need to be re-sent once the gap-filling chunk
/// arrives), but does not advance the watermark until the gap closes.
/// This is what lets a decoder trust "the first `contiguous_len` bytes
/// are complete and correct" without needing to track individual chunk
/// ranges itself.
struct CacheEntry {
    file: File,
    path: PathBuf,
    contiguous_len: u64,
    /// Out-of-order chunks that landed ahead of the current watermark,
    /// kept here (not yet counted into `contiguous_len`) until the gap
    /// before them closes. Keyed by chunk_offset. Expected to stay small
    /// in practice — `somcat --stream` sends strictly in order — but
    /// correctness shouldn't depend on that assumption holding for every
    /// future client.
    pending_ranges: Vec<(u64, u64)>,
    content_type: ContentType,
    /// `0` means the sender never filled it in — same "unknown, don't
    /// guess" convention `Chunk::total_size` itself documents.
    total_size: u64,
    /// The widest placeholder-cell column decoded from any paint pass so
    /// far — see [`RichContentCache::record_max_column_seen`] for why this
    /// needs to persist across paint calls rather than being recomputed
    /// from whatever's on screen right now.
    max_column_seen: std::cell::Cell<u32>,
    /// The image's natural decoded pixel dimensions, from the first
    /// chunk's `ContentMetadata::Image` (`None` for non-image content
    /// types, or if a sender ever left width/height as `0` = unknown).
    /// Needed to re-derive the placeholder grid's column/row count when
    /// the terminal's cell size changes (window resize, font size change)
    /// — see `Terminal::resync_rich_content_placements`.
    image_size_px: Option<(u32, u32)>,
}

/// Per-terminal-session store of in-progress and completed rich-content
/// file transfers. One instance lives on `Terminal`, mirroring
/// `kitty_graphics_store::ImageStore`'s lifetime.
pub struct RichContentCache {
    cache_dir: PathBuf,
    entries: HashMap<(u32, u32), CacheEntry>,
}

impl RichContentCache {
    /// `cache_dir` is the directory chunks get written under — callers
    /// pass `paths::config_dir().join("media_cache")` in production (see
    /// `paths::config_dir`'s existing use in
    /// `crates/som_tmux/src/protocol.rs` for the established pattern of a
    /// per-purpose subdirectory under the same config root), and a
    /// throwaway `tempdir` in tests so test runs never touch a real
    /// user's `~/.config/som/media_cache/`.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir, entries: HashMap::new() }
    }

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

    /// Applies one chunk: opens (or reuses) the file for
    /// `(session_id, file_id)`, writes the payload at `chunk_offset`,
    /// advances the contiguous watermark as far as newly-arrived data
    /// allows, and returns the number of contiguous bytes now available
    /// from the start of the file — the caller (a decoder) reads at most
    /// this many bytes and knows they're gap-free.
    ///
    /// Errors (directory creation failure, seek/write failure) are
    /// returned rather than panicking — a single corrupted/failed chunk
    /// write shouldn't take down the whole terminal session; the caller
    /// decides whether to drop the chunk and continue or surface the
    /// error further.
    pub fn apply_chunk(&mut self, chunk: &Chunk) -> std::io::Result<u64> {
        let key = (chunk.session_id, chunk.file_id);
        if !self.entries.contains_key(&key) {
            std::fs::create_dir_all(&self.cache_dir)?;
            let ext = Self::extension_for(chunk.content_type);
            let path = self.cache_dir.join(format!(
                "{session:08x}-{file:08x}.{ext}",
                session = chunk.session_id,
                file = chunk.file_id
            ));
            let file = OpenOptions::new().create(true).write(true).truncate(true).read(true).open(&path)?;
            self.entries.insert(
                key,
                CacheEntry {
                    file,
                    path,
                    contiguous_len: 0,
                    pending_ranges: Vec::new(),
                    content_type: chunk.content_type,
                    total_size: chunk.total_size,
                    max_column_seen: std::cell::Cell::new(0),
                    image_size_px: match chunk.metadata {
                        ContentMetadata::Image { width_px, height_px, .. } if width_px > 0 && height_px > 0 => {
                            Some((width_px, height_px))
                        },
                        _ => None,
                    },
                },
            );
        }
        let entry = self.entries.get_mut(&key).expect("just inserted above if absent");

        entry.file.seek(SeekFrom::Start(chunk.chunk_offset))?;
        entry.file.write_all(&chunk.payload)?;

        let chunk_end = chunk.chunk_offset + chunk.payload.len() as u64;
        if chunk.chunk_offset <= entry.contiguous_len {
            // Either exactly at the watermark, or overlapping/behind it
            // (a retransmit of already-seen data) — either way this
            // chunk's own bytes extend (or don't move) the watermark.
            entry.contiguous_len = entry.contiguous_len.max(chunk_end);
            // A previously-out-of-order chunk might now be contiguous
            // with the advanced watermark — keep absorbing pending
            // ranges as long as one starts at (or before) the current
            // watermark. Small linear scan is fine: `pending_ranges` is
            // expected to stay tiny (see its own doc comment).
            loop {
                let Some(pos) =
                    entry.pending_ranges.iter().position(|&(offset, _)| offset <= entry.contiguous_len)
                else {
                    break;
                };
                let (offset, end) = entry.pending_ranges.remove(pos);
                entry.contiguous_len = entry.contiguous_len.max(end.max(offset));
            }
        } else {
            entry.pending_ranges.push((chunk.chunk_offset, chunk_end));
        }

        Ok(entry.contiguous_len)
    }

    /// The on-disk path for a given file, if any chunk for it has arrived
    /// yet. A decoder reads from this path directly (up to
    /// [`Self::contiguous_len`] bytes) rather than through this store —
    /// mirrors the "receiver reads bytes off disk itself" model the whole
    /// progressive-copy design is built around, instead of routing
    /// decoded pixel data back through an in-memory store the way
    /// `ImageStore` does for Kitty.
    pub fn path(&self, session_id: u32, file_id: u32) -> Option<&std::path::Path> {
        self.entries.get(&(session_id, file_id)).map(|e| e.path.as_path())
    }

    /// The `ContentType` the first chunk for this id declared — decoding
    /// strategy (progressive GIF vs wait-for-complete-file static image)
    /// depends on it, see `rich_content_player::refresh_or_create`.
    pub fn content_type(&self, session_id: u32, file_id: u32) -> Option<ContentType> {
        self.entries.get(&(session_id, file_id)).map(|e| e.content_type)
    }

    /// The `total_size` the first chunk for this id declared (`0` if the
    /// sender never filled it in).
    pub fn total_size(&self, session_id: u32, file_id: u32) -> u64 {
        self.entries.get(&(session_id, file_id)).map(|e| e.total_size).unwrap_or(0)
    }

    /// Every `(session_id, file_id)` pair with at least one chunk applied
    /// so far — a caller that doesn't already know the exact id (e.g. a
    /// test polling for whatever `somcat --stream` happens to be sending)
    /// uses this to discover it rather than needing the ids threaded
    /// through some other channel.
    pub fn all_known_ids(&self) -> Vec<(u32, u32)> {
        self.entries.keys().copied().collect()
    }

    /// How many leading bytes of the file are contiguously present and
    /// safe to read.
    pub fn contiguous_len(&self, session_id: u32, file_id: u32) -> u64 {
        self.entries.get(&(session_id, file_id)).map(|e| e.contiguous_len).unwrap_or(0)
    }

    /// Records that a placeholder cell at `column` (0-based, within the
    /// image) was decoded as visible during a paint pass, growing the
    /// entry's remembered maximum if this is wider than anything seen
    /// before. Never shrinks — the placement's true column count is
    /// whatever the sending client's grid actually was, and a narrower
    /// SUBSET of columns happening to be on screen right now (e.g. only
    /// the image's left edge is visible after a horizontal-adjacent split
    /// resize) must not un-learn a wider count already observed. `&self`
    /// (not `&mut self`) because paint runs through a `&Terminal` borrow —
    /// see [`CacheEntry::max_column_seen`]'s `Cell` for why interior
    /// mutability is needed here.
    pub fn record_max_column_seen(&self, session_id: u32, file_id: u32, column: u32) {
        if let Some(entry) = self.entries.get(&(session_id, file_id)) {
            let current = entry.max_column_seen.get();
            if column > current {
                entry.max_column_seen.set(column);
            }
        }
    }

    /// The widest column index ([`Self::record_max_column_seen`]) observed
    /// for this placement across every paint pass so far — `None` if
    /// nothing's been recorded yet (id unknown, or no paint has happened).
    pub fn max_column_seen(&self, session_id: u32, file_id: u32) -> Option<u32> {
        self.entries.get(&(session_id, file_id)).map(|e| e.max_column_seen.get())
    }

    /// The image's natural pixel dimensions, if known — see
    /// [`CacheEntry::image_size_px`]'s doc comment.
    pub fn image_size_px(&self, session_id: u32, file_id: u32) -> Option<(u32, u32)> {
        self.entries.get(&(session_id, file_id))?.image_size_px
    }

    /// Overwrites the remembered max-column-seen for a placement — used
    /// after actively re-deriving and rewriting a placement's grid cells
    /// on resize (see `Terminal::resync_rich_content_placements`), where
    /// the OLD `max_column_seen` value (from before the resize) would
    /// otherwise keep the paint path anchored to the stale column count
    /// via [`Self::record_max_column_seen`]'s monotonic-growth-only rule.
    pub fn reset_max_column_seen(&self, session_id: u32, file_id: u32, column: u32) {
        if let Some(entry) = self.entries.get(&(session_id, file_id)) {
            entry.max_column_seen.set(column);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich_content_transport::ContentType;

    fn chunk(session_id: u32, file_id: u32, offset: u64, payload: &[u8]) -> Chunk {
        Chunk {
            content_type: ContentType::Gif,
            session_id,
            file_id,
            chunk_offset: offset,
            total_size: 0,
            metadata: crate::rich_content_transport::ContentMetadata::Image {
                width_px: 0,
                height_px: 0,
                color_bits: 0,
                is_animated: false,
            },
            payload: payload.to_vec(),
        }
    }

    fn temp_cache_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("som_rich_content_cache_test_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sequential_chunks_advance_watermark_and_write_correct_bytes() {
        let dir = temp_cache_dir("sequential");
        let mut cache = RichContentCache::new(dir.clone());

        let len1 = cache.apply_chunk(&chunk(1, 1, 0, b"hello ")).unwrap();
        assert_eq!(len1, 6);
        let len2 = cache.apply_chunk(&chunk(1, 1, 6, b"world")).unwrap();
        assert_eq!(len2, 11);

        let path = cache.path(1, 1).unwrap().to_path_buf();
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, b"hello world");
        assert_eq!(cache.contiguous_len(1, 1), 11);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn out_of_order_chunk_writes_but_does_not_advance_watermark_until_gap_closes() {
        let dir = temp_cache_dir("out_of_order");
        let mut cache = RichContentCache::new(dir.clone());

        // Chunk 2 (offset 6) arrives before chunk 1 (offset 0) — write
        // succeeds, but the watermark must stay at 0 since bytes 0..6
        // are still missing.
        let len_after_second = cache.apply_chunk(&chunk(1, 1, 6, b"world")).unwrap();
        assert_eq!(len_after_second, 0, "watermark must not advance past a gap");

        // Now the gap-filling chunk arrives — watermark should jump all
        // the way to the end, absorbing the already-written pending range.
        let len_after_first = cache.apply_chunk(&chunk(1, 1, 0, b"hello ")).unwrap();
        assert_eq!(len_after_first, 11, "watermark must absorb the pending range once the gap closes");

        let path = cache.path(1, 1).unwrap().to_path_buf();
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, b"hello world", "bytes on disk must be correct regardless of arrival order");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn different_session_or_file_ids_use_separate_cache_entries() {
        let dir = temp_cache_dir("separate_entries");
        let mut cache = RichContentCache::new(dir.clone());

        cache.apply_chunk(&chunk(1, 1, 0, b"first")).unwrap();
        cache.apply_chunk(&chunk(2, 1, 0, b"second-session")).unwrap();
        cache.apply_chunk(&chunk(1, 2, 0, b"second-file")).unwrap();

        assert_eq!(cache.contiguous_len(1, 1), 5);
        assert_eq!(cache.contiguous_len(2, 1), 14);
        assert_eq!(cache.contiguous_len(1, 2), 11);
        assert_ne!(cache.path(1, 1), cache.path(2, 1));
        assert_ne!(cache.path(1, 1), cache.path(1, 2));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_session_file_pair_reports_zero_and_no_path() {
        let dir = temp_cache_dir("unknown");
        let cache = RichContentCache::new(dir.clone());
        assert_eq!(cache.contiguous_len(99, 99), 0);
        assert!(cache.path(99, 99).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn retransmitted_chunk_at_or_behind_watermark_does_not_move_watermark_backward() {
        let dir = temp_cache_dir("retransmit");
        let mut cache = RichContentCache::new(dir.clone());

        cache.apply_chunk(&chunk(1, 1, 0, b"hello world")).unwrap();
        assert_eq!(cache.contiguous_len(1, 1), 11);

        // Re-apply the first half again (offset 0, shorter payload) —
        // watermark must not regress even though this chunk's own
        // `chunk_end` (6) is less than the current watermark (11).
        let len = cache.apply_chunk(&chunk(1, 1, 0, b"hello ")).unwrap();
        assert_eq!(len, 11, "watermark must never move backward on a retransmit");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extension_matches_content_type() {
        let dir = temp_cache_dir("extension");
        let mut cache = RichContentCache::new(dir.clone());
        cache.apply_chunk(&chunk(1, 1, 0, b"x")).unwrap();
        let path = cache.path(1, 1).unwrap();
        assert_eq!(path.extension().unwrap(), "gif");
        std::fs::remove_dir_all(&dir).ok();
    }
}
