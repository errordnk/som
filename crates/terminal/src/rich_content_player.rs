//! GPU-upload-ready, animatable decode state for Som's own rich-content
//! protocol ([`crate::rich_content_transport`]) — the paint-time
//! counterpart to [`crate::rich_content_cache::RichContentCache`] (which
//! only owns bytes-on-disk) and [`crate::rich_content_gif_player`] (which
//! only owns "decode this many bytes into frames").
//!
//! Mirrors [`crate::kitty_graphics_store::DecodedImage`]'s shape closely
//! (same `Arc<RenderImage>` + `Cell`-based animation playback state, same
//! reason for both: cheap repeated paint calls, and playback advancement
//! without needing `&mut self` from inside a paint pass — see that type's
//! doc comment) but is driven by re-decoding a growing cache file instead
//! of accumulating Kitty `a=f` frame commands.

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::RenderImage;
use smallvec::SmallVec;

use crate::rich_content_gif_player;
use crate::rich_content_static_image_player;
use crate::rich_content_transport::ContentType;

/// One rich-content file's paint-ready state: the most recently decoded
/// frames (cached — re-decoding on every paint call would be wasteful,
/// see [`RichContentPlayer::refresh`]) plus animation playback position.
pub struct RichContentPlayer {
    render_image: Arc<RenderImage>,
    /// How many contiguous bytes [`Self::render_image`] was decoded from —
    /// [`Self::refresh`] only re-decodes when the cache's current
    /// `contiguous_len` has grown past this, so a paint call between
    /// chunk arrivals is a cheap no-op instead of re-running the GIF
    /// decoder every frame.
    decoded_through: u64,
    /// Whether the last decode reached the file's real end (GIF trailer
    /// byte), not just "ran out of available bytes so far" — mirrors
    /// `rich_content_gif_player::DecodedPrefix::complete`.
    complete: bool,
    current_frame: Cell<usize>,
    last_advance: Cell<Option<Instant>>,
}

impl RichContentPlayer {
    /// Builds a player from an already-decoded prefix — used both for the
    /// initial creation (see [`refresh_or_create`]) and to reuse the same
    /// struct shape after a later re-decode.
    fn from_prefix(mut prefix: rich_content_gif_player::DecodedPrefix, decoded_through: u64) -> Self {
        // `image::Frame`/the `gif` crate's decoder only ever produce RGBA
        // pixel data, but `gpui::RenderImage` requires BGRA (see that
        // type's own doc comment) — without this swap, red and blue are
        // visibly transposed on screen (confirmed live: skin/hair/seat
        // colors all came out shifted toward blue). Mirrors
        // `kitty_graphics_store::swap_to_bgra`'s identical per-pixel
        // `swap(0, 2)`, which Kitty's own PNG-decoding path already needs
        // for the exact same reason — not duplicated as a shared helper
        // only because the two modules deliberately don't depend on each
        // other (see `rich_content_transport`'s module doc comment).
        for frame in &mut prefix.frames {
            for pixel in frame.buffer_mut().as_flat_samples_mut().samples.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        let render_image = Arc::new(RenderImage::new(SmallVec::from_vec(prefix.frames)));
        Self {
            render_image,
            decoded_through,
            complete: prefix.complete,
            current_frame: Cell::new(0),
            last_advance: Cell::new(None),
        }
    }

    /// The frame index to paint right now, advancing playback as a side
    /// effect if enough time has passed since the last switch — same
    /// time-check shape as `kitty_graphics_store::DecodedImage::current_frame`.
    /// Unlike Kitty (which has an explicit `s=1`/`s=2`/`s=3` play/pause/loop
    /// state via `a=a`), SRP has no such command yet — a rich-content
    /// player is always "playing" once it has more than one frame, looping
    /// back to the start once the LAST successfully decoded frame is
    /// reached (whether or not the file has finished transferring: an
    /// incomplete-but-multi-frame prefix still loops what it has rather
    /// than freezing on the last available frame, since there's no signal
    /// here to distinguish "paused" from "still downloading").
    pub fn current_frame(&self) -> usize {
        let frame_count = self.render_image.frame_count();
        if frame_count <= 1 {
            return self.current_frame.get();
        }

        let now = Instant::now();
        match self.last_advance.get() {
            None => self.last_advance.set(Some(now)),
            Some(last) => {
                let elapsed = now - last;
                let frame_duration = Duration::from(self.render_image.delay(self.current_frame.get()));
                if elapsed >= frame_duration {
                    let next_frame = (self.current_frame.get() + 1) % frame_count;
                    self.current_frame.set(next_frame);
                    self.last_advance.set(Some(now - (elapsed - frame_duration)));
                }
            },
        }
        self.current_frame.get()
    }

    /// True while there's more than one frame — the signal a paint pass
    /// uses to decide whether to request another animation frame /
    /// throttled force-redraw, same role as
    /// `kitty_graphics_store::DecodedImage::is_animating`.
    pub fn is_animating(&self) -> bool {
        self.render_image.frame_count() > 1
    }

    pub fn render_image(&self) -> &Arc<RenderImage> {
        &self.render_image
    }
}

/// Re-decodes `path` if `contiguous_len` has grown since `existing`'s last
/// decode (or `existing` is `None`), returning the (possibly unchanged)
/// player to keep. Returns `None` if there still isn't enough data for
/// even a first frame, or the file isn't decodable content this module
/// understands.
///
/// Kept as a free function (not a `RichContentPlayer` method) because
/// creating and refreshing are the same decision from a caller's
/// perspective ("give me an up-to-date player for this path, or nothing
/// yet") — a caller (`Terminal`, see its `rich_content_players` field)
/// doesn't need to know which case applies.
pub fn refresh_or_create(
    existing: Option<RichContentPlayer>,
    path: &std::path::Path,
    contiguous_len: u64,
    content_type: ContentType,
    total_size: u64,
) -> Option<RichContentPlayer> {
    if let Some(player) = &existing {
        // Already fully decoded and nothing new has arrived — reuse as-is
        // rather than re-opening/re-parsing the file for no reason. A
        // `complete` player with MORE bytes now available (a retransmit,
        // or a second file reusing bytes past the original trailer) is
        // deliberately still treated as "nothing to do" — once a GIF
        // decode reaches its own trailer byte, there is nothing further
        // for this protocol to show from that file.
        if player.complete || contiguous_len <= player.decoded_through {
            return existing;
        }
    }

    let decoded = match content_type {
        ContentType::Gif => rich_content_gif_player::try_decode_progressive(path, contiguous_len),
        // JPEG/PNG have no progressive-prefix decode story in the `image`
        // crate (see `rich_content_static_image_player`'s module doc
        // comment) — wait for the whole file rather than attempting (and
        // failing) a decode on every chunk arrival.
        ContentType::Jpeg | ContentType::Png => {
            if total_size == 0 || contiguous_len < total_size {
                Ok(None)
            } else {
                rich_content_static_image_player::decode_complete(path).map(Some)
            }
        },
        // Not an image format at all — nothing for this player to decode.
        ContentType::Audio | ContentType::Markdown | ContentType::Video => Ok(None),
    };

    match decoded {
        Ok(Some(prefix)) => Some(RichContentPlayer::from_prefix(prefix, contiguous_len)),
        // Not enough data yet, or a real decode error — either way, no
        // usable player. A genuine format error (not just "too early")
        // is silently dropped rather than surfaced: same tolerance
        // principle `rich_content_transport::parse_envelope` failures
        // already use in `Terminal::process_event` — a single
        // unrecognized/corrupt stream shouldn't need special paint-path
        // error handling, it just never produces a player to paint.
        Ok(None) | Err(_) => existing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn giphy_gif_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../giphy.gif")
    }

    /// Builds a tiny (2x1px) single-frame GIF with a known solid red
    /// pixel and a known solid blue pixel, written to a fresh temp file —
    /// enough to prove [`RichContentPlayer::from_prefix`]'s RGBA->BGRA
    /// swap actually happens, without depending on any real GIF fixture's
    /// specific pixel colors.
    fn write_two_pixel_gif(name: &str) -> std::path::PathBuf {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame, Rgba, RgbaImage};

        let mut buffer = RgbaImage::new(2, 1);
        buffer.put_pixel(0, 0, Rgba([255, 0, 0, 255])); // solid red
        buffer.put_pixel(1, 0, Rgba([0, 0, 255, 255])); // solid blue
        let frame = Frame::from_parts(buffer, 0, 0, Delay::from_numer_denom_ms(100, 1));

        let path = std::env::temp_dir()
            .join(format!("som_rich_content_player_test_{name}_{}.gif", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = GifEncoder::new(file);
        encoder.encode_frame(frame).unwrap();
        drop(encoder);
        path
    }

    #[test]
    fn refresh_or_create_returns_none_before_any_frame_is_decodable() {
        let path = giphy_gif_path();
        let full_len = std::fs::metadata(&path).unwrap().len();
        let player = refresh_or_create(None, &path, 4, ContentType::Gif, full_len);
        assert!(player.is_none(), "a handful of header bytes must not produce a player yet");
    }

    #[test]
    fn refresh_or_create_produces_a_player_once_a_frame_is_available() {
        let path = giphy_gif_path();
        let full_len = std::fs::metadata(&path).unwrap().len();
        let player = refresh_or_create(None, &path, full_len / 2, ContentType::Gif, full_len);
        let player = player.expect("half the file should decode at least one frame");
        assert!(player.render_image().frame_count() > 0);
    }

    #[test]
    fn refresh_reuses_existing_player_when_contiguous_len_has_not_grown() {
        let path = giphy_gif_path();
        let full_len = std::fs::metadata(&path).unwrap().len();
        let first = refresh_or_create(None, &path, full_len / 2, ContentType::Gif, full_len).unwrap();
        let first_frame_count = first.render_image().frame_count();
        let first_ptr = Arc::as_ptr(first.render_image());

        // Same contiguous_len again — must reuse the SAME Arc, not
        // re-decode (proven by pointer identity, not just frame count
        // equality, which a fresh decode of the same bytes would also
        // satisfy).
        let second = refresh_or_create(Some(first), &path, full_len / 2, ContentType::Gif, full_len).unwrap();
        assert_eq!(Arc::as_ptr(second.render_image()), first_ptr, "must reuse the cached Arc, not re-decode");
        assert_eq!(second.render_image().frame_count(), first_frame_count);
    }

    #[test]
    fn refresh_decodes_more_frames_once_contiguous_len_grows() {
        let path = giphy_gif_path();
        let full_len = std::fs::metadata(&path).unwrap().len();
        let first = refresh_or_create(None, &path, full_len / 4, ContentType::Gif, full_len).unwrap();
        let first_frame_count = first.render_image().frame_count();

        let second = refresh_or_create(Some(first), &path, full_len / 2, ContentType::Gif, full_len).unwrap();
        assert!(
            second.render_image().frame_count() >= first_frame_count,
            "more available bytes must never decode fewer frames"
        );
    }

    #[test]
    fn refresh_reaches_all_frames_once_contiguous_len_covers_the_whole_file() {
        let path = giphy_gif_path();
        let full_len = std::fs::metadata(&path).unwrap().len();
        let player = refresh_or_create(None, &path, full_len, ContentType::Gif, full_len).unwrap();
        assert_eq!(player.render_image().frame_count(), 47, "giphy.gif fixture is known to have 47 frames");
        assert!(player.complete);
    }

    #[test]
    fn complete_player_is_not_re_decoded_even_if_contiguous_len_grows_further() {
        let path = giphy_gif_path();
        let full_len = std::fs::metadata(&path).unwrap().len();
        let complete = refresh_or_create(None, &path, full_len, ContentType::Gif, full_len).unwrap();
        let ptr = Arc::as_ptr(complete.render_image());

        // Passing a larger contiguous_len than the file's own length
        // (simulating a stale/retransmitted watermark) must not trigger
        // another decode attempt once already complete.
        let still =
            refresh_or_create(Some(complete), &path, full_len + 1000, ContentType::Gif, full_len).unwrap();
        assert_eq!(Arc::as_ptr(still.render_image()), ptr, "a complete player must never be re-decoded");
    }

    #[test]
    fn single_frame_player_is_not_animating() {
        let path = giphy_gif_path();
        // A tiny prefix — right at the boundary where only the first
        // frame is decodable — is exactly the regime this checks (as
        // opposed to reasoning about a synthetic single-frame GIF fixture,
        // which this module doesn't otherwise need).
        let full_len = std::fs::metadata(&path).unwrap().len();
        let mut available = 64u64;
        let player = loop {
            if let Some(player) = refresh_or_create(None, &path, available, ContentType::Gif, full_len) {
                break player;
            }
            available += 64;
            assert!(available < full_len, "must find a decodable prefix before exhausting the file");
        };
        if player.render_image().frame_count() == 1 {
            assert!(!player.is_animating());
        }
    }

    #[test]
    fn multi_frame_player_is_animating() {
        let path = giphy_gif_path();
        let full_len = std::fs::metadata(&path).unwrap().len();
        let player = refresh_or_create(None, &path, full_len, ContentType::Gif, full_len).unwrap();
        assert!(player.is_animating(), "the full 47-frame giphy.gif must report as animating");
    }

    #[test]
    fn current_frame_advances_past_the_first_frame_delay() {
        let path = giphy_gif_path();
        let full_len = std::fs::metadata(&path).unwrap().len();
        let player = refresh_or_create(None, &path, full_len, ContentType::Gif, full_len).unwrap();

        let first = player.current_frame();
        assert_eq!(first, 0, "playback must start on frame 0");

        // giphy.gif's frames are documented elsewhere in this codebase
        // (rich_content_gif_player's tests) as having a real nonzero
        // delay — sleeping past any plausible per-frame delay and
        // checking again proves advancement actually happens, without
        // hardcoding this fixture's exact delay value here.
        std::thread::sleep(Duration::from_millis(500));
        let later = player.current_frame();
        assert_ne!(later, first, "playback must have advanced past the first frame after waiting");
    }

    #[test]
    fn from_prefix_converts_rgba_to_bgra() {
        // Confirmed live (2026-08-10): without this swap, the paint path
        // showed visibly wrong colors on a real GIF (skin/hair/upholstery
        // all shifted toward blue) — gpui::RenderImage requires BGRA (see
        // that type's own doc comment) but image::Frame/the gif decoder
        // only ever produce RGBA. A synthetic 2-pixel (red, blue) GIF
        // makes the exact byte-level effect unambiguous: if the swap
        // works, red's R/B bytes trade places (0xFF,0x00,0x00 ->
        // 0x00,0x00,0xFF) and blue's do too (0x00,0x00,0xFF ->
        // 0xFF,0x00,0x00) — anything else means the swap either didn't
        // run or ran on the wrong bytes.
        let path = write_two_pixel_gif("bgra_swap");
        let full_len = std::fs::metadata(&path).unwrap().len();
        let player = refresh_or_create(None, &path, full_len, ContentType::Gif, full_len)
            .expect("tiny fixture must decode fully");

        let bytes = player.render_image().as_bytes(0).expect("frame 0 must have pixel data");
        assert_eq!(&bytes[0..4], &[0, 0, 255, 255], "solid red RGBA (255,0,0,255) must become BGRA (0,0,255,255)");
        assert_eq!(&bytes[4..8], &[255, 0, 0, 255], "solid blue RGBA (0,0,255,255) must become BGRA (255,0,0,255)");

        std::fs::remove_file(&path).ok();
    }
}
