//! Wire format for Som's own binary rich-content protocol — completely
//! independent of the Kitty terminal graphics protocol
//! ([`crate::kitty_graphics`]), which stays untouched for compatibility
//! with third-party Kitty clients (yazi and similar). This protocol has
//! its own client, its own APC marker byte, and its own viewer; the two
//! only ever share the same physical PTY channel.
//!
//! Model: progressive file copy. A client (e.g. `somcat --stream`) reads a
//! real media file (GIF today; audio/markdown/video later, distinguished
//! by [`ContentType`]) and sends it chunk by chunk, unmodified except for
//! this module's own wire encoding — no per-frame re-encoding. The
//! receiving side writes each chunk to a local cache file and can start
//! decoding/playing before the whole file has arrived.
//!
//! # Why every byte on the wire is base91, not raw binary
//!
//! The original design of this protocol sent `payload` as raw binary,
//! relying on a patched VTE parser (see `errordnk/vte`'s
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
//! machinery might reinterpret) on the wire at all.
//!
//! Kitty's own graphics protocol sidesteps this identical problem by
//! base64-encoding its payload — turns out that requirement was never
//! only about the historical 7-bit-only VTE parser this project already
//! patched around; it's ALSO required for real ConPTY survival, and the
//! patch doesn't change that. This module goes one step further: a plain
//! 91-symbol alphabet (every printable ASCII byte `0x21..=0x7E` except
//! `"`, `'`, and `\`, chosen to keep `\` away from this protocol's own
//! `ESC \` terminator) fits a basE91-style bit-packing scheme with ~1.23x
//! overhead, better than base64's ~1.33x or plain ASCII85's ~1.25x — all
//! three were tested byte-for-byte against a real ConPTY round-trip (see
//! `project_srp_protocol_checkpoint` memory for the experiment), and all
//! three survive; base91 was chosen for being the cheapest of the three.
//!
//! Because EVERY wire byte is now drawn from that 91-symbol alphabet,
//! there is no dangerous byte value left to guard against — `payload` no
//! longer needs the `trailing_byte`/`split_into_chunks` byte-stuffing
//! machinery an earlier version of this module used to route `0x1B`/`0x0A`
//! around the wire separately. [`encode_chunk_len_and_payload`] handles
//! the whole payload uniformly.
//!
//! # Envelope layout
//!
//! ```text
//! [marker:            1 byte  = b'S']
//! [header_b91:        base91-encoded header fields — see below]
//! [separator:         1 byte  = 0x20 (space)]
//! [payload_b91:       base91-encoded raw chunk payload]
//! ```
//!
//! The header, before base91 encoding, is:
//! ```text
//! [version:           1 byte  = 1]
//! [content_type:      1 byte  = ContentType as u8]
//! [session_id:        4 bytes LE]
//! [file_id:           4 bytes LE]
//! [chunk_offset:      8 bytes LE]
//! [chunk_len:         4 bytes LE, length of the RAW (pre-encoding) payload]
//! [total_size:        8 bytes LE, 0 = unknown]
//! [width_px:          4 bytes LE, natural decoded pixel width, 0 = unknown]
//! [height_px:         4 bytes LE, natural decoded pixel height, 0 = unknown]
//! [color_bits:        1 byte, bits per pixel in the decoded image]
//! [is_animated:       1 byte, 0 or 1 — see Chunk::is_animated]
//! [crc32:             4 bytes LE, over the raw payload]
//! ```
//! `chunk_len` and `crc32` describe the raw payload bytes, not their
//! base91-encoded size on the wire — [`RichContentCache`](crate::rich_content_cache::RichContentCache)
//! and every downstream decoder work exclusively in terms of real file
//! bytes and must never need to know this module's wire encoding exists.
//!
//! `width_px`/`height_px`/`color_bits`/`is_animated` travel on EVERY chunk
//! (not just the first) rather than being a separate one-time command —
//! this keeps the envelope format uniform (a receiver never needs to
//! special-case "the first chunk looks different") at the cost of a few
//! redundant bytes per chunk, which base91's own per-byte overhead
//! already dwarfs. See [`Chunk::width_px`]'s doc comment for why the
//! paint path needs this sent explicitly rather than inferred from
//! however much of the file happens to be decoded so far.
//!
//! Sent as `ESC _ S <header_b91> <SEPARATOR> <payload_b91> ESC \` — the
//! leading `S` (Som) is what a receiver checks first to route bytes here
//! instead of to [`crate::kitty_graphics::parse_command`] (which requires
//! a leading `G` and returns `None` otherwise, so the two parsers are
//! mutually exclusive by construction, not by convention). A literal
//! `0x20` separates the two base91-encoded regions — see [`SEPARATOR`]'s
//! doc comment for why a fixed byte offset can't do this job the way the
//! header/payload split in this module's earlier hex-encoded design
//! could.

use std::fmt;

/// Leading byte inside the APC string (right after `ESC _`) that marks
/// this as Som's own protocol, not Kitty's (`G`) or anything else.
pub const MARKER: u8 = b'S';

/// Wire format version — bumped if the envelope layout below ever needs a
/// breaking change. Only version 1 exists today; a receiver seeing any
/// other value should reject the envelope rather than guess.
pub const VERSION: u8 = 1;

/// Size, in raw (pre-base91) bytes, of the fixed header fields that come
/// after [`MARKER`] — version, content_type, session_id, file_id,
/// chunk_offset, chunk_len, total_size, encoded `ContentMetadata`
/// ([`METADATA_ENCODED_LEN`] bytes — see that constant and
/// [`ContentMetadata`]'s own doc comment for why this is fixed-length
/// regardless of which variant a chunk carries), crc32. See this module's
/// doc comment for the exact field layout.
const HEADER_FIELDS_LEN: usize = 1 + 1 + 4 + 4 + 8 + 4 + 8 + METADATA_ENCODED_LEN + 4;

/// Separates the base91-encoded header from the base91-encoded payload on
/// the wire. basE91's bit-packing means the ENCODED length of a
/// fixed-length input varies with the actual bit pattern (13 or 14 bits
/// consumed per output symbol pair, decided by the data itself — see
/// [`base91_encode`]'s doc comment) — so, unlike this module's earlier
/// hex-header design, there is no fixed byte offset a receiver could slice
/// the header out at. A literal `0x20` (space) works as an unambiguous
/// separator because it sits directly below [`BASE91_ALPHABET`]'s lower
/// bound (`0x21`) and therefore can never appear inside either
/// base91-encoded region.
const SEPARATOR: u8 = 0x20;

/// Every printable ASCII byte (`0x21..=0x7E`, 94 values) except `"`, `'`,
/// and `\` — 91 symbols. `\` is avoided specifically to keep it away from
/// this protocol's `ESC \` terminator; `"`/`'` are avoided on general
/// principle (common shell/JSON quoting characters, no reason to risk
/// interacting badly with some future tool built on top of this wire
/// format). See this module's doc comment for why this alphabet exists at
/// all — confirmed byte-for-byte lossless across a real ConPTY round-trip.
const BASE91_ALPHABET: [u8; 91] = build_base91_alphabet();

const fn build_base91_alphabet() -> [u8; 91] {
    let mut alphabet = [0u8; 91];
    let mut i = 0;
    let mut b = 0x21u8;
    while b <= 0x7E {
        if b != b'"' && b != 0x27 && b != 0x5C {
            alphabet[i] = b;
            i += 1;
        }
        b += 1;
    }
    debug_assert!(i == 91);
    alphabet
}

/// Reverse lookup table: wire byte -> its index in [`BASE91_ALPHABET`], or
/// `255` for any byte that isn't part of the alphabet at all (used by
/// [`base91_decode`] to reject malformed input instead of guessing).
const BASE91_INDEX: [u8; 256] = build_base91_index();

const fn build_base91_index() -> [u8; 256] {
    let mut index = [255u8; 256];
    let mut i = 0;
    while i < 91 {
        index[BASE91_ALPHABET[i] as usize] = i as u8;
        i += 1;
    }
    index
}

/// An upper bound (not the exact length — see [`SEPARATOR`]'s doc comment
/// for why basE91's encoded length isn't a pure function of the input
/// length) on how many base91 wire bytes encode `raw_len` raw bytes. Used
/// only to pre-size a `Vec` via `with_capacity`, never to locate a byte
/// offset in an actual envelope.
const fn base91_encoded_len(raw_len: usize) -> usize {
    // Each input byte contributes 8 bits; the encoder emits 2 output
    // symbols per 13-14 input bits, i.e. call it 16 bits amortized per
    // symbol PAIR in the worst case (13-bit case, the denser packing is
    // strictly smaller) — `raw_len * 8 * 2 / 13`, rounded up, plus room
    // for the up-to-2-symbol tail the encoder flushes at the end.
    (raw_len * 8 * 2).div_ceil(13) + 2
}

/// Encodes `data` with [`BASE91_ALPHABET`] using the standard basE91
/// bit-packing algorithm: bits are packed LSB-first into a growing
/// buffer, and every time the buffer holds more than 13 bits, two output
/// symbols are emitted, consuming either 13 or 14 bits depending on
/// whether the low 13 bits alone already exceed the largest value a
/// single base-91 digit pair below the alphabet size can represent
/// (`13 bits -> 8192 max value vs 91*91-1 = 8280`, so most 13-bit values
/// fit two digits; the ones that don't (> 8180) borrow a 14th bit instead
/// — the same trade-off the reference basE91 implementation uses).
fn base91_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(base91_encoded_len(data.len()));
    let mut bit_buf: u32 = 0;
    let mut bit_count: u32 = 0;
    for &byte in data {
        bit_buf |= (byte as u32) << bit_count;
        bit_count += 8;
        if bit_count > 13 {
            let mut v = bit_buf & 8191; // low 13 bits
            if v > 88 {
                bit_buf >>= 13;
                bit_count -= 13;
            } else {
                v = bit_buf & 16383; // low 14 bits
                bit_buf >>= 14;
                bit_count -= 14;
            }
            out.push(BASE91_ALPHABET[(v % 91) as usize]);
            out.push(BASE91_ALPHABET[(v / 91) as usize]);
        }
    }
    if bit_count > 0 {
        out.push(BASE91_ALPHABET[(bit_buf % 91) as usize]);
        if bit_count > 7 || bit_buf > 90 {
            out.push(BASE91_ALPHABET[(bit_buf / 91) as usize]);
        }
    }
    out
}

/// Reverses [`base91_encode`]. Returns `None` if `data` contains any byte
/// outside [`BASE91_ALPHABET`] — a malformed/corrupted envelope, not
/// something to guess through.
fn base91_decode(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 13 / 16 + 1);
    let mut bit_buf: u32 = 0;
    let mut bit_count: u32 = 0;
    let mut pending: i32 = -1;
    for &byte in data {
        let digit = BASE91_INDEX[byte as usize];
        if digit == 255 {
            return None;
        }
        if pending == -1 {
            pending = digit as i32;
            continue;
        }
        let v = pending + digit as i32 * 91;
        pending = -1;
        bit_buf |= (v as u32) << bit_count;
        bit_count += if (v & 8191) > 88 { 13 } else { 14 };
        while bit_count >= 8 {
            out.push((bit_buf & 0xFF) as u8);
            bit_buf >>= 8;
            bit_count -= 8;
        }
    }
    if pending != -1 {
        bit_buf |= (pending as u32) << bit_count;
        out.push((bit_buf & 0xFF) as u8);
    }
    Some(out)
}

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

impl ContentType {
    fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Gif),
            1 => Some(Self::Audio),
            2 => Some(Self::Markdown),
            3 => Some(Self::Video),
            4 => Some(Self::Jpeg),
            5 => Some(Self::Png),
            _ => None,
        }
    }
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
}

impl VideoCodec {
    fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Unknown),
            1 => Some(Self::H264),
            2 => Some(Self::H265),
            3 => Some(Self::Vp9),
            4 => Some(Self::Av1),
            _ => None,
        }
    }
}

/// Per-content-type metadata carried in every chunk's header — deliberately
/// NOT a single flat set of fields shared across every [`ContentType`]:
/// a still/animated image, an audio stream, a video stream, and markdown
/// text each need genuinely different descriptive fields (pixel
/// dimensions and color depth make no sense for audio; sample rate and
/// channel count make no sense for an image), and a flat struct would
/// force every chunk to carry fields meaningless for its own content type
/// (either wasted wire bytes or, worse, fields silently reinterpreted
/// across unrelated content types as more variants get added later).
///
/// Every variant's fields are fixed-size integers (no `Vec`/`String`), so
/// the WHOLE enum still encodes to a fixed number of header bytes — see
/// [`metadata_field_count`] and [`encode_metadata`]/[`decode_metadata`]
/// for how a shorter variant (e.g. [`Self::Markdown`], which carries no
/// fields at all) is padded out to the same length as the longest
/// variant ([`Self::Video`]) rather than needing a variable-length
/// header the way [`SEPARATOR`] exists to handle for `payload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentMetadata {
    /// A still or animated raster image — GIF today, PNG/JPEG once those
    /// `ContentType`s exist. `width_px`/`height_px` are the DECODED pixel
    /// dimensions (not anything about the file's compressed byte size).
    /// 0/0 means unknown, mirroring `Chunk::total_size`'s own convention.
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
    /// Reserved for [`ContentType::Audio`] — not wired up to any decoder
    /// yet, but the field shape is settled now so adding real audio
    /// support later doesn't need another header-format break.
    Audio { sample_rate: u32, channels: u8, bits_per_sample: u8 },
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

/// One fully-parsed chunk of a progressive file transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub content_type: ContentType,
    /// Sender-assigned, unique per client process (time-based, same
    /// principle as `crates/somcat`'s existing per-invocation image id —
    /// see that module's doc comment on why a fixed id was a real, once
    /// shipped bug: it made a second invocation silently overwrite the
    /// first's data instead of appearing as a separate item).
    pub session_id: u32,
    /// Unique per file within a session — a single client run could
    /// stream more than one file (mirrors somcat's existing `next_image_id`
    /// counter, one per `display_image` call).
    pub file_id: u32,
    pub chunk_offset: u64,
    /// Total file size in bytes, if the sender knows it upfront (e.g.
    /// reading from a regular file with a known length). 0 means unknown
    /// — a receiver must not treat 0 as "empty file", only as "don't know
    /// yet", and should rely on chunk arrival order/completion signaling
    /// from a higher layer instead.
    pub total_size: u64,
    /// See [`ContentMetadata`]'s own doc comment for why this isn't a
    /// flat set of fields shared across every `ContentType`.
    pub metadata: ContentMetadata,
    /// Raw (decoded) file bytes for this chunk — [`build_envelope`]
    /// base91-encodes this for the wire; [`parse_envelope`] decodes it
    /// back. No byte value is off-limits here; the wire encoding handles
    /// all 256 possible values uniformly (see this module's doc comment).
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// No [`SEPARATOR`] byte found after [`MARKER`] — not even a complete
    /// header, since a well-formed envelope always has one between the
    /// header and payload.
    TooShort,
    /// Leading byte wasn't [`MARKER`] — not this protocol's envelope at
    /// all (could be Kitty's `G`, or garbage).
    WrongMarker,
    /// The header's base91 encoding contained a byte outside
    /// [`BASE91_ALPHABET`], or decoded to the wrong number of raw bytes —
    /// a corrupted or truncated envelope, or (if this ever fires) a
    /// sender bug in [`build_envelope`] itself.
    InvalidHeaderEncoding,
    /// The payload's base91 encoding contained a byte outside
    /// [`BASE91_ALPHABET`].
    InvalidPayloadEncoding,
    /// `version` field didn't match [`VERSION`].
    UnsupportedVersion(u8),
    /// `content_type` byte didn't match any [`ContentType`] variant.
    UnknownContentType(u8),
    /// The encoded [`ContentMetadata`] had an unrecognized discriminant
    /// tag, or (specifically for [`ContentMetadata::Video`]) an
    /// unrecognized [`VideoCodec`] byte.
    InvalidMetadata,
    /// `chunk_len` in the header didn't match the actual decoded payload
    /// length — a truncated or corrupted envelope.
    LengthMismatch { declared: u32, actual: usize },
    /// The payload's CRC32 didn't match the header's `crc32` field — bit
    /// corruption somewhere in transit. The chunk is discarded, not
    /// applied; a real GIF/audio decode downstream would otherwise fail
    /// far less legibly on subtly-wrong pixel/sample data.
    ChecksumMismatch { expected: u32, actual: u32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => write!(f, "envelope shorter than the fixed header"),
            Self::WrongMarker => write!(f, "leading byte is not the rich-content marker"),
            Self::InvalidHeaderEncoding => write!(f, "header base91 encoding is malformed"),
            Self::InvalidPayloadEncoding => write!(f, "payload base91 encoding is malformed"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported envelope version {v}"),
            Self::UnknownContentType(b) => write!(f, "unknown content type byte {b}"),
            Self::InvalidMetadata => write!(f, "content metadata tag or codec byte is unrecognized"),
            Self::LengthMismatch { declared, actual } => {
                write!(f, "declared chunk_len {declared} does not match actual payload length {actual}")
            },
            Self::ChecksumMismatch { expected, actual } => {
                write!(f, "payload CRC32 mismatch: expected {expected:#010x}, got {actual:#010x}")
            },
        }
    }
}

/// CRC32 (IEEE 802.3 polynomial, same table-driven algorithm `zip`/`png`
/// use) — implemented locally rather than pulling in a crate, since the
/// full generator-polynomial table is under 1KB of source and this is the
/// only place in the workspace that needs a checksum, not a general
/// compression/archival need.
fn crc32(data: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB88320;
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (POLY & mask);
        }
    }
    !crc
}

/// `ContentMetadata` discriminant bytes on the wire — deliberately NOT
/// tied to `ContentType`'s own discriminant values (a future content type
/// could reuse an existing metadata shape, e.g. a second image format
/// sharing [`ContentMetadata::Image`]), so this is its own independent
/// enumeration.
const METADATA_TAG_IMAGE: u8 = 0;
const METADATA_TAG_AUDIO: u8 = 1;
const METADATA_TAG_VIDEO: u8 = 2;
const METADATA_TAG_MARKDOWN: u8 = 3;

/// How many raw bytes [`ContentMetadata`]'s field payload (everything
/// after the 1-byte discriminant) occupies on the wire — fixed at the
/// longest variant's size ([`ContentMetadata::Video`]: 4+4+4+4+1 = 17
/// bytes) so every variant, including [`ContentMetadata::Markdown`]
/// (which has no fields of its own), encodes to the exact same header
/// length. This is what lets [`HEADER_FIELDS_LEN`] stay a plain constant
/// instead of depending on which variant a given chunk happens to carry.
const METADATA_FIELDS_LEN: usize = 4 + 4 + 4 + 4 + 1;

/// Total wire size of an encoded [`ContentMetadata`]: 1 discriminant byte
/// plus [`METADATA_FIELDS_LEN`] bytes of (possibly zero-padded) fields.
const METADATA_ENCODED_LEN: usize = 1 + METADATA_FIELDS_LEN;

/// Encodes `metadata` into exactly [`METADATA_ENCODED_LEN`] raw bytes:
/// a 1-byte discriminant tag followed by that variant's fields packed in
/// declaration order (as fixed-width little-endian integers), zero-padded
/// out to [`METADATA_FIELDS_LEN`] for any variant shorter than
/// [`ContentMetadata::Video`].
fn encode_metadata(metadata: ContentMetadata) -> [u8; METADATA_ENCODED_LEN] {
    let mut out = [0u8; METADATA_ENCODED_LEN];
    out[0] = match metadata {
        ContentMetadata::Image { .. } => METADATA_TAG_IMAGE,
        ContentMetadata::Audio { .. } => METADATA_TAG_AUDIO,
        ContentMetadata::Video { .. } => METADATA_TAG_VIDEO,
        ContentMetadata::Markdown => METADATA_TAG_MARKDOWN,
    };
    let fields = &mut out[1..];
    match metadata {
        ContentMetadata::Image { width_px, height_px, color_bits, is_animated } => {
            fields[0..4].copy_from_slice(&width_px.to_le_bytes());
            fields[4..8].copy_from_slice(&height_px.to_le_bytes());
            fields[8] = color_bits;
            fields[9] = is_animated as u8;
        },
        ContentMetadata::Audio { sample_rate, channels, bits_per_sample } => {
            fields[0..4].copy_from_slice(&sample_rate.to_le_bytes());
            fields[4] = channels;
            fields[5] = bits_per_sample;
        },
        ContentMetadata::Video { width_px, height_px, fps_numerator, fps_denominator, codec } => {
            fields[0..4].copy_from_slice(&width_px.to_le_bytes());
            fields[4..8].copy_from_slice(&height_px.to_le_bytes());
            fields[8..12].copy_from_slice(&fps_numerator.to_le_bytes());
            fields[12..16].copy_from_slice(&fps_denominator.to_le_bytes());
            fields[16] = codec as u8;
        },
        ContentMetadata::Markdown => {},
    }
    out
}

/// Reverses [`encode_metadata`]. `bytes` must be exactly
/// [`METADATA_ENCODED_LEN`] long (callers slice it out of the already
/// fixed-length decoded header — see [`parse_envelope`]). Returns `None`
/// for an unrecognized discriminant tag or (for [`ContentMetadata::Video`])
/// an unrecognized [`VideoCodec`] byte — both are treated the same as any
/// other malformed-header case by the caller.
fn decode_metadata(bytes: &[u8]) -> Option<ContentMetadata> {
    debug_assert_eq!(bytes.len(), METADATA_ENCODED_LEN);
    let fields = &bytes[1..];
    match bytes[0] {
        METADATA_TAG_IMAGE => Some(ContentMetadata::Image {
            width_px: u32::from_le_bytes(fields[0..4].try_into().unwrap()),
            height_px: u32::from_le_bytes(fields[4..8].try_into().unwrap()),
            color_bits: fields[8],
            is_animated: fields[9] != 0,
        }),
        METADATA_TAG_AUDIO => Some(ContentMetadata::Audio {
            sample_rate: u32::from_le_bytes(fields[0..4].try_into().unwrap()),
            channels: fields[4],
            bits_per_sample: fields[5],
        }),
        METADATA_TAG_VIDEO => Some(ContentMetadata::Video {
            width_px: u32::from_le_bytes(fields[0..4].try_into().unwrap()),
            height_px: u32::from_le_bytes(fields[4..8].try_into().unwrap()),
            fps_numerator: u32::from_le_bytes(fields[8..12].try_into().unwrap()),
            fps_denominator: u32::from_le_bytes(fields[12..16].try_into().unwrap()),
            codec: VideoCodec::from_u8(fields[16])?,
        }),
        METADATA_TAG_MARKDOWN => Some(ContentMetadata::Markdown),
        _ => None,
    }
}

/// Builds one complete envelope ready to be wrapped in `ESC _ S ... ESC \`
/// and written to a PTY.
pub fn build_envelope(chunk: &Chunk) -> Vec<u8> {
    let mut fields = Vec::with_capacity(HEADER_FIELDS_LEN);
    fields.push(VERSION);
    fields.push(chunk.content_type as u8);
    fields.extend_from_slice(&chunk.session_id.to_le_bytes());
    fields.extend_from_slice(&chunk.file_id.to_le_bytes());
    fields.extend_from_slice(&chunk.chunk_offset.to_le_bytes());
    fields.extend_from_slice(&(chunk.payload.len() as u32).to_le_bytes());
    fields.extend_from_slice(&chunk.total_size.to_le_bytes());
    fields.extend_from_slice(&encode_metadata(chunk.metadata));
    fields.extend_from_slice(&crc32(&chunk.payload).to_le_bytes());
    debug_assert_eq!(fields.len(), HEADER_FIELDS_LEN);

    let header_b91 = base91_encode(&fields);

    let mut envelope = Vec::with_capacity(1 + header_b91.len() + 1 + chunk.payload.len() * 2);
    envelope.push(MARKER);
    envelope.extend_from_slice(&header_b91);
    envelope.push(SEPARATOR);
    envelope.extend_from_slice(&base91_encode(&chunk.payload));
    envelope
}

/// Parses one envelope (the raw APC bytes between `apc_hook` and
/// `apc_unhook`) into a [`Chunk`]. Validates the marker, header encoding,
/// version, content type, length, and checksum in that order — an error
/// variant tells the caller exactly which check failed rather than a
/// single generic "malformed" result, since a checksum failure (transient
/// corruption, safe to just drop this chunk and wait for a retransmit at
/// a higher layer) is a very different situation from a wrong marker
/// (this APC string isn't even meant for this parser at all).
pub fn parse_envelope(bytes: &[u8]) -> Result<Chunk, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::TooShort);
    }
    if bytes[0] != MARKER {
        return Err(DecodeError::WrongMarker);
    }

    let separator_pos = bytes[1..].iter().position(|&b| b == SEPARATOR).ok_or(DecodeError::TooShort)?;
    let header_b91 = &bytes[1..1 + separator_pos];
    let payload_b91 = &bytes[1 + separator_pos + 1..];

    let fields = base91_decode(header_b91).ok_or(DecodeError::InvalidHeaderEncoding)?;
    if fields.len() != HEADER_FIELDS_LEN {
        return Err(DecodeError::InvalidHeaderEncoding);
    }

    if fields[0] != VERSION {
        return Err(DecodeError::UnsupportedVersion(fields[0]));
    }
    let content_type = ContentType::from_u8(fields[1]).ok_or(DecodeError::UnknownContentType(fields[1]))?;

    let mut offset = 2;
    let session_id = u32::from_le_bytes(fields[offset..offset + 4].try_into().unwrap());
    offset += 4;
    let file_id = u32::from_le_bytes(fields[offset..offset + 4].try_into().unwrap());
    offset += 4;
    let chunk_offset = u64::from_le_bytes(fields[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let chunk_len = u32::from_le_bytes(fields[offset..offset + 4].try_into().unwrap());
    offset += 4;
    let total_size = u64::from_le_bytes(fields[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let metadata = decode_metadata(&fields[offset..offset + METADATA_ENCODED_LEN]).ok_or(DecodeError::InvalidMetadata)?;
    offset += METADATA_ENCODED_LEN;
    let declared_crc32 = u32::from_le_bytes(fields[offset..offset + 4].try_into().unwrap());
    offset += 4;
    debug_assert_eq!(offset, HEADER_FIELDS_LEN);

    let payload = base91_decode(payload_b91).ok_or(DecodeError::InvalidPayloadEncoding)?;
    if payload.len() != chunk_len as usize {
        return Err(DecodeError::LengthMismatch { declared: chunk_len, actual: payload.len() });
    }

    let actual_crc32 = crc32(&payload);
    if actual_crc32 != declared_crc32 {
        return Err(DecodeError::ChecksumMismatch { expected: declared_crc32, actual: actual_crc32 });
    }

    Ok(Chunk { content_type, session_id, file_id, chunk_offset, total_size, metadata, payload })
}

/// Splits `data` into payload-sized pieces no larger than `max_len` raw
/// bytes each — plain fixed-size chunking, since base91 encoding (unlike
/// the earlier raw-binary design) has no byte value it needs to avoid
/// splitting around. A thin wrapper kept mainly so callers (see
/// `crates/somcat`) don't need to hand-roll `chunks(max_len)` themselves
/// and so this module stays the single place that defines "how a source
/// file becomes a sequence of chunks."
pub fn split_into_chunks(data: &[u8], max_len: usize) -> Vec<Vec<u8>> {
    assert!(max_len > 0, "max_len must be positive");
    data.chunks(max_len).map(|c| c.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_chunk(payload: Vec<u8>) -> Chunk {
        Chunk {
            content_type: ContentType::Gif,
            session_id: 0xDEAD_BEEF,
            file_id: 0x1234_5678,
            chunk_offset: 4096,
            total_size: 500_000,
            metadata: ContentMetadata::Image { width_px: 480, height_px: 480, color_bits: 32, is_animated: true },
            payload,
        }
    }

    #[test]
    fn crc32_matches_known_vector() {
        // "123456789" is the standard CRC32 (IEEE 802.3) test vector —
        // any correct implementation must produce 0xCBF43926 for it.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn base91_alphabet_has_91_unique_printable_ascii_bytes_excluding_quotes_and_backslash() {
        assert_eq!(BASE91_ALPHABET.len(), 91);
        let mut seen = std::collections::HashSet::new();
        for &b in &BASE91_ALPHABET {
            assert!((0x21..=0x7E).contains(&b), "byte {b:#04x} outside printable ASCII");
            assert_ne!(b, b'"');
            assert_ne!(b, 0x27, "must not contain '\\''");
            assert_ne!(b, b'\\');
            assert!(seen.insert(b), "duplicate byte {b:#04x} in alphabet");
        }
    }

    #[test]
    fn base91_roundtrip_with_every_byte_value() {
        let data: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
        let encoded = base91_encode(&data);
        for &b in &encoded {
            assert!((0x21..=0x7E).contains(&b) && b != b'"' && b != 0x27 && b != b'\\');
        }
        let decoded = base91_decode(&encoded).expect("well-formed base91 must decode");
        assert_eq!(decoded, data);
    }

    #[test]
    fn base91_roundtrip_with_empty_input() {
        let encoded = base91_encode(&[]);
        assert!(encoded.is_empty());
        assert_eq!(base91_decode(&encoded), Some(Vec::new()));
    }

    #[test]
    fn base91_roundtrip_at_various_lengths() {
        // Different lengths exercise different tail-flush paths in the
        // bit-packing loop (the exact bit count left over at the end of
        // input varies with length in a way that isn't a simple
        // modulus — easier to just brute-force small lengths than to
        // reason the boundary cases out by hand).
        for len in 0..64 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 7) as u8).collect();
            let encoded = base91_encode(&data);
            let decoded = base91_decode(&encoded).unwrap_or_else(|| panic!("len={len} failed to decode"));
            assert_eq!(decoded, data, "roundtrip mismatch at len={len}");
        }
    }

    #[test]
    fn base91_decode_rejects_byte_outside_alphabet() {
        // Space (0x20) is adjacent to the alphabet's lower bound (0x21)
        // but deliberately excluded.
        assert_eq!(base91_decode(b" "), None);
        // A high byte can never appear in well-formed base91 output.
        assert_eq!(base91_decode(&[0xFF]), None);
    }

    #[test]
    fn envelope_contains_exactly_one_separator_right_after_the_header() {
        let chunk = sample_chunk(b"payload data".to_vec());
        let envelope = build_envelope(&chunk);
        // Exactly one SEPARATOR byte, since base91 output never produces
        // one itself (0x20 sits below the alphabet's lower bound) — this
        // is what lets `parse_envelope` find the header/payload boundary
        // unambiguously.
        let separator_count = envelope.iter().filter(|&&b| b == SEPARATOR).count();
        assert_eq!(separator_count, 1, "envelope must contain exactly one SEPARATOR byte");
    }

    #[test]
    fn envelope_roundtrip_preserves_all_fields() {
        let chunk = sample_chunk(b"some GIF bytes right here".to_vec());
        let envelope = build_envelope(&chunk);
        let parsed = parse_envelope(&envelope).expect("well-formed envelope must parse");
        assert_eq!(parsed, chunk);
    }

    #[test]
    fn envelope_roundtrip_with_empty_payload() {
        let chunk = sample_chunk(Vec::new());
        let envelope = build_envelope(&chunk);
        let parsed = parse_envelope(&envelope).expect("empty payload is a valid chunk");
        assert_eq!(parsed, chunk);
    }

    #[test]
    fn envelope_roundtrip_with_every_possible_byte_value() {
        // The whole point of base91-encoding payload: there is no longer
        // any byte value this protocol needs to forbid or special-case.
        let payload: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
        let chunk = sample_chunk(payload);
        let envelope = build_envelope(&chunk);
        let parsed = parse_envelope(&envelope).expect("full byte range payload must round-trip");
        assert_eq!(parsed, chunk);
    }

    #[test]
    fn envelope_wire_bytes_never_leave_the_base91_alphabet_marker_or_separator() {
        // Direct proof that a real APC-string scanner (like
        // `Terminal::write_output`'s boundary detector, or the VTE parser
        // itself) will never see a byte that could be confused with an
        // `ESC` (0x1B) or `LF` (0x0A) anywhere in this envelope, payload
        // included — the failure mode that motivated this whole redesign.
        let payload: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
        let chunk = sample_chunk(payload);
        let envelope = build_envelope(&chunk);
        assert_eq!(envelope[0], MARKER);
        for &b in &envelope[1..] {
            assert!(
                b == SEPARATOR || ((0x21..=0x7E).contains(&b) && b != b'"' && b != 0x27 && b != b'\\'),
                "byte {b:#04x} outside the safe base91 alphabet and not the separator"
            );
            assert_ne!(b, 0x1B);
            assert_ne!(b, 0x0A);
        }
    }

    #[test]
    fn too_short_is_rejected_before_indexing_panics() {
        assert_eq!(parse_envelope(&[MARKER]), Err(DecodeError::TooShort));
        assert_eq!(parse_envelope(&[]), Err(DecodeError::TooShort));
    }

    #[test]
    fn wrong_marker_is_rejected() {
        let chunk = sample_chunk(b"data".to_vec());
        let mut envelope = build_envelope(&chunk);
        envelope[0] = b'G'; // Kitty's marker, not ours.
        assert_eq!(parse_envelope(&envelope), Err(DecodeError::WrongMarker));
    }

    /// Overwrites one decoded header field (by its index into the
    /// `fields` buffer [`build_envelope`] constructs) with a new raw byte
    /// value, re-encodes the header, and splices it back into the
    /// envelope — tests can no longer poke a raw byte at a fixed offset
    /// (the header is base91-encoded on the wire), so this goes through
    /// the same encode/decode path the real header does.
    fn patch_header_field(chunk: &Chunk, field_index: usize, new_value: u8) -> Vec<u8> {
        let mut fields = vec![VERSION, chunk.content_type as u8];
        fields.extend_from_slice(&chunk.session_id.to_le_bytes());
        fields.extend_from_slice(&chunk.file_id.to_le_bytes());
        fields.extend_from_slice(&chunk.chunk_offset.to_le_bytes());
        fields.extend_from_slice(&(chunk.payload.len() as u32).to_le_bytes());
        fields.extend_from_slice(&chunk.total_size.to_le_bytes());
        fields.extend_from_slice(&encode_metadata(chunk.metadata));
        fields.extend_from_slice(&crc32(&chunk.payload).to_le_bytes());
        fields[field_index] = new_value;

        let mut envelope = Vec::new();
        envelope.push(MARKER);
        envelope.extend_from_slice(&base91_encode(&fields));
        envelope.push(SEPARATOR);
        envelope.extend_from_slice(&base91_encode(&chunk.payload));
        envelope
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let chunk = sample_chunk(b"data".to_vec());
        let envelope = patch_header_field(&chunk, 0, 99); // fields[0] = version
        assert_eq!(parse_envelope(&envelope), Err(DecodeError::UnsupportedVersion(99)));
    }

    #[test]
    fn unknown_content_type_is_rejected() {
        let chunk = sample_chunk(b"data".to_vec());
        let envelope = patch_header_field(&chunk, 1, 200); // fields[1] = content_type
        assert_eq!(parse_envelope(&envelope), Err(DecodeError::UnknownContentType(200)));
    }

    #[test]
    fn truncated_payload_is_rejected_as_length_mismatch() {
        let chunk = sample_chunk(b"0123456789".to_vec());
        let mut envelope = build_envelope(&chunk);
        // Chop the last symbol off the base91-encoded payload — chunk_len
        // in the header still claims the original (longer) length, and
        // the shortened encoding decodes to fewer raw bytes.
        envelope.pop();
        match parse_envelope(&envelope) {
            Err(DecodeError::LengthMismatch { declared, .. }) => assert_eq!(declared, 10),
            other => panic!("expected LengthMismatch, got {other:?}"),
        }
    }

    #[test]
    fn corrupted_payload_byte_is_rejected_as_checksum_mismatch() {
        let chunk = sample_chunk(b"0123456789".to_vec());
        let mut envelope = build_envelope(&chunk);
        // Swap the last two base91 symbols of the payload for two OTHER
        // alphabet symbols — same encoded length (so LengthMismatch
        // doesn't fire first), different decoded bytes, exercising the
        // checksum check specifically.
        let last = envelope.len() - 1;
        let replacement = if envelope[last] == BASE91_ALPHABET[0] { BASE91_ALPHABET[1] } else { BASE91_ALPHABET[0] };
        envelope[last] = replacement;
        assert!(matches!(parse_envelope(&envelope), Err(DecodeError::ChecksumMismatch { .. })));
    }

    #[test]
    fn invalid_header_encoding_is_rejected() {
        let chunk = sample_chunk(b"data".to_vec());
        let mut envelope = build_envelope(&chunk);
        // Byte 1 is the first symbol of the base91-encoded header —
        // replacing it with a byte outside BASE91_ALPHABET (but not
        // SEPARATOR itself, which would just move the header/payload
        // boundary instead of corrupting the header's encoding) breaks
        // decoding.
        envelope[1] = 0x00;
        assert_eq!(parse_envelope(&envelope), Err(DecodeError::InvalidHeaderEncoding));
    }

    #[test]
    fn invalid_payload_encoding_is_rejected() {
        let chunk = sample_chunk(b"data".to_vec());
        let mut envelope = build_envelope(&chunk);
        let last = envelope.len() - 1;
        envelope[last] = 0x00; // Not in BASE91_ALPHABET, and not SEPARATOR.
        assert_eq!(parse_envelope(&envelope), Err(DecodeError::InvalidPayloadEncoding));
    }

    #[test]
    fn all_content_type_variants_round_trip() {
        for content_type in [ContentType::Gif, ContentType::Audio, ContentType::Markdown, ContentType::Video] {
            let mut chunk = sample_chunk(b"x".to_vec());
            chunk.content_type = content_type;
            let envelope = build_envelope(&chunk);
            let parsed = parse_envelope(&envelope).expect("every declared ContentType variant must round-trip");
            assert_eq!(parsed.content_type, content_type);
        }
    }

    #[test]
    fn zero_total_size_means_unknown_not_empty_file() {
        let mut chunk = sample_chunk(b"data".to_vec());
        chunk.total_size = 0;
        let envelope = build_envelope(&chunk);
        let parsed = parse_envelope(&envelope).unwrap();
        assert_eq!(parsed.total_size, 0);
        assert!(!parsed.payload.is_empty(), "total_size=0 must not be conflated with an empty payload");
    }

    #[test]
    fn split_into_chunks_respects_max_len() {
        let data = vec![b'x'; 10];
        let pieces = split_into_chunks(&data, 3);
        assert_eq!(pieces.len(), 4); // 3 + 3 + 3 + 1
        for piece in &pieces {
            assert!(piece.len() <= 3);
        }
        let reassembled: Vec<u8> = pieces.into_iter().flatten().collect();
        assert_eq!(reassembled, data);
    }

    #[test]
    fn split_into_chunks_of_empty_data_produces_no_pieces() {
        assert_eq!(split_into_chunks(&[], 4096), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn split_into_chunks_with_data_smaller_than_max_len_produces_one_piece() {
        let data = b"short".to_vec();
        let pieces = split_into_chunks(&data, 4096);
        assert_eq!(pieces, vec![data]);
    }

    #[test]
    fn split_into_chunks_output_all_round_trip_through_build_and_parse_envelope() {
        // Property-style check across the full 0x00-0xFF byte range,
        // split small enough to produce several chunks — every piece
        // must build and parse back to the exact same bytes, with no
        // byte value requiring special handling anymore.
        let data: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
        let pieces = split_into_chunks(&data, 7);
        let mut reassembled = Vec::new();
        for piece in &pieces {
            let chunk = Chunk {
                content_type: ContentType::Gif,
                session_id: 1,
                file_id: 1,
                chunk_offset: 0,
                total_size: 0,
                metadata: ContentMetadata::Image { width_px: 0, height_px: 0, color_bits: 0, is_animated: false },
                payload: piece.clone(),
            };
            let envelope = build_envelope(&chunk);
            let parsed = parse_envelope(&envelope).unwrap();
            assert_eq!(parsed.payload, *piece);
            reassembled.extend_from_slice(&parsed.payload);
        }
        assert_eq!(reassembled, data);
    }
}
