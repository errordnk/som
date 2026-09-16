//! Per-`Terminal` metadata registry for Som's own rich-content protocol
//! ([`crate::rich_content_transport`]) — tracks the bookkeeping every
//! `rich_content_*_placements` paint-path method needs to know about a
//! placement (content type, watermark, natural pixel/audio dimensions,
//! reserved grid footprint) WITHOUT owning any bytes itself. The actual
//! bytes for every content type now live in [`crate::rich_content_srv_
//! channel::SrvProgressState`] — this registry was originally also the
//! on-disk cache for image/GIF/markdown's chunk bytes (`apply_chunk`
//! opened and wrote a real file per placement), but that disk-writing
//! half was removed once those three content types moved onto the same
//! in-memory buffer video/audio already used, closing the gap that
//! motivated `somsrv`'s own `media_cache` size-limit watcher in the
//! first place (a disk cache for bytes that were also being kept
//! in-memory made the watcher's whole reason to exist redundant).
//!
//! Deliberately separate from [`crate::kitty_graphics_store::ImageStore`]
//! — this protocol doesn't reuse Kitty's in-memory `ImageStore` at all
//! (see `rich_content_transport`'s module doc comment for why).

use std::collections::HashMap;

use crate::rich_content_transport::{ContentMetadata, ContentType};

/// One placement's metadata: content type, watermark, and whatever
/// per-content-type extras the paint path needs (natural image/audio
/// dimensions, reserved grid footprint) — no bytes, no file handle.
struct CacheEntry {
    contiguous_len: u64,
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

/// Per-terminal-session registry of in-progress and completed rich-content
/// placements' metadata. One instance lives on `Terminal`, mirroring
/// `kitty_graphics_store::ImageStore`'s lifetime.
pub struct RichContentCache {
    entries: HashMap<(u32, u32), CacheEntry>,
}

impl RichContentCache {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    /// Records a `somsrv::protocol::SrvResponse::Progress` push:
    /// updates `contiguous_len`/`total_size`/metadata for `(session_id,
    /// file_id)` — pure bookkeeping, no bytes ever touch this registry.
    /// The FIRST push for a given key seeds `content_type`/the per-
    /// content-type metadata extras; every later push for the same key
    /// only advances `contiguous_len`/`total_size` (a real sender's
    /// metadata never actually changes mid-transfer, see `somsrv::
    /// protocol::SrvRequest::PutChunk`'s own doc comment).
    pub fn record_progress(
        &mut self,
        content_type: ContentType,
        session_id: u32,
        file_id: u32,
        contiguous_len: u64,
        total_size: u64,
        metadata: ContentMetadata,
    ) {
        let key = (session_id, file_id);
        self.entries.entry(key).or_insert_with(|| CacheEntry {
            contiguous_len: 0,
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
        });
        let entry = self.entries.get_mut(&key).expect("just inserted above if absent");
        // `contiguous_len` only ever moves forward — `somsrv` is the
        // single source of truth for this value, so a later push always
        // reflects at least as much progress as an earlier one; `max`
        // here is just defense against a hypothetical out-of-order
        // delivery of `Progress` pushes themselves, not an expected case.
        entry.contiguous_len = entry.contiguous_len.max(contiguous_len);
        entry.total_size = total_size;
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
    /// `somsrv`'s own `SrvCache` is the source of truth for the bytes
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

    fn test_metadata() -> ContentMetadata {
        ContentMetadata::Image { width_px: 0, height_px: 0, color_bits: 0, is_animated: false }
    }

    #[test]
    fn record_progress_seeds_content_type_and_advances_watermark() {
        let mut cache = RichContentCache::new();

        cache.record_progress(ContentType::Gif, 1, 1, 6, 100, test_metadata());
        assert_eq!(cache.content_type(1, 1), Some(ContentType::Gif));
        assert_eq!(cache.contiguous_len(1, 1), 6);
        assert_eq!(cache.total_size(1, 1), 100);

        cache.record_progress(ContentType::Gif, 1, 1, 50, 100, test_metadata());
        assert_eq!(cache.contiguous_len(1, 1), 50);
    }

    #[test]
    fn record_progress_never_moves_the_watermark_backward() {
        let mut cache = RichContentCache::new();

        cache.record_progress(ContentType::Gif, 1, 1, 50, 100, test_metadata());
        assert_eq!(cache.contiguous_len(1, 1), 50);

        // A later push reporting a SMALLER contiguous_len than already
        // observed — defense against a hypothetical out-of-order
        // `Progress` push, not an expected case (see `record_progress`'s
        // own doc comment).
        cache.record_progress(ContentType::Gif, 1, 1, 10, 100, test_metadata());
        assert_eq!(cache.contiguous_len(1, 1), 50, "watermark must never move backward");
    }

    #[test]
    fn different_session_or_file_ids_use_separate_cache_entries() {
        let mut cache = RichContentCache::new();

        cache.record_progress(ContentType::Gif, 1, 1, 5, 5, test_metadata());
        cache.record_progress(ContentType::Gif, 2, 1, 14, 14, test_metadata());
        cache.record_progress(ContentType::Gif, 1, 2, 11, 11, test_metadata());

        assert_eq!(cache.contiguous_len(1, 1), 5);
        assert_eq!(cache.contiguous_len(2, 1), 14);
        assert_eq!(cache.contiguous_len(1, 2), 11);
    }

    #[test]
    fn unknown_session_file_pair_reports_zero_and_no_content_type() {
        let cache = RichContentCache::new();
        assert_eq!(cache.contiguous_len(99, 99), 0);
        assert!(cache.content_type(99, 99).is_none());
    }

    #[test]
    fn remove_forgets_the_entry_entirely() {
        let mut cache = RichContentCache::new();
        cache.record_progress(ContentType::Png, 1, 1, 10, 10, test_metadata());
        assert!(cache.content_type(1, 1).is_some());

        cache.remove(1, 1);

        assert!(cache.content_type(1, 1).is_none());
        assert_eq!(cache.contiguous_len(1, 1), 0);
        assert!(cache.all_known_ids().is_empty());
    }

    #[test]
    fn image_size_px_is_seeded_from_the_first_progress_push_only() {
        let mut cache = RichContentCache::new();
        let metadata = ContentMetadata::Image { width_px: 90, height_px: 60, color_bits: 32, is_animated: false };
        cache.record_progress(ContentType::Png, 1, 1, 10, 10, metadata);
        assert_eq!(cache.image_size_px(1, 1), Some((90, 60)));

        // A later push with different metadata must NOT overwrite the
        // dimensions already seeded — a real sender's own metadata never
        // actually changes mid-transfer (see `record_progress`'s own doc
        // comment), so this only matters for a malformed/adversarial
        // later push, which must not corrupt an already-correct value.
        let different_metadata = ContentMetadata::Image { width_px: 1, height_px: 1, color_bits: 32, is_animated: false };
        cache.record_progress(ContentType::Png, 1, 1, 10, 10, different_metadata);
        assert_eq!(cache.image_size_px(1, 1), Some((90, 60)));
    }
}
