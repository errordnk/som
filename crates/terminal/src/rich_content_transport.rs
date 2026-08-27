//! Shared content-description types for Som's rich-content pipeline —
//! independent of the Kitty terminal graphics protocol
//! ([`crate::kitty_graphics`]), which stays untouched for compatibility
//! with third-party Kitty clients (yazi and similar).
//!
//! # History: this module used to carry payload bytes over the PTY too
//!
//! Until the `som-srv` daemon existed, this module ALSO defined a full
//! binary envelope format (`Chunk`, `build_envelope`/`parse_envelope`,
//! `base91_encode`/`base91_decode`, a `Query`/`QueryType` request-response
//! mechanism) that shipped actual file bytes and Som -> client byte-range
//! requests through APC escape sequences (`ESC _ S ... ESC \`) on the same
//! PTY the shell itself uses. That machinery is gone — deleted, not
//! deprecated — now that `som-srv` (see `crates/som_srv`, specifically
//! `som_srv::protocol::SrvRequest::PutChunk` and `::RequestByteRange`)
//! carries payload bytes and byte-range requests over its own binary side
//! channel instead. The reasoning below is kept as HISTORICAL CONTEXT: it
//! explains a real, hard-won constraint of Windows ConPTY that still
//! matters for anything this crate DOES still put on the PTY (right now,
//! that's just the placeholder-grid control handshake — see
//! `crates/somcat/src/main.rs`'s `print_placeholder_grid_with_cell_dims`,
//! which sends a session/file id through a placeholder cell's RGB
//! attributes via ordinary printable-text escape sequences, not through
//! any encoding this module defines).
//!
//! ## Why a byte `>= 0x80` can't survive a real ConPTY
//!
//! The original design of the now-deleted envelope format sent `payload`
//! as raw binary, relying on a patched VTE parser (see `errordnk/vte`'s
//! `advance_apc_string` doc comment) that passes every byte except `ESC`
//! straight through an APC string. That works when bytes are injected
//! directly into the parser (e.g. `Terminal::write_output`) — but Windows'
//! real ConPTY does NOT behave like a transparent byte pipe for a child
//! process's stdout: `conhost`/`OpenConsole.exe` on the other end
//! interprets the child's raw output through the ACTIVE CONSOLE OUTPUT
//! CODEPAGE (a real per-character-cell text buffer, not a byte-oriented
//! one) and re-encodes it to UTF-8 before this process's PTY reader ever
//! sees it. Any byte `>= 0x80` gets silently reinterpreted as whatever
//! character that codepage maps it to, then re-emitted as a multi-byte
//! UTF-8 sequence — turning a single source byte into two or three wire
//! bytes with no way to tell where the damage happened after the fact.
//!
//! Confirmed the hard way, not assumed: `Terminal::write_output` (direct
//! VTE injection, no real ConPTY involved) reconstructed a ~1MB GIF
//! byte-for-byte; the exact same byte stream sent through a REAL `somcat
//! --stream` child process over a REAL ConPTY pseudo-console consistently
//! arrived LARGER than it was sent, with each corrupted envelope's
//! declared `chunk_len` smaller than the actual bytes received (e.g.
//! declared 76, actual 129) — an 8-byte fixture with known high bytes
//! (`c0 80 81 82 ff fe`) showed each byte `>= 0x80` turn into its CP866
//! (the active codepage at diagnosis time) character re-encoded as 2-3
//! UTF-8 bytes. Switching the child's console output codepage to UTF-8
//! (`SetConsoleOutputCP(65001)`) does NOT fix this — it only changes the
//! failure mode (arbitrary binary isn't valid UTF-8, so invalid bytes get
//! replaced with U+FFFD, 3 bytes each, instead of CP866-transliterated).
//! This is Windows console subsystem behavior, entirely outside this
//! protocol's or `alacritty_terminal`'s control — the only way to survive
//! it is to never put a byte `>= 0x80` (or any other value the codepage
//! machinery might reinterpret) on the wire at all. This is exactly why
//! bulk payload bytes no longer travel over the PTY at all — `som-srv`'s
//! side channel is a real binary pipe with no console codepage in the
//! middle — and why anything that DOES still travel over the PTY (the
//! placeholder-grid handshake) must keep using printable-text-safe
//! encodings rather than raw binary.

/// What kind of media a chunk belongs to. Only [`ContentType::Gif`] is
/// wired up end-to-end right now — the others are reserved so the
/// envelope format doesn't need to change shape when audio/markdown/video
/// support is added later, only a new match arm on the receiving side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ContentType {
    Gif = 0,
    Audio = 1,
    Markdown = 2,
    Video = 3,
    /// Static (non-animated) raster formats — unlike `Gif`, these have no
    /// progressive/streaming decode story in the `image` crate (no partial-
    /// prefix decoding), so the receiving side waits for the full file
    /// before attempting to decode either.
    Jpeg = 4,
    Png = 5,
}

/// Known video codecs — placeholder for when [`ContentType::Video`] is
/// wired up end-to-end (not yet). One byte in [`ContentMetadata::Video`],
/// kept as a real enum rather than a bare `u8` so unrecognized values are
/// caught at parse time instead of silently carried through as garbage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VideoCodec {
    Unknown = 0,
    H264 = 1,
    H265 = 2,
    Vp9 = 3,
    Av1 = 4,
    /// MPEG-4 Part 2 (DivX/Xvid-era codecs) — common in older `.avi`
    /// files specifically, which is why this exists even though it
    /// predates every other variant here in practice.
    Mpeg4 = 5,
}

/// Per-content-type metadata describing a piece of rich content —
/// deliberately NOT a single flat set of fields shared across every
/// [`ContentType`]: a still/animated image, an audio stream, a video
/// stream, and markdown text each need genuinely different descriptive
/// fields (pixel dimensions and color depth make no sense for audio;
/// sample rate and channel count make no sense for an image), and a flat
/// struct would force every chunk to carry fields meaningless for its own
/// content type (either wasted wire bytes or, worse, fields silently
/// reinterpreted across unrelated content types as more variants get
/// added later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentMetadata {
    /// A still or animated raster image — GIF today, PNG/JPEG once those
    /// `ContentType`s exist. `width_px`/`height_px` are the DECODED pixel
    /// dimensions (not anything about the file's compressed byte size).
    /// 0/0 means unknown, same "0 = unknown, not empty" convention a
    /// transfer's total-size field uses elsewhere in this pipeline (see
    /// `som_srv::protocol::SrvRequest::PutChunk`'s `total_size` field).
    ///
    /// Sent explicitly by the client (parsed from the source file's own
    /// header before streaming — see `crates/somcat`'s `stream_file`)
    /// rather than left for the receiving side to infer from whatever
    /// prefix of the file happens to have decoded so far: a GIF's logical
    /// screen descriptor isn't guaranteed to land inside the FIRST chunk
    /// once a large file is split, and the paint path needs a stable size
    /// to lay out the placement's grid footprint as soon as ANY chunk for
    /// a new file arrives, not only once decoding has progressed far
    /// enough on its own.
    Image {
        width_px: u32,
        height_px: u32,
        /// Bits per pixel in the DECODED image, e.g. 32 for RGBA.
        color_bits: u8,
        /// Whether the sender knows this file has more than one frame (a
        /// real animation) as opposed to a single still frame —
        /// determined client-side from the source file's own frame count
        /// before streaming, not left for the receiver to discover only
        /// after progressively decoding far enough to see a second
        /// frame.
        is_animated: bool,
    },
    /// [`ContentType::Audio`]'s metadata. `duration_ms` is the whole
    /// track's real duration, known from the source file's own header
    /// (e.g. an MP3's Xing/VBRI VBR tag, or a size/bitrate estimate) —
    /// NOT derived from however many bytes have streamed in so far. This
    /// is what lets Som show an accurate duration/seek-bar length the
    /// moment the FIRST chunk arrives, before the rest of a
    /// multi-gigabyte file has streamed in at all (see
    /// `SRP_PROTOCOL.md`'s progressive-audio section). `0` means unknown,
    /// same "unknown, don't guess" convention [`Self::Image`]'s
    /// `width_px`/`height_px` already use.
    Audio { sample_rate: u32, channels: u8, bits_per_sample: u8, duration_ms: u32 },
    /// Reserved for [`ContentType::Video`] — same rationale as
    /// [`Self::Audio`]. `fps_numerator`/`fps_denominator` (rather than a
    /// single float or rounded integer fps) mirrors how GIF/video
    /// container formats themselves commonly express frame rate as a
    /// rational (e.g. NTSC's 30000/1001), so this doesn't lose precision
    /// converting to/from whatever a real decoder reports.
    Video { width_px: u32, height_px: u32, fps_numerator: u32, fps_denominator: u32, codec: VideoCodec },
    /// [`ContentType::Markdown`] carries no geometric/format metadata at
    /// all — plain text, rendered by whatever overlay eventually consumes
    /// it, not sized like a raster image or timed like audio/video.
    Markdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_metadata_variants_are_distinguishable_by_equality() {
        let image = ContentMetadata::Image { width_px: 480, height_px: 480, color_bits: 32, is_animated: true };
        let audio = ContentMetadata::Audio { sample_rate: 44_100, channels: 2, bits_per_sample: 16, duration_ms: 208_500 };
        let video = ContentMetadata::Video {
            width_px: 1920,
            height_px: 1080,
            fps_numerator: 30000,
            fps_denominator: 1001,
            codec: VideoCodec::Mpeg4,
        };
        let markdown = ContentMetadata::Markdown;

        assert_ne!(image, audio);
        assert_ne!(audio, video);
        assert_ne!(video, markdown);
        assert_eq!(image, image);
        assert_eq!(
            audio,
            ContentMetadata::Audio { sample_rate: 44_100, channels: 2, bits_per_sample: 16, duration_ms: 208_500 }
        );
    }

    #[test]
    fn content_metadata_audio_zero_duration_means_unknown() {
        // `0` is a valid, meaningful value (not a sentinel Rust needs to
        // special-case) — this just documents the convention for future
        // callers, mirroring the same "0 = unknown" rule `ContentMetadata::
        // Image`'s width_px/height_px use.
        let audio = ContentMetadata::Audio { sample_rate: 44_100, channels: 2, bits_per_sample: 16, duration_ms: 0 };
        assert_eq!(audio, ContentMetadata::Audio { sample_rate: 44_100, channels: 2, bits_per_sample: 16, duration_ms: 0 });
    }
}
