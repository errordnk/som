//! Progressive GIF decoding for Som's own rich-content protocol — reads
//! however many contiguous bytes [`crate::rich_content_srv_channel::
//! SrvProgressState`] currently reports as available and tries to
//! decode as many frames as that prefix supports, without waiting for
//! the whole file. Re-tried on every new chunk arrival; a decode failure
//! caused by the file simply being incomplete so far is expected and
//! silent (not an error to surface), while a genuinely malformed GIF is
//! reported once and then not retried pointlessly forever.
//!
//! Same "accumulate, then make frames available" shape as
//! `crate::kitty_graphics_store`'s in-memory `PendingFrames`/`DecodedImage`
//! — both read through `image`'s own GIF decoder rather than parsing
//! Kitty's `a=f`/`a=a` command stream frame-by-frame.

use std::time::Duration;

use image::AnimationDecoder as _;
use image::codecs::gif::GifDecoder;

/// One successfully-decoded prefix of a progressively-arriving GIF.
///
/// No `Debug`/`PartialEq` derive — `image::Frame` itself implements
/// neither, so tests compare `frames.len()`/individual frame properties
/// instead of whole-struct equality or automatic `{:?}` formatting.
pub struct DecodedPrefix {
    pub frames: Vec<image::Frame>,
    /// Whether decoding ran all the way to the GIF's own trailer byte
    /// (0x3B) rather than stopping because the available bytes just ran
    /// out mid-frame. `true` means no further decode attempts are needed
    /// even if more chunks keep arriving (a client re-sending a file with
    /// itself as content is not this module's problem to guard against).
    pub complete: bool,
}

/// Tries to decode as many complete frames as possible from `bytes` (the
/// first `contiguous_len` bytes already received, per the caller).
/// Returns `Ok(None)` when there isn't enough data yet for even a valid
/// GIF header/first frame — this is the expected, silent "try again once
/// more bytes arrive" case, NOT a `Result::Err`. Returns `Err` only for
/// a file that's actually malformed in a way more data arriving wouldn't
/// fix (e.g. it doesn't start with a GIF signature at all).
pub fn try_decode_progressive(bytes: &[u8]) -> Result<Option<DecodedPrefix>, String> {
    // `AnimationDecoder` needs `BufRead + Seek` — `std::io::Cursor` gives
    // both over a plain `&[u8]` with no copy, and `bytes` is already
    // exactly "the available prefix" (no `Take`/length-limiting wrapper
    // needed the way the old file-backed version required).
    let decoder = match GifDecoder::new(std::io::Cursor::new(bytes)) {
        Ok(decoder) => decoder,
        // A truncated header (not enough bytes yet for even the fixed
        // GIF signature/logical screen descriptor) surfaces as
        // `ImageError::Decoding` wrapping the underlying `gif` crate's
        // own `DecodingError::UnexpectedEof` (NOT `ImageError::IoError`
        // — confirmed via `gif-0.14.2/src/reader/decoder.rs`: that
        // variant's own `source()` returns `None`, it isn't a wrapped
        // `std::io::Error` at all, so matching on `ImageError::IoError`
        // never catches it). `is_truncation_error` below is what
        // actually distinguishes "not enough data yet" from a real
        // format error.
        Err(err) if is_truncation_error(&err) => return Ok(None),
        Err(other) => return Err(format!("not a valid GIF: {other}")),
    };

    let mut frames = Vec::new();
    let mut complete = true;
    for frame_result in decoder.into_frames() {
        match frame_result {
            Ok(frame) => frames.push(frame),
            // Same truncation-vs-real-error distinction as above, now
            // partway through frame iteration instead of the initial
            // header parse. Whatever frames decoded successfully BEFORE
            // this one are still kept and returned — a progressive
            // player can display them immediately rather than throwing
            // away already-valid frames while waiting for the rest.
            Err(err) if is_truncation_error(&err) => {
                complete = false;
                break;
            },
            Err(other) => return Err(format!("frame decode failed: {other}")),
        }
    }

    if frames.is_empty() {
        return Ok(None);
    }
    Ok(Some(DecodedPrefix { frames, complete }))
}

/// Whether an `ImageError` from the GIF decoder means "the available
/// bytes just run out mid-stream" (more data arriving later would fix
/// this) as opposed to a genuine format error. The `gif` crate's own
/// `DecodingError::UnexpectedEof` variant (see
/// `gif-0.14.2/src/reader/decoder.rs`) is a bare enum case with no
/// wrapped `std::io::Error` to match on structurally — its `Display`
/// text ("Unexpected End of File") is the only distinguishing signal
/// `image::ImageError::Decoding` exposes for it without depth-first
/// downcasting into `gif`'s own error type (which `image` doesn't
/// re-export as a public dependency to downcast against). Substring
/// matching a specific, version-pinned dependency's error text is
/// fragile in the abstract, but the exact case tested for here
/// (`envelope_roundtrip_*`/`partial_prefix_*` tests in this module,
/// exercised against the checked-in `giphy.gif` fixture) will fail loudly
/// (a wrong `Err` instead of `Ok(None)`) if a `gif`/`image` version bump
/// ever changes this wording, rather than silently misbehaving.
fn is_truncation_error(err: &image::ImageError) -> bool {
    matches!(err, image::ImageError::Decoding(_)) && err.to_string().contains("Unexpected End of File")
}

/// Convenience for callers that just want a frame's display duration as a
/// `Duration` — `image::Frame::delay()` returns its own `Delay` newtype
/// (numerator/denominator pair), this converts it the same way
/// `crates/somsrp`'s `decode_gif_frames` already does on the sending
/// side, kept consistent so a gap value round-trips the same on both
/// ends of the pipeline.
pub fn frame_delay(frame: &image::Frame) -> Duration {
    Duration::from(frame.delay())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn giphy_gif_bytes() -> Vec<u8> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../giphy.gif");
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"))
    }

    #[test]
    fn empty_prefix_returns_none_not_error() {
        let result = try_decode_progressive(&[]).unwrap();
        assert!(result.is_none(), "zero available bytes must be Ok(None), not an error");
    }

    #[test]
    fn tiny_prefix_before_any_full_frame_returns_none_not_error() {
        let bytes = giphy_gif_bytes();
        // 32 bytes is enough for the GIF signature + logical screen
        // descriptor but nowhere near a full first frame's compressed
        // image data for this fixture.
        let result = try_decode_progressive(&bytes[..32]).unwrap();
        assert!(result.is_none(), "a header-only prefix must be Ok(None), not an error, until a full frame lands");
    }

    #[test]
    fn full_file_decodes_all_frames_and_reports_complete() {
        let bytes = giphy_gif_bytes();
        let result = try_decode_progressive(&bytes).unwrap().expect("full file must decode");
        assert_eq!(result.frames.len(), 47, "giphy.gif fixture is known to have 47 frames");
        assert!(result.complete, "decoding the entire file must report complete=true");
    }

    #[test]
    fn partial_prefix_decodes_some_frames_and_reports_incomplete() {
        let bytes = giphy_gif_bytes();
        // Half the file: enough for several frames, but the file is cut
        // off mid-stream, not at the trailer byte.
        let half = bytes.len() / 2;
        let result = try_decode_progressive(&bytes[..half]).unwrap().expect("half the file must yield some frames");
        assert!(result.frames.len() > 0, "a partial prefix should still decode at least one frame");
        assert!(result.frames.len() < 47, "a partial prefix must not report every frame the full file has");
        assert!(!result.complete, "a truncated prefix must not report complete=true");
    }

    #[test]
    fn progressively_growing_prefix_yields_monotonically_more_frames() {
        // The core progressive-playback property: as more bytes become
        // available, the decoded frame count never goes DOWN across
        // successive attempts on the same growing file — a real player
        // polling this on every chunk arrival must never see time run
        // backward.
        let bytes = giphy_gif_bytes();

        let mut last_frame_count = 0usize;
        let steps = [bytes.len() / 10, bytes.len() / 4, bytes.len() / 2, bytes.len() * 3 / 4, bytes.len()];
        for &available in &steps {
            if let Some(decoded) = try_decode_progressive(&bytes[..available]).unwrap() {
                assert!(
                    decoded.frames.len() >= last_frame_count,
                    "frame count must never decrease as more bytes become available (was {last_frame_count}, now {})",
                    decoded.frames.len()
                );
                last_frame_count = decoded.frames.len();
            }
        }
        assert_eq!(last_frame_count, 47, "the final, full-length attempt must reach all 47 frames");
    }

    #[test]
    fn not_a_gif_at_all_is_a_real_error() {
        let result = try_decode_progressive(b"this is definitely not a GIF file, no signature here");
        assert!(result.is_err(), "non-GIF content must be a real error, not Ok(None)");
    }

    #[test]
    fn frame_delay_converts_to_a_nonzero_duration() {
        let bytes = giphy_gif_bytes();
        let decoded = try_decode_progressive(&bytes).unwrap().unwrap();
        let first = &decoded.frames[0];
        assert!(frame_delay(first) > Duration::ZERO, "giphy.gif's frames must have a real, nonzero display delay");
    }
}
