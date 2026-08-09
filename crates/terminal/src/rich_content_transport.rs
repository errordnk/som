//! Wire format for Som's own binary rich-content protocol — completely
//! independent of the Kitty terminal graphics protocol
//! ([`crate::kitty_graphics`]), which stays untouched for compatibility
//! with third-party Kitty clients (yazi and similar). This protocol has
//! its own client, its own APC marker byte, and its own viewer; the two
//! only ever share the same physical PTY channel.
//!
//! Model: progressive file copy. A client (e.g. `somcat --stream`) reads a
//! real media file (GIF today; audio/markdown/video later, distinguished
//! by [`ContentType`]) and sends it chunk by chunk, unmodified — no
//! per-frame re-encoding, no base64. The receiving side writes each chunk
//! to a local cache file and can start decoding/playing before the whole
//! file has arrived.
//!
//! # Why raw binary, not base64
//!
//! Kitty's protocol base64-encodes its payload because the VTE parser
//! historically only passed 7-bit-safe bytes through an APC string (see
//! `errordnk/vte`'s original `advance_apc_string`). That fork has since
//! been patched (see its own doc comment) to pass every byte except `ESC`
//! (0x1B, the start of the `ESC \` terminator) straight through — a change
//! made specifically for this protocol. Kitty's own APC traffic is
//! unaffected (it never emits any of the newly-unlocked byte values), so
//! there was no reason to inherit its 7-bit-safe encoding for a protocol
//! that isn't Kitty and answers to no external spec.
//!
//! # Why `payload` can never contain `0x1B`, and no escaping scheme fixes that
//!
//! The patched VTE parser (see `advance_apc_string`'s own doc comment)
//! only knows one thing about `0x1B`: it ends the APC string, full stop —
//! it has no concept of "this ESC is escaped, keep going" the way, say, a
//! doubled-byte or COBS scheme would need it to. An APPLICATION-level
//! escaping scheme (double every `0x1B` in the payload before sending)
//! looks correct on paper but silently doesn't work: the parser hits the
//! FIRST `0x1B` of any such pair and terminates the string right there,
//! before this module's own `parse_envelope` ever gets a chance to
//! un-escape anything (confirmed the hard way — see
//! `test_rich_content_chunk_reaches_terminal_via_real_vte_parser` in
//! `terminal.rs`, which caught this exact bug: it passed against
//! `parse_envelope` called directly in a unit test, then failed the first
//! time it went through the real parser). Patching VTE a second time to
//! understand escaped pairs was considered and rejected — it would make
//! the parser aware of an application-level detail (this protocol's own
//! escaping convention) that has no business living in a general-purpose
//! terminal state machine.
//!
//! Instead: `payload` is a hard invariant — it must never contain `0x1B`
//! at all, not even escaped. [`build_envelope`] enforces this by
//! panicking (a payload with an embedded `0x1B` is a caller bug, not a
//! runtime condition to recover from — the ONE legitimate way for a
//! source file's `0x1B` byte to reach the wire is the `trailing_esc_byte`
//! flag below, which every real caller in this codebase — see
//! `crates/somcat`'s streaming client — is expected to use). A sender
//! that encounters `0x1B` while filling a chunk stops the chunk right
//! before it and sets `trailing_esc_byte: true` on THAT chunk instead —
//! the receiver appends exactly one literal `0x1B` byte to the cache file
//! right after `payload` when it sees the flag set, then the NEXT chunk
//! resumes normally at `chunk_offset + payload.len() + 1`. This adds at
//! most one extra small chunk per `0x1B` byte in the source file (roughly
//! 1-in-256 bytes for arbitrary binary media) — no per-byte doubling
//! overhead across the rest of the payload, and zero parser changes.
//!
//! # Envelope layout
//!
//! ```text
//! [marker:            1 byte  = b'S']
//! [version:           1 byte  = 1]
//! [content_type:      1 byte  = ContentType as u8]
//! [trailing_esc_byte: 1 byte  = 0 or 1]
//! [session_id:        4 bytes LE]
//! [file_id:           4 bytes LE]
//! [chunk_offset:      8 bytes LE]
//! [chunk_len:         4 bytes LE]
//! [total_size:        8 bytes LE, 0 = unknown]
//! [crc32:             4 bytes LE, over payload only]
//! [payload:           chunk_len bytes, MUST NOT contain 0x1B]
//! ```
//!
//! Sent as `ESC _ S <envelope> ESC \` — the leading `S` (Som) is what a
//! receiver checks first to route bytes here instead of to
//! [`crate::kitty_graphics::parse_command`] (which requires a leading `G`
//! and returns `None` otherwise, so the two parsers are mutually exclusive
//! by construction, not by convention). No escaping layer wraps the
//! envelope — see the section above for why one doesn't exist here.

use std::fmt;

/// Leading byte inside the APC string (right after `ESC _`) that marks
/// this as Som's own protocol, not Kitty's (`G`) or anything else.
pub const MARKER: u8 = b'S';

/// Wire format version — bumped if the envelope layout below ever needs a
/// breaking change. Only version 1 exists today; a receiver seeing any
/// other value should reject the envelope rather than guess.
pub const VERSION: u8 = 1;

/// Fixed envelope header size in bytes, before the payload — see this
/// module's doc comment for the exact field layout.
const HEADER_LEN: usize = 1 + 1 + 1 + 1 + 4 + 4 + 8 + 4 + 8 + 4;

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
}

impl ContentType {
    fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Gif),
            1 => Some(Self::Audio),
            2 => Some(Self::Markdown),
            3 => Some(Self::Video),
            _ => None,
        }
    }
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
    /// MUST NOT contain `0x1B` or `0x0A` — see this module's doc comment
    /// for why and what to do instead ([`Self::trailing_byte`]).
    /// [`build_envelope`] panics if this invariant is violated.
    pub payload: Vec<u8>,
    /// When [`Some`], exactly one literal byte of that value belongs in
    /// the cache file immediately after `payload` (i.e. at file offset
    /// `chunk_offset + payload.len()`) — the sender's way of transmitting
    /// a source `0x1B`/`0x0A` byte without ever putting it inside
    /// `payload` itself. See this module's doc comment for the full
    /// rationale.
    pub trailing_byte: Option<TrailingByte>,
}

/// The two byte values that can never appear inside [`Chunk::payload`] —
/// see this module's doc comment ("Why `payload` can never contain
/// certain bytes") for why each one is dangerous, and why one shared
/// mechanism ([`Chunk::trailing_byte`]) handles both instead of two
/// separate boolean flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrailingByte {
    /// `0x1B` (ESC) — the only byte the patched VTE parser still treats
    /// specially inside an APC string (it's the terminator's first byte).
    Esc = 0x1B,
    /// `0x0A` (LF) — Windows ConPTY inserts an extra `0x0D` (CR)
    /// immediately before every bare `0x0A` byte in a child process's
    /// output BEFORE that data ever reaches this process — confirmed via
    /// `alacritty_terminal`'s own `EventLoop::collapse_cr_cr_lf` doc
    /// comment, which documents this exact ConPTY behavior for an
    /// unrelated reason (SSH double-pty CRLF duplication). This is
    /// operating-system-level behavior outside this protocol's control —
    /// no amount of care in `somcat`'s own write path can prevent it, so
    /// `0x0A` gets the same trailing-byte treatment as `0x1B` rather than
    /// being allowed to travel inside `payload` and arrive corrupted.
    Lf = 0x0A,
}

impl TrailingByte {
    fn from_marker(byte: u8) -> Option<Self> {
        match byte {
            0 => None,
            0x1B => Some(Self::Esc),
            0x0A => Some(Self::Lf),
            other => unreachable!("invalid trailing_byte marker {other} — parse_envelope should have rejected this"),
        }
    }

    fn to_marker(this: Option<Self>) -> u8 {
        match this {
            None => 0,
            Some(Self::Esc) => 0x1B,
            Some(Self::Lf) => 0x0A,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than [`HEADER_LEN`] — not even a complete header.
    TooShort,
    /// Leading byte wasn't [`MARKER`] — not this protocol's envelope at
    /// all (could be Kitty's `G`, or garbage).
    WrongMarker,
    /// `version` field didn't match [`VERSION`].
    UnsupportedVersion(u8),
    /// `content_type` byte didn't match any [`ContentType`] variant.
    UnknownContentType(u8),
    /// `trailing_byte` marker byte wasn't `0`, `0x1B`, or `0x0A` — see
    /// [`TrailingByte`].
    UnknownTrailingByte(u8),
    /// `chunk_len` in the header didn't match the actual remaining byte
    /// count after the header — a truncated or corrupted envelope.
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
            Self::UnsupportedVersion(v) => write!(f, "unsupported envelope version {v}"),
            Self::UnknownContentType(b) => write!(f, "unknown content type byte {b}"),
            Self::UnknownTrailingByte(b) => write!(f, "unknown trailing_byte marker {b:#04x}"),
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

/// Builds one complete envelope ready to be wrapped in `ESC _ S ... ESC \`
/// and written to a PTY.
///
/// # Panics
///
/// If `chunk.payload` contains a `0x1B` or `0x0A` byte — see this
/// module's doc comment for why that's a hard protocol invariant rather
/// than something this function could silently work around, and what a
/// caller should do instead (split the chunk before the dangerous byte
/// and set `trailing_byte` on it).
pub fn build_envelope(chunk: &Chunk) -> Vec<u8> {
    assert!(
        !chunk.payload.contains(&0x1B) && !chunk.payload.contains(&0x0A),
        "rich_content_transport::Chunk::payload must never contain 0x1B or 0x0A — split the chunk \
         before the dangerous byte and set trailing_byte instead (see this module's doc comment)"
    );

    let mut envelope = Vec::with_capacity(HEADER_LEN + chunk.payload.len());
    envelope.push(MARKER);
    envelope.push(VERSION);
    envelope.push(chunk.content_type as u8);
    envelope.push(TrailingByte::to_marker(chunk.trailing_byte));
    envelope.extend_from_slice(&chunk.session_id.to_le_bytes());
    envelope.extend_from_slice(&chunk.file_id.to_le_bytes());
    envelope.extend_from_slice(&chunk.chunk_offset.to_le_bytes());
    envelope.extend_from_slice(&(chunk.payload.len() as u32).to_le_bytes());
    envelope.extend_from_slice(&chunk.total_size.to_le_bytes());
    envelope.extend_from_slice(&crc32(&chunk.payload).to_le_bytes());
    envelope.extend_from_slice(&chunk.payload);
    envelope
}

/// Parses one envelope (the raw APC bytes between `apc_hook` and
/// `apc_unhook`) into a [`Chunk`]. Validates the marker, version, content
/// type, length, and checksum in that order — an error variant tells the
/// caller exactly which check failed rather than a single generic
/// "malformed" result, since a checksum failure (transient corruption,
/// safe to just drop this chunk and wait for a retransmit at a higher
/// layer) is a very different situation from a wrong marker (this APC
/// string isn't even meant for this parser at all).
pub fn parse_envelope(bytes: &[u8]) -> Result<Chunk, DecodeError> {
    if bytes.len() < HEADER_LEN {
        return Err(DecodeError::TooShort);
    }
    if bytes[0] != MARKER {
        return Err(DecodeError::WrongMarker);
    }
    if bytes[1] != VERSION {
        return Err(DecodeError::UnsupportedVersion(bytes[1]));
    }
    let content_type = ContentType::from_u8(bytes[2]).ok_or(DecodeError::UnknownContentType(bytes[2]))?;
    let trailing_byte = match bytes[3] {
        0 | 0x1B | 0x0A => TrailingByte::from_marker(bytes[3]),
        other => return Err(DecodeError::UnknownTrailingByte(other)),
    };

    let mut offset = 4;
    let session_id = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    offset += 4;
    let file_id = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    offset += 4;
    let chunk_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let chunk_len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    offset += 4;
    let total_size = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let declared_crc32 = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    offset += 4;

    debug_assert_eq!(offset, HEADER_LEN);
    let payload = &bytes[HEADER_LEN..];
    if payload.len() != chunk_len as usize {
        return Err(DecodeError::LengthMismatch { declared: chunk_len, actual: payload.len() });
    }

    let actual_crc32 = crc32(payload);
    if actual_crc32 != declared_crc32 {
        return Err(DecodeError::ChecksumMismatch { expected: declared_crc32, actual: actual_crc32 });
    }

    Ok(Chunk {
        content_type,
        session_id,
        file_id,
        chunk_offset,
        total_size,
        payload: payload.to_vec(),
        trailing_byte,
    })
}

/// Splits `data` into a sequence of `(payload, trailing_byte)` pieces
/// suitable for building one [`Chunk`] each, guaranteeing every resulting
/// `payload` slice is free of BOTH dangerous bytes (`0x1B` and `0x0A` —
/// satisfying [`build_envelope`]'s hard invariant) and no longer than
/// `max_len`. A run of consecutive dangerous bytes in `data` produces one
/// zero-length-payload, `trailing_byte: Some(_)` piece per dangerous byte
/// — correct, if not maximally compact, and simple enough that a client
/// (see `crates/somcat`) doesn't need to duplicate this splitting logic
/// itself.
pub fn split_into_chunks(data: &[u8], max_len: usize) -> Vec<(Vec<u8>, Option<TrailingByte>)> {
    assert!(max_len > 0, "max_len must be positive");
    let mut pieces = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        match rest.iter().position(|&b| b == 0x1B || b == 0x0A) {
            Some(0) => {
                let marker = TrailingByte::from_marker(rest[0]);
                pieces.push((Vec::new(), marker));
                rest = &rest[1..];
            },
            Some(dangerous_pos) => {
                let take = dangerous_pos.min(max_len);
                let hit_dangerous_byte = take == dangerous_pos && take < rest.len();
                let marker = if hit_dangerous_byte { TrailingByte::from_marker(rest[take]) } else { None };
                pieces.push((rest[..take].to_vec(), marker));
                rest = &rest[take + if hit_dangerous_byte { 1 } else { 0 }..];
            },
            None => {
                let take = rest.len().min(max_len);
                pieces.push((rest[..take].to_vec(), None));
                rest = &rest[take..];
            },
        }
    }
    pieces
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
            payload,
            trailing_esc_byte: false,
        }
    }

    #[test]
    fn crc32_matches_known_vector() {
        // "123456789" is the standard CRC32 (IEEE 802.3) test vector —
        // any correct implementation must produce 0xCBF43926 for it.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn envelope_roundtrip_preserves_all_fields() {
        let chunk = sample_chunk(b"some GIF bytes right here".to_vec());
        let envelope = build_envelope(&chunk);
        let parsed = parse_envelope(&envelope).expect("well-formed envelope must parse");
        assert_eq!(parsed, chunk);
    }

    #[test]
    fn envelope_roundtrip_with_trailing_esc_byte_flag_set() {
        let mut chunk = sample_chunk(b"data right before an ESC byte".to_vec());
        chunk.trailing_esc_byte = true;
        let envelope = build_envelope(&chunk);
        let parsed = parse_envelope(&envelope).expect("trailing_esc_byte flag must round-trip");
        assert!(parsed.trailing_esc_byte);
        assert_eq!(parsed.payload, chunk.payload);
    }

    #[test]
    #[should_panic(expected = "must never contain 0x1B")]
    fn build_envelope_panics_on_payload_containing_esc() {
        let chunk = sample_chunk(vec![b'a', 0x1B, b'b']);
        build_envelope(&chunk);
    }

    #[test]
    fn envelope_roundtrip_with_empty_payload() {
        let chunk = sample_chunk(Vec::new());
        let envelope = build_envelope(&chunk);
        let parsed = parse_envelope(&envelope).expect("empty payload is a valid chunk");
        assert_eq!(parsed, chunk);
    }

    #[test]
    fn envelope_roundtrip_with_every_non_esc_byte_value() {
        // Every possible byte value except 0x1B — proving the whole
        // 8-bit range (minus the one reserved byte) this protocol exists
        // to unlock survives the envelope round-trip intact.
        let payload: Vec<u8> = (0u8..=255).filter(|&b| b != 0x1B).collect();
        let chunk = sample_chunk(payload);
        let envelope = build_envelope(&chunk);
        let parsed = parse_envelope(&envelope).expect("full non-ESC byte range payload must round-trip");
        assert_eq!(parsed, chunk);
    }

    #[test]
    fn too_short_is_rejected_before_indexing_panics() {
        assert_eq!(parse_envelope(&[MARKER, VERSION]), Err(DecodeError::TooShort));
        assert_eq!(parse_envelope(&[]), Err(DecodeError::TooShort));
    }

    #[test]
    fn wrong_marker_is_rejected() {
        let chunk = sample_chunk(b"data".to_vec());
        let mut envelope = build_envelope(&chunk);
        envelope[0] = b'G'; // Kitty's marker, not ours.
        assert_eq!(parse_envelope(&envelope), Err(DecodeError::WrongMarker));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let chunk = sample_chunk(b"data".to_vec());
        let mut envelope = build_envelope(&chunk);
        envelope[1] = 99;
        assert_eq!(parse_envelope(&envelope), Err(DecodeError::UnsupportedVersion(99)));
    }

    #[test]
    fn unknown_content_type_is_rejected() {
        let chunk = sample_chunk(b"data".to_vec());
        let mut envelope = build_envelope(&chunk);
        envelope[2] = 200;
        assert_eq!(parse_envelope(&envelope), Err(DecodeError::UnknownContentType(200)));
    }

    #[test]
    fn truncated_payload_is_rejected_as_length_mismatch() {
        let chunk = sample_chunk(b"0123456789".to_vec());
        let mut envelope = build_envelope(&chunk);
        // Chop the last 3 payload bytes off — chunk_len in the header
        // still claims the original (longer) length.
        envelope.truncate(envelope.len() - 3);
        match parse_envelope(&envelope) {
            Err(DecodeError::LengthMismatch { declared, actual }) => {
                assert_eq!(declared, 10);
                assert_eq!(actual, 7);
            },
            other => panic!("expected LengthMismatch, got {other:?}"),
        }
    }

    #[test]
    fn corrupted_payload_byte_is_rejected_as_checksum_mismatch() {
        let chunk = sample_chunk(b"0123456789".to_vec());
        let mut envelope = build_envelope(&chunk);
        // Flip a bit somewhere inside the payload region (well past the
        // fixed header) without touching chunk_len, so this exercises the
        // checksum check specifically, not the length check above it.
        let last = envelope.len() - 1;
        envelope[last] ^= 0xFF;
        assert!(matches!(parse_envelope(&envelope), Err(DecodeError::ChecksumMismatch { .. })));
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
    fn split_into_chunks_with_no_esc_bytes_produces_one_piece_under_max_len() {
        let data = b"no escape bytes here at all".to_vec();
        let pieces = split_into_chunks(&data, 4096);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0], (data, false));
    }

    #[test]
    fn split_into_chunks_respects_max_len() {
        let data = vec![b'x'; 10];
        let pieces = split_into_chunks(&data, 3);
        assert_eq!(pieces.len(), 4); // 3 + 3 + 3 + 1
        for (payload, trailing) in &pieces {
            assert!(payload.len() <= 3);
            assert!(!trailing);
        }
        let reassembled: Vec<u8> = pieces.iter().flat_map(|(p, _)| p.clone()).collect();
        assert_eq!(reassembled, data);
    }

    #[test]
    fn split_into_chunks_splits_before_each_esc_byte() {
        let data = vec![b'a', b'b', 0x1B, b'c', b'd'];
        let pieces = split_into_chunks(&data, 4096);
        assert_eq!(pieces, vec![(vec![b'a', b'b'], true), (vec![b'c', b'd'], false)]);

        // Reassembling: payload + (literal 0x1B if trailing_esc_byte) for
        // each piece, in order, must equal the original bytes.
        let mut reassembled = Vec::new();
        for (payload, trailing) in &pieces {
            reassembled.extend_from_slice(payload);
            if *trailing {
                reassembled.push(0x1B);
            }
        }
        assert_eq!(reassembled, data);
    }

    #[test]
    fn split_into_chunks_handles_consecutive_esc_bytes() {
        let data = vec![b'a', 0x1B, 0x1B, b'b'];
        let pieces = split_into_chunks(&data, 4096);
        assert_eq!(pieces, vec![(vec![b'a'], true), (Vec::new(), true), (vec![b'b'], false)]);

        let mut reassembled = Vec::new();
        for (payload, trailing) in &pieces {
            reassembled.extend_from_slice(payload);
            if *trailing {
                reassembled.push(0x1B);
            }
        }
        assert_eq!(reassembled, data);
    }

    #[test]
    fn split_into_chunks_handles_esc_byte_at_the_very_start() {
        let data = vec![0x1B, b'a', b'b'];
        let pieces = split_into_chunks(&data, 4096);
        assert_eq!(pieces, vec![(Vec::new(), true), (vec![b'a', b'b'], false)]);
    }

    #[test]
    fn split_into_chunks_handles_esc_byte_at_the_very_end() {
        let data = vec![b'a', b'b', 0x1B];
        let pieces = split_into_chunks(&data, 4096);
        assert_eq!(pieces, vec![(vec![b'a', b'b'], true)]);
    }

    #[test]
    fn split_into_chunks_of_empty_data_produces_no_pieces() {
        assert_eq!(split_into_chunks(&[], 4096), Vec::<(Vec<u8>, bool)>::new());
    }

    #[test]
    fn split_into_chunks_output_never_contains_payload_with_esc_byte() {
        // Property-style check across a range of inputs deliberately
        // dense with ESC bytes — every produced payload must satisfy
        // build_envelope's hard invariant, proven here by actually
        // calling build_envelope/parse_envelope on each piece rather than
        // just asserting `!payload.contains(&0x1B)` directly (which would
        // only test this function, not that its output is truly usable
        // downstream).
        let data: Vec<u8> = (0u8..=255).collect(); // includes 0x1B once, at index 27
        let pieces = split_into_chunks(&data, 7);
        for (payload, trailing) in &pieces {
            let chunk = Chunk {
                content_type: ContentType::Gif,
                session_id: 1,
                file_id: 1,
                chunk_offset: 0,
                total_size: 0,
                payload: payload.clone(),
                trailing_esc_byte: *trailing,
            };
            let envelope = build_envelope(&chunk); // Must not panic.
            let parsed = parse_envelope(&envelope).unwrap();
            assert_eq!(parsed.payload, *payload);
        }
    }
}
