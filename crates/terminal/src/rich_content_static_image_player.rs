//! Decoding for Som's own rich-content protocol's static (non-animated)
//! raster formats — JPEG, PNG. Unlike
//! [`crate::rich_content_gif_player`]'s progressive decode (the `gif`
//! crate can decode a truncated prefix and still return whatever frames
//! completed so far), neither JPEG nor PNG have that story in the `image`
//! crate: a truncated file just fails to decode at all, with no reliable
//! way to tell "not enough bytes yet" apart from "genuinely corrupt".
//! Callers therefore only attempt a decode once the full transfer is
//! known to be complete (`contiguous_len == total_size`), same principle
//! as waiting for a GIF's own trailer byte, just enforced by the caller
//! instead of discoverable mid-decode here.

/// Decodes a complete in-memory buffer as a single-frame image, mirroring
/// [`crate::rich_content_gif_player::DecodedPrefix`]'s shape (a `Vec` of
/// one `image::Frame`, always `complete: true`) so
/// `rich_content_player::RichContentPlayer::from_prefix` doesn't need a
/// separate code path for static formats.
pub fn decode_complete(bytes: &[u8]) -> Result<crate::rich_content_gif_player::DecodedPrefix, String> {
    let image = image::load_from_memory(bytes).map_err(|e| format!("decoding image: {e}"))?;
    let frame = image::Frame::new(image.to_rgba8());
    Ok(crate::rich_content_gif_player::DecodedPrefix { frames: vec![frame], complete: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_png() -> Vec<u8> {
        use image::{Rgba, RgbaImage};
        let mut buffer = RgbaImage::new(2, 1);
        buffer.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        buffer.put_pixel(1, 0, Rgba([0, 0, 255, 255]));
        let mut bytes = Vec::new();
        buffer.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        bytes
    }

    #[test]
    fn decodes_a_complete_png_into_a_single_frame() {
        let bytes = encode_png();
        let prefix = decode_complete(&bytes).expect("a valid PNG must decode");
        assert_eq!(prefix.frames.len(), 1);
        assert!(prefix.complete);
    }

    #[test]
    fn rejects_a_truncated_file() {
        let bytes = encode_png();
        let truncated = &bytes[..bytes.len() / 2];
        assert!(decode_complete(truncated).is_err(), "a truncated PNG must not decode successfully");
    }
}
