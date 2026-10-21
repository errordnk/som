//! The frames of the link, and how they travel inside a PTY byte stream.
//!
//! A frame is one escape sequence, `ESC _ SRV ; <base64> ESC \` (an APC
//! string: what a terminal that does not know it ignores). Text, because a
//! PTY is not 8-bit clean (line endings are rewritten, control bytes mean
//! things); the base64 alphabet has no byte that any terminal layer touches.
//!
//! The body, before base64: `seq (4) | whitened(channel (4) | flags (1) | data)
//! | crc32 (4)`.
//! - `seq` counts the frames of one direction. A frame handed on twice (the
//!   ConPTY duplicate-read bug) is dropped; one that is missing is a break
//!   in the stream and ends the link's current state (see `Decoder`).
//! - Whitening xors the rest with a stream seeded from `seq`: a payload of
//!   zeros (silence, a black picture) would otherwise be long runs of one
//!   character, which the terminal side's duplicate-read workaround would
//!   take for duplicated reads and eat.
//! - The crc32 covers `seq` and the whitened bytes.
//!
//! A message longer than [`PART_LEN`] travels as several frames of one channel
//! (`MORE` on all but the last), so one big transfer cannot hold up the
//! terminal's own output or another channel for long.

use base64::Engine as _;
use std::collections::HashMap;

const START: &[u8] = b"\x1b_SRV;";
const END: &[u8] = b"\x1b\\";

/// The most message bytes one frame carries.
pub const PART_LEN: usize = 12 * 1024;

/// A frame body longer than this (in base64 characters) is not a frame.
const MAX_BODY_LEN: usize = 64 * 1024;

/// A message is compressed when it is at least this long and that makes it
/// smaller. The daemon's messages carry bytes as JSON arrays of numbers,
/// which compress to little more than the bytes themselves.
const COMPRESS_FROM: usize = 512;

/// The most a reassembled message may be (a transfer chunk is a few MiB).
const MAX_MESSAGE_LEN: usize = 256 * 1024 * 1024;

const F_CLOSE: u8 = 1;
const F_MORE: u8 = 2;
const F_ZSTD: u8 = 4;
const F_HELLO: u8 = 8;
const F_RESET: u8 = 16;

/// What one end says to the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The relay is here and speaks the link. Nothing is ever sent to a relay
    /// that has not said so: an older one would type it into the shell.
    Hello,
    /// The sender dropped everything it knew about the link (it saw a break in
    /// the stream); the receiver does too.
    Reset,
    /// One message of one channel (a connection to the daemon).
    Data { channel: u32, bytes: Vec<u8> },
    /// The channel is closed.
    Close { channel: u32 },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Part {
    channel: u32,
    flags: u8,
    data: Vec<u8>,
}

/// Cuts a message into the frames' payloads.
pub fn split(message: &Message) -> Vec<Part> {
    match message {
        Message::Hello => vec![Part { channel: 0, flags: F_HELLO, data: Vec::new() }],
        Message::Reset => vec![Part { channel: 0, flags: F_RESET, data: Vec::new() }],
        Message::Close { channel } => vec![Part { channel: *channel, flags: F_CLOSE, data: Vec::new() }],
        Message::Data { channel, bytes } => {
            let mut flags = 0;
            let mut body = std::borrow::Cow::Borrowed(bytes.as_slice());
            if bytes.len() >= COMPRESS_FROM
                && let Ok(compressed) = zstd::bulk::compress(bytes, 1)
                && compressed.len() < bytes.len()
            {
                body = std::borrow::Cow::Owned(compressed);
                flags |= F_ZSTD;
            }
            let mut parts: Vec<Part> = body
                .chunks(PART_LEN)
                .map(|chunk| Part { channel: *channel, flags: flags | F_MORE, data: chunk.to_vec() })
                .collect();
            if parts.is_empty() {
                parts.push(Part { channel: *channel, flags, data: Vec::new() });
            }
            if let Some(last) = parts.last_mut() {
                last.flags &= !F_MORE;
            }
            parts
        },
    }
}

/// Numbers and seals the frames of one direction.
#[derive(Default)]
pub struct Encoder {
    next_seq: u32,
}

impl Encoder {
    /// The bytes to write for `part`: the whole escape sequence.
    pub fn seal(&mut self, part: &Part) -> Vec<u8> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);

        let mut body = Vec::with_capacity(4 + 5 + part.data.len() + 4);
        body.extend_from_slice(&seq.to_le_bytes());
        body.extend_from_slice(&part.channel.to_le_bytes());
        body.push(part.flags);
        body.extend_from_slice(&part.data);
        whiten(seq, &mut body[4..]);
        let crc = crc32(&body);
        body.extend_from_slice(&crc.to_le_bytes());

        let text = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&body);
        let mut frame = Vec::with_capacity(START.len() + text.len() + END.len());
        frame.extend_from_slice(START);
        frame.extend_from_slice(text.as_bytes());
        frame.extend_from_slice(END);
        frame
    }
}

fn whiten(seq: u32, bytes: &mut [u8]) {
    let mut state = (seq ^ 0x9E37_79B9) | 1;
    for byte in bytes {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte ^= state as u8;
    }
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 { 0xEDB8_8320 ^ (value >> 1) } else { value >> 1 };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc = CRC_TABLE[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// What is wrong with the stream.
#[derive(Debug, PartialEq, Eq)]
pub enum StreamBroken {
    /// A frame that does not read: damaged on the way, or not ours.
    Corrupt(&'static str),
    /// Frames are missing.
    Gap { expected: u32, got: u32 },
}

/// Reads the frames of one direction back into messages.
#[derive(Default)]
pub struct Decoder {
    last_seq: Option<u32>,
    partial: HashMap<u32, Vec<u8>>,
}

impl Decoder {
    /// The message a frame body (the base64 text between the markers)
    /// completes, if it completes one.
    pub fn decode(&mut self, body: &[u8]) -> Result<Option<Message>, StreamBroken> {
        let raw = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(body)
            .map_err(|_| StreamBroken::Corrupt("not base64"))?;
        if raw.len() < 4 + 5 + 4 {
            return Err(StreamBroken::Corrupt("too short"));
        }
        let (covered, crc_bytes) = raw.split_at(raw.len() - 4);
        if crc32(covered).to_le_bytes() != crc_bytes {
            return Err(StreamBroken::Corrupt("checksum"));
        }
        let seq = u32::from_le_bytes([covered[0], covered[1], covered[2], covered[3]]);
        let mut rest = covered[4..].to_vec();
        whiten(seq, &mut rest);
        let channel = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let flags = rest[4];
        let data = &rest[5..];

        if flags & F_RESET != 0 {
            // The one frame accepted wherever the count stands.
            self.last_seq = Some(seq);
            self.partial.clear();
            return Ok(Some(Message::Reset));
        }
        if let Some(last) = self.last_seq {
            let expected = last.wrapping_add(1);
            if seq == last {
                return Ok(None); // the same frame handed on twice
            }
            if seq != expected {
                return Err(StreamBroken::Gap { expected, got: seq });
            }
        }
        self.last_seq = Some(seq);

        if flags & F_HELLO != 0 {
            self.partial.clear();
            return Ok(Some(Message::Hello));
        }
        if flags & F_CLOSE != 0 {
            self.partial.remove(&channel);
            return Ok(Some(Message::Close { channel }));
        }
        let message = self.partial.entry(channel).or_default();
        if message.len() + data.len() > MAX_MESSAGE_LEN {
            self.partial.remove(&channel);
            return Err(StreamBroken::Corrupt("message too long"));
        }
        message.extend_from_slice(data);
        if flags & F_MORE != 0 {
            return Ok(None);
        }
        let mut bytes = self.partial.remove(&channel).unwrap_or_default();
        if flags & F_ZSTD != 0 {
            bytes = zstd::bulk::decompress(&bytes, MAX_MESSAGE_LEN).map_err(|_| StreamBroken::Corrupt("zstd"))?;
        }
        Ok(Some(Message::Data { channel, bytes }))
    }

    /// Forgets the stream's state: the next frame starts it anew.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

enum Scan {
    /// Plain stream bytes; `held` is how many bytes of [`START`] have been
    /// seen at the end of the last chunk (and are not yet passed on).
    Plain { held: usize },
    /// Inside a frame: its body so far.
    Body(Vec<u8>),
    /// Inside a frame, and the last byte was an ESC.
    BodyEsc(Vec<u8>),
}

/// Takes frames out of a byte stream, passing everything else on in order.
/// A chunk may end anywhere: a trailing piece that could be the start of a
/// frame is held back until the next chunk says what it is.
pub struct Extractor {
    scan: Scan,
}

impl Default for Extractor {
    fn default() -> Self {
        Self { scan: Scan::Plain { held: 0 } }
    }
}

impl Extractor {
    /// `passed` gets the bytes that are not frames, `bodies` the bodies of the
    /// frames found.
    pub fn feed(&mut self, input: &[u8], passed: &mut Vec<u8>, bodies: &mut Vec<Vec<u8>>) {
        for &byte in input {
            self.step(byte, passed, bodies);
        }
    }

    /// Passes on what is being held back as a possible start of a frame, for
    /// when nothing more has come to say what it is: a lone ESC is a key the
    /// user pressed, not the start of anything. Returns whether anything was
    /// held. Never touches a frame that has really begun.
    pub fn take_held(&mut self, passed: &mut Vec<u8>) -> bool {
        match self.scan {
            Scan::Plain { held } if held > 0 => {
                passed.extend_from_slice(&START[..held]);
                self.scan = Scan::Plain { held: 0 };
                true
            },
            _ => false,
        }
    }

    fn step(&mut self, byte: u8, passed: &mut Vec<u8>, bodies: &mut Vec<Vec<u8>>) {
        match std::mem::replace(&mut self.scan, Scan::Plain { held: 0 }) {
            Scan::Plain { held } => {
                if byte == START[held] {
                    if held + 1 == START.len() {
                        self.scan = Scan::Body(Vec::new());
                    } else {
                        self.scan = Scan::Plain { held: held + 1 };
                    }
                    return;
                }
                // Not the start of a frame after all: what was held is plain.
                passed.extend_from_slice(&START[..held]);
                if byte == START[0] {
                    self.scan = Scan::Plain { held: 1 };
                } else {
                    passed.push(byte);
                }
            },
            Scan::Body(mut body) => match byte {
                0x1b => self.scan = Scan::BodyEsc(body),
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' if body.len() < MAX_BODY_LEN => {
                    body.push(byte);
                    self.scan = Scan::Body(body);
                },
                // Anything else cannot be part of a frame: it was not one (what
                // was taken for its start is lost, this byte is the stream's).
                _ => self.step(byte, passed, bodies),
            },
            Scan::BodyEsc(body) => {
                if byte == END[1] {
                    bodies.push(body);
                } else {
                    // The ESC began something else (or damaged the frame).
                    self.step(byte, passed, bodies);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn through(message: &Message) -> Vec<Vec<u8>> {
        let mut encoder = Encoder::default();
        split(message).iter().map(|part| encoder.seal(part)).collect()
    }

    fn body_of(frame: &[u8]) -> &[u8] {
        &frame[START.len()..frame.len() - END.len()]
    }

    #[test]
    fn a_message_comes_back_as_it_went_including_big_and_empty_ones() {
        let big: Vec<u8> = (0..200_000u32).map(|n| (n * 7 % 251) as u8).collect();
        for message in [
            Message::Hello,
            Message::Reset,
            Message::Close { channel: 9 },
            Message::Data { channel: 3, bytes: b"x".to_vec() },
            Message::Data { channel: 3, bytes: Vec::new() },
            Message::Data { channel: 4, bytes: big },
            Message::Data { channel: 5, bytes: vec![0; 100_000] },
        ] {
            let mut decoder = Decoder::default();
            let frames = through(&message);
            let mut decoded = None;
            for frame in &frames {
                if let Some(done) = decoder.decode(body_of(frame)).unwrap() {
                    assert!(decoded.replace(done).is_none(), "one message, one result");
                }
            }
            assert_eq!(decoded.as_ref(), Some(&message), "{} frames", frames.len());
        }
    }

    #[test]
    fn json_shaped_data_is_compressed_and_a_frame_stays_within_a_pty_line_of_text() {
        let bytes: Vec<u8> = serde_json::to_vec(&(0..4000u32).map(|n| (n % 256) as u8).collect::<Vec<u8>>()).unwrap();
        let frames = through(&Message::Data { channel: 1, bytes: bytes.clone() });
        let total: usize = frames.iter().map(Vec::len).sum();
        assert!(total < bytes.len(), "{total} bytes sent for {} of JSON", bytes.len());
        for frame in &frames {
            assert!(frame.iter().all(|&byte| byte == 0x1b || byte.is_ascii_graphic() && byte != b'\\' || byte == b'\\'), "only printable text and the two ESC markers");
            assert!(!frame.contains(&b'\n') && !frame.contains(&b'\r') && !frame.contains(&0));
        }
    }

    #[test]
    fn identical_payloads_do_not_make_identical_text() {
        // The terminal side drops a read that repeats the one before it.
        let frames = through(&Message::Data { channel: 1, bytes: vec![0; 40_000] });
        let text: Vec<u8> = frames.concat();
        let window = 64;
        let mut seen = std::collections::HashSet::new();
        for chunk in text.chunks(window).filter(|chunk| chunk.len() == window) {
            assert!(seen.insert(chunk.to_vec()), "two equal {window}-byte pieces in the text of zeros");
        }
    }

    #[test]
    fn a_frame_handed_on_twice_is_dropped_and_a_missing_one_is_noticed() {
        let mut encoder = Encoder::default();
        let frames: Vec<Vec<u8>> = (1..=4).map(|channel| encoder.seal(&split(&Message::Close { channel })[0])).collect();
        let mut decoder = Decoder::default();
        assert_eq!(decoder.decode(body_of(&frames[0])).unwrap(), Some(Message::Close { channel: 1 }));
        assert_eq!(decoder.decode(body_of(&frames[0])).unwrap(), None, "the same frame again");
        assert_eq!(decoder.decode(body_of(&frames[1])).unwrap(), Some(Message::Close { channel: 2 }));
        // frames[2] went missing.
        assert_eq!(decoder.decode(body_of(&frames[3])), Err(StreamBroken::Gap { expected: 2, got: 3 }));
    }

    #[test]
    fn a_damaged_frame_is_noticed_and_a_reset_is_accepted_anywhere() {
        let mut encoder = Encoder::default();
        let frame = encoder.seal(&split(&Message::Data { channel: 1, bytes: b"hello".to_vec() })[0]);
        let mut damaged = body_of(&frame).to_vec();
        let middle = damaged.len() / 2;
        damaged[middle] = if damaged[middle] == b'A' { b'B' } else { b'A' };
        assert!(matches!(Decoder::default().decode(&damaged), Err(StreamBroken::Corrupt(_))));

        let mut decoder = Decoder::default();
        decoder.decode(body_of(&frame)).unwrap();
        for _ in 0..5 {
            encoder.seal(&split(&Message::Close { channel: 1 })[0]); // frames lost
        }
        let reset = encoder.seal(&split(&Message::Reset)[0]);
        assert_eq!(decoder.decode(body_of(&reset)).unwrap(), Some(Message::Reset));
        let next = encoder.seal(&split(&Message::Close { channel: 2 })[0]);
        assert_eq!(decoder.decode(body_of(&next)).unwrap(), Some(Message::Close { channel: 2 }));
    }

    fn extract_all(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut extractor = Extractor::default();
        let (mut passed, mut bodies) = (Vec::new(), Vec::new());
        for chunk in chunks {
            extractor.feed(chunk, &mut passed, &mut bodies);
        }
        (passed, bodies)
    }

    #[test]
    fn frames_come_out_of_a_stream_and_everything_else_goes_on_unchanged() {
        let mut encoder = Encoder::default();
        let frame = encoder.seal(&split(&Message::Data { channel: 7, bytes: b"payload".to_vec() })[0]);
        let mut stream = b"before \x1b[31mred\x1b[0m ".to_vec();
        stream.extend_from_slice(&frame);
        stream.extend_from_slice(b" after\r\n\x1b");
        stream.extend_from_slice(b"[2J");
        let (passed, bodies) = extract_all(&[&stream]);
        assert_eq!(passed, b"before \x1b[31mred\x1b[0m  after\r\n\x1b[2J");
        assert_eq!(bodies, vec![body_of(&frame).to_vec()]);
    }

    #[test]
    fn a_stream_cut_anywhere_gives_the_same_result() {
        let mut encoder = Encoder::default();
        let first = encoder.seal(&split(&Message::Data { channel: 1, bytes: b"one".to_vec() })[0]);
        let second = encoder.seal(&split(&Message::Close { channel: 1 })[0]);
        let mut stream = b"a\x1b[1mb".to_vec();
        stream.extend_from_slice(&first);
        stream.extend_from_slice(b"c\x1b_notours;x\x1b\\d\x1b");
        stream.extend_from_slice(&second);
        stream.extend_from_slice(b"e\x1b");
        let whole = extract_all(&[&stream]);
        for cut in 0..=stream.len() {
            let (head, tail) = stream.split_at(cut);
            assert_eq!(extract_all(&[head, tail]), whole, "cut at {cut}");
        }
        // Single bytes, the worst case.
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(extract_all(&bytes), whole);
        assert_eq!(whole.1.len(), 2);
    }

    #[test]
    fn an_escape_that_is_not_the_start_of_a_frame_is_passed_on_in_order() {
        let (passed, bodies) = extract_all(&[b"\x1b\x1b_SR\x1b_SRV;", b"AAAA!rest\x1b_S", b"x"]);
        assert!(bodies.is_empty());
        assert_eq!(passed, b"\x1b\x1b_SR!rest\x1b_Sx", "a damaged frame is lost, never turned into terminal input");
    }

    #[test]
    fn a_held_start_is_passed_on_when_nothing_follows_and_a_real_frame_is_left_alone() {
        let mut extractor = Extractor::default();
        let (mut passed, mut bodies) = (Vec::new(), Vec::new());
        extractor.feed(b"\x1b", &mut passed, &mut bodies);
        assert!(passed.is_empty(), "held back, it may be the start of a frame");
        assert!(extractor.take_held(&mut passed));
        assert_eq!(passed, b"\x1b", "a lone ESC is a key press");
        assert!(!extractor.take_held(&mut passed), "nothing left to give");

        // The stream goes on as if nothing had been held.
        extractor.feed(b"[A", &mut passed, &mut bodies);
        assert_eq!(passed, b"\x1b[A");

        // Inside a frame that has really begun there is nothing to give up.
        extractor.feed(b"\x1b_SRV;AAAA", &mut passed, &mut bodies);
        assert!(!extractor.take_held(&mut passed));
    }

    #[test]
    fn an_unterminated_frame_does_not_swallow_the_stream() {
        let mut stream = b"\x1b_SRV;".to_vec();
        stream.extend(std::iter::repeat_n(b'A', MAX_BODY_LEN + 10));
        stream.extend_from_slice(b" visible again");
        let (passed, _) = extract_all(&[&stream]);
        assert!(passed.ends_with(b" visible again"));
    }
}
