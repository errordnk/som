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

use crate::rich_content_transport::{ContentMetadata, ContentType};

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
    /// `None` for `Video`/`Audio` entries created via [`RichContentCache::
    /// record_progress`] — those content types no longer have an on-disk
    /// file at all (`GrowingFileStream`/the audio decoder's equivalent
    /// read straight out of `SrvProgressState`'s in-memory buffer
    /// instead, see that type's own doc comment for why). `Some` for
    /// every entry [`RichContentCache::apply_chunk`] creates (the OLD
    /// PTY-parsing write path, still disk-backed) and for `record_
    /// progress` entries of every OTHER content type (image/GIF/
    /// markdown), which stay disk-backed in this pass — see the plan
    /// this change implements for why those are explicitly out of scope.
    file: Option<File>,
    path: Option<PathBuf>,
    contiguous_len: u64,
    /// Out-of-order chunks that landed ahead of the current watermark,
    /// kept here (not yet counted into `contiguous_len`) until the gap
    /// before them closes. Keyed by chunk_offset. Expected to stay small
    /// in practice — `somcat --stream` sends strictly in order — but
    /// correctness shouldn't depend on that assumption holding for every
    /// future client.
    pending_ranges: Vec<(u64, u64)>,
    content_type: ContentType,
    /// `0` means the sender never filled it in — "unknown, don't guess",
    /// not "empty file".
    total_size: u64,
    /// The widest placeholder-cell column decoded from any paint pass so
    /// far — see [`RichContentCache::record_max_column_seen`] for why this
    /// needs to persist across paint calls rather than being recomputed
    /// from whatever's on screen right now.
    max_column_seen: std::cell::Cell<u32>,
    /// The tallest placeholder-cell row decoded from any paint pass so
    /// far — same purpose/persistence rationale as `max_column_seen`,
    /// just the vertical counterpart. See
    /// [`RichContentCache::record_max_row_seen`].
    max_row_seen: std::cell::Cell<u32>,
    /// The image's natural decoded pixel dimensions, from the first
    /// chunk's `ContentMetadata::Image` (`None` for non-image content
    /// types, or if a sender ever left width/height as `0` = unknown).
    /// Needed to re-derive the placeholder grid's column/row count when
    /// the terminal's cell size changes (window resize, font size change)
    /// — see `Terminal::resync_rich_content_placements`.
    image_size_px: Option<(u32, u32)>,
    /// `(sample_rate, channels, bits_per_sample, duration_ms)` from the
    /// first chunk's `ContentMetadata::Audio` (`None` for non-audio
    /// content types). `duration_ms` is what lets a paint pass know the
    /// audio widget's real total length — and thus render an accurate
    /// seek bar / compute a correct seek target frame — from the very
    /// first chunk, well before the file (or even a decodable prefix of
    /// it) has actually arrived. See `rich_content_transport::
    /// ContentMetadata::Audio`'s own doc comment for where this number
    /// comes from on the sending side.
    audio_metadata: Option<(u32, u8, u8, u32)>,
    /// From the first chunk's `ContentMetadata::Video::audio_stream_index`
    /// (`None` for non-video content types, OR a video whose sender
    /// never set the field — see that field's own doc comment for what
    /// `None` means to the decoder: "use FFmpeg's own heuristic").
    video_audio_stream_index: Option<u32>,
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
    /// `crates/som_srv/src/protocol.rs` for the established pattern of a
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
    /// `(session_id, file_id)`, writes `payload` at `chunk_offset`,
    /// advances the contiguous watermark as far as newly-arrived data
    /// allows, and returns the number of contiguous bytes now available
    /// from the start of the file — the caller (a decoder) reads at most
    /// this many bytes and knows they're gap-free.
    ///
    /// Takes raw fields rather than a single struct — this used to take
    /// `&rich_content_transport::Chunk`, but that type (along with the
    /// whole APC/base91 envelope format it was parsed from) was deleted
    /// once chunks started arriving via `som-srv`'s binary side channel
    /// instead of the PTY (see `som_srv::protocol::SrvRequest::PutChunk`,
    /// which carries the same fields this function now takes directly).
    /// `content_type`/`metadata` are only consulted the FIRST time a given
    /// `(session_id, file_id)` is seen (to open the cache file and seed
    /// per-placement metadata) — a caller applying a later chunk for an
    /// already-known id can pass whatever it has on hand for those two
    /// (e.g. the same values as the first chunk), since they're ignored
    /// once the entry already exists.
    ///
    /// Errors (directory creation failure, seek/write failure) are
    /// returned rather than panicking — a single corrupted/failed chunk
    /// write shouldn't take down the whole terminal session; the caller
    /// decides whether to drop the chunk and continue or surface the
    /// error further.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_chunk(
        &mut self,
        content_type: ContentType,
        session_id: u32,
        file_id: u32,
        chunk_offset: u64,
        total_size: u64,
        metadata: ContentMetadata,
        payload: &[u8],
    ) -> std::io::Result<u64> {
        let key = (session_id, file_id);
        if !self.entries.contains_key(&key) {
            std::fs::create_dir_all(&self.cache_dir)?;
            let ext = Self::extension_for(content_type);
            let path =
                self.cache_dir.join(format!("{session_id:08x}-{file_id:08x}.{ext}"));
            let file = OpenOptions::new().create(true).write(true).truncate(true).read(true).open(&path)?;
            self.entries.insert(
                key,
                CacheEntry {
                    file: Some(file),
                    path: Some(path),
                    contiguous_len: 0,
                    pending_ranges: Vec::new(),
                    content_type,
                    total_size,
                    max_column_seen: std::cell::Cell::new(0),
                    max_row_seen: std::cell::Cell::new(0),
                    image_size_px: match metadata {
                        ContentMetadata::Image { width_px, height_px, .. } if width_px > 0 && height_px > 0 => {
                            Some((width_px, height_px))
                        },
                        _ => None,
                    },
                    audio_metadata: match metadata {
                        ContentMetadata::Audio { sample_rate, channels, bits_per_sample, duration_ms, .. } => {
                            Some((sample_rate, channels, bits_per_sample, duration_ms))
                        },
                        _ => None,
                    },
                    video_audio_stream_index: match metadata {
                        ContentMetadata::Video { audio_stream_index, .. } => audio_stream_index,
                        _ => None,
                    },
                },
            );
        }
        let entry = self.entries.get_mut(&key).expect("just inserted above if absent");

        // `apply_chunk` always creates its entry with `Some(file)` just
        // above — the only way `file` is ever `None` is via `record_
        // progress`'s Video/Audio branch (see `CacheEntry::file`'s own
        // doc comment), a path this function never takes.
        let file = entry.file.as_mut().expect("apply_chunk's own entries always have a file");
        file.seek(SeekFrom::Start(chunk_offset))?;
        file.write_all(payload)?;

        let chunk_end = chunk_offset + payload.len() as u64;
        if chunk_offset <= entry.contiguous_len {
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
            entry.pending_ranges.push((chunk_offset, chunk_end));
        }

        Ok(entry.contiguous_len)
    }

    /// Records a `som_srv::protocol::SrvResponse::Progress` push:
    /// updates `contiguous_len`/`total_size`/metadata for `(session_id,
    /// file_id)` WITHOUT writing any payload bytes to disk itself.
    ///
    /// For `Video`/`Audio`, there is no cache FILE to open at all
    /// anymore — `som-srv` no longer persists chunks to disk for any
    /// content type (see `som_srv::srv_cache::SrvCache`'s own doc
    /// comment), and those two content types' decoders
    /// (`GrowingFileStream`/the audio decoder's equivalent) read
    /// directly out of `SrvProgressState`'s in-memory buffer instead of
    /// ever consulting this cache's `path()` — so this method only
    /// tracks bookkeeping (watermarks, `video_audio_stream_index`, etc.)
    /// for those two, never touching the filesystem.
    ///
    /// For every OTHER content type (image/GIF/markdown — still
    /// disk-backed in this pass, see the plan this implements for why),
    /// `som-srv` no longer writes any content type to disk either —
    /// `RichContentCache::apply_chunk` (the OLD PTY-parsing write path)
    /// is the only thing that still creates a file on disk for image/
    /// GIF/markdown today. A `record_progress` entry for one of those
    /// content types opens that SAME file if `apply_chunk` already
    /// created it, or fails (same tolerance as before — see this
    /// method's own call site in `Terminal::ensure_rich_content_srv_
    /// subscription`) if nothing has written it yet.
    pub fn record_progress(
        &mut self,
        content_type: ContentType,
        session_id: u32,
        file_id: u32,
        contiguous_len: u64,
        total_size: u64,
        metadata: ContentMetadata,
    ) -> std::io::Result<()> {
        let key = (session_id, file_id);
        if !self.entries.contains_key(&key) {
            let (file, path) = if matches!(content_type, ContentType::Video | ContentType::Audio) {
                (None, None)
            } else {
                let ext = Self::extension_for(content_type);
                let path = self.cache_dir.join(format!("{session_id:08x}-{file_id:08x}.{ext}"));
                let file = OpenOptions::new().read(true).open(&path)?;
                (Some(file), Some(path))
            };
            self.entries.insert(
                key,
                CacheEntry {
                    file,
                    path,
                    contiguous_len: 0,
                    pending_ranges: Vec::new(),
                    content_type,
                    total_size,
                    max_column_seen: std::cell::Cell::new(0),
                    max_row_seen: std::cell::Cell::new(0),
                    image_size_px: match metadata {
                        ContentMetadata::Image { width_px, height_px, .. } if width_px > 0 && height_px > 0 => {
                            Some((width_px, height_px))
                        },
                        _ => None,
                    },
                    audio_metadata: match metadata {
                        ContentMetadata::Audio { sample_rate, channels, bits_per_sample, duration_ms, .. } => {
                            Some((sample_rate, channels, bits_per_sample, duration_ms))
                        },
                        _ => None,
                    },
                    video_audio_stream_index: match metadata {
                        ContentMetadata::Video { audio_stream_index, .. } => audio_stream_index,
                        _ => None,
                    },
                },
            );
        }
        let entry = self.entries.get_mut(&key).expect("just inserted above if absent");
        // `contiguous_len` only ever moves forward — `som-srv` is the
        // single source of truth for this value (it tracks the exact
        // same gap-tolerant watermark `apply_chunk` used to compute
        // locally), so a later push always reflects at least as much
        // progress as an earlier one; `max` here is just defense against
        // a hypothetical out-of-order delivery of `Progress` pushes
        // themselves, not an expected case.
        entry.contiguous_len = entry.contiguous_len.max(contiguous_len);
        entry.total_size = total_size;
        Ok(())
    }

    /// The on-disk path for a given file, if any chunk for it has arrived
    /// yet AND this content type is still disk-backed (image/GIF/
    /// markdown — `None` for `Video`/`Audio` entries even once they
    /// exist, since those two no longer have a file at all, see
    /// [`CacheEntry::file`]'s own doc comment). A decoder for a
    /// disk-backed content type reads from this path directly (up to
    /// [`Self::contiguous_len`] bytes) rather than through this store —
    /// mirrors the "receiver reads bytes off disk itself" model the whole
    /// progressive-copy design is built around, instead of routing
    /// decoded pixel data back through an in-memory store the way
    /// `ImageStore` does for Kitty.
    pub fn path(&self, session_id: u32, file_id: u32) -> Option<&std::path::Path> {
        self.entries.get(&(session_id, file_id))?.path.as_deref()
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

    /// Forgets `(session_id, file_id)` entirely — after this call,
    /// [`Self::all_known_ids`] no longer includes it, and every accessor
    /// above returns `None`/`0`/empty for it as if no chunk had ever
    /// arrived. Needed by a real `clear`/screen-erase escape sequence
    /// (`Terminal::process_event`'s `AlacTermEvent::ClearScreen` handling):
    /// without this, EVERY placements-lookup method (`rich_content_
    /// placements`, `rich_content_audio_placements`, `rich_content_video_
    /// placements`) keeps discovering this id via `all_known_ids` on every
    /// subsequent paint pass regardless of whether its placeholder cells
    /// are still visible anywhere — `clear` erasing those cells does NOT
    /// erase this cache entry, so a player dropped by `clear` (audio's
    /// `stop_all_rich_content_audio_playback`, video's own teardown in
    /// `ClearScreen` handling) simply reopens itself (and, for video,
    /// autoplays) on the very next paint, fully inaudible-widget but
    /// fully audible — confirmed live as exactly this symptom (video
    /// stopped, then `clear`, and the video's own audio track started
    /// playing again with no picture visible anywhere). The underlying
    /// on-disk cache file is deliberately left alone (not deleted) —
    /// `som-srv`'s own `SrvCache` is the source of truth for the bytes
    /// themselves; this only forgets Som's in-memory bookkeeping about
    /// that same id, mirroring how `clear` already only erases the
    /// terminal's own grid, not any process actually still running.
    pub fn remove(&mut self, session_id: u32, file_id: u32) {
        self.entries.remove(&(session_id, file_id));
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

    /// Vertical counterpart to [`Self::record_max_column_seen`] — records
    /// that a placeholder cell at `row` (0-based, within the image) was
    /// decoded as visible during a paint pass, growing the entry's
    /// remembered maximum if this is taller than anything seen before.
    /// Needed because the sending client's grid height (`rows =
    /// height_px.div_ceil(cell_height)`) rounds UP to a whole number of
    /// cells, so the image's real pixel height essentially never exactly
    /// fills that many rows — deriving the painted height purely from the
    /// image's own aspect ratio (as the paint path used to) leaves a
    /// visible gap of blank terminal background below the image, in the
    /// LAST row the grid reserved but the image doesn't fully reach.
    /// Painting to `max_row_seen + 1` rows tall instead makes the image
    /// fill the exact footprint its own placeholder grid reserved, same
    /// as it already does horizontally via `max_column_seen`.
    pub fn record_max_row_seen(&self, session_id: u32, file_id: u32, row: u32) {
        if let Some(entry) = self.entries.get(&(session_id, file_id)) {
            let current = entry.max_row_seen.get();
            if row > current {
                entry.max_row_seen.set(row);
            }
        }
    }

    /// The tallest row index ([`Self::record_max_row_seen`]) observed for
    /// this placement across every paint pass so far — `None` if nothing's
    /// been recorded yet (id unknown, or no paint has happened).
    pub fn max_row_seen(&self, session_id: u32, file_id: u32) -> Option<u32> {
        self.entries.get(&(session_id, file_id)).map(|e| e.max_row_seen.get())
    }

    /// The image's natural pixel dimensions, if known — see
    /// [`CacheEntry::image_size_px`]'s doc comment.
    pub fn image_size_px(&self, session_id: u32, file_id: u32) -> Option<(u32, u32)> {
        self.entries.get(&(session_id, file_id))?.image_size_px
    }

    /// `(sample_rate, channels, bits_per_sample, duration_ms)`, if known
    /// — see [`CacheEntry::audio_metadata`]'s doc comment.
    pub fn audio_metadata(&self, session_id: u32, file_id: u32) -> Option<(u32, u8, u8, u32)> {
        self.entries.get(&(session_id, file_id))?.audio_metadata
    }

    /// The video's `-a <N>` audio-stream-index override, if the sender
    /// set one — see [`CacheEntry::video_audio_stream_index`]'s own doc
    /// comment. `None` at the OUTER `Option` level means no entry exists
    /// yet for this id at all; `None` at the inner level means an entry
    /// exists but no override was requested (use FFmpeg's own
    /// heuristic) — both collapse to the same `None` here since callers
    /// (`RichContentVideoPlayer::open`) treat them identically.
    pub fn video_audio_stream_index(&self, session_id: u32, file_id: u32) -> Option<u32> {
        self.entries.get(&(session_id, file_id))?.video_audio_stream_index
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

    /// Thin wrapper around [`RichContentCache::apply_chunk`] fixing every
    /// argument except the four that vary across this module's test cases
    /// — mirrors the old `Chunk`-struct-building test helper this replaced
    /// once `apply_chunk` started taking raw fields instead of a
    /// `rich_content_transport::Chunk` (see that method's own doc comment
    /// for why).
    fn apply_test_chunk(cache: &mut RichContentCache, session_id: u32, file_id: u32, offset: u64, payload: &[u8]) -> std::io::Result<u64> {
        cache.apply_chunk(
            ContentType::Gif,
            session_id,
            file_id,
            offset,
            0,
            crate::rich_content_transport::ContentMetadata::Image {
                width_px: 0,
                height_px: 0,
                color_bits: 0,
                is_animated: false,
            },
            payload,
        )
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

        let len1 = apply_test_chunk(&mut cache, 1, 1, 0, b"hello ").unwrap();
        assert_eq!(len1, 6);
        let len2 = apply_test_chunk(&mut cache, 1, 1, 6, b"world").unwrap();
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
        let len_after_second = apply_test_chunk(&mut cache, 1, 1, 6, b"world").unwrap();
        assert_eq!(len_after_second, 0, "watermark must not advance past a gap");

        // Now the gap-filling chunk arrives — watermark should jump all
        // the way to the end, absorbing the already-written pending range.
        let len_after_first = apply_test_chunk(&mut cache, 1, 1, 0, b"hello ").unwrap();
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

        apply_test_chunk(&mut cache, 1, 1, 0, b"first").unwrap();
        apply_test_chunk(&mut cache, 2, 1, 0, b"second-session").unwrap();
        apply_test_chunk(&mut cache, 1, 2, 0, b"second-file").unwrap();

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

        apply_test_chunk(&mut cache, 1, 1, 0, b"hello world").unwrap();
        assert_eq!(cache.contiguous_len(1, 1), 11);

        // Re-apply the first half again (offset 0, shorter payload) —
        // watermark must not regress even though this chunk's own
        // `chunk_end` (6) is less than the current watermark (11).
        let len = apply_test_chunk(&mut cache, 1, 1, 0, b"hello ").unwrap();
        assert_eq!(len, 11, "watermark must never move backward on a retransmit");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extension_matches_content_type() {
        let dir = temp_cache_dir("extension");
        let mut cache = RichContentCache::new(dir.clone());
        apply_test_chunk(&mut cache, 1, 1, 0, b"x").unwrap();
        let path = cache.path(1, 1).unwrap();
        assert_eq!(path.extension().unwrap(), "gif");
        std::fs::remove_dir_all(&dir).ok();
    }
}
