//! Tells whether a terminal byte stream is between escape sequences.
//!
//! The relay writes link frames into the same stream as the shell's own
//! output. A frame is taken out again by Som before the terminal parser sees
//! it, but the terminal layers in between (the ConPTY of a Windows Som) parse
//! the stream too, and a frame that lands in the middle of one of the shell's
//! escape sequences breaks that sequence for them. So frames are written only
//! at points where this says the stream is at rest.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    Esc,
    /// ESC followed by intermediate bytes (`ESC ( B`).
    EscIntermediate,
    Csi,
    /// A string (OSC, DCS, APC, PM, SOS) up to its terminator; the flag says
    /// whether BEL ends it (only OSC's does).
    String { bel_ends: bool },
    /// An ESC inside a string.
    StringEsc { bel_ends: bool },
}

#[derive(Debug)]
pub struct VtTracker {
    state: State,
    /// Continuation bytes of a UTF-8 character still to come.
    utf8_pending: u8,
}

impl Default for VtTracker {
    fn default() -> Self {
        Self { state: State::Ground, utf8_pending: 0 }
    }
}

impl VtTracker {
    pub fn is_ground(&self) -> bool {
        self.state == State::Ground && self.utf8_pending == 0
    }

    pub fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.step(byte);
        }
    }

    fn step(&mut self, byte: u8) {
        // CAN and SUB abandon whatever sequence is under way.
        if matches!(byte, 0x18 | 0x1a) {
            self.state = State::Ground;
            self.utf8_pending = 0;
            return;
        }
        self.state = match self.state {
            State::Ground => {
                self.utf8(byte);
                if byte == 0x1b { State::Esc } else { State::Ground }
            },
            State::Esc => match byte {
                b'[' => State::Csi,
                b']' => State::String { bel_ends: true },
                b'P' | b'_' | b'^' | b'X' => State::String { bel_ends: false },
                0x20..=0x2f => State::EscIntermediate,
                0x1b => State::Esc,
                0x00..=0x1f => State::Esc, // a control executed inside the sequence
                _ => State::Ground,
            },
            State::EscIntermediate => match byte {
                0x20..=0x2f => State::EscIntermediate,
                0x1b => State::Esc,
                0x30..=0x7e => State::Ground,
                _ => State::EscIntermediate,
            },
            State::Csi => match byte {
                0x40..=0x7e => State::Ground,
                0x1b => State::Esc,
                _ => State::Csi,
            },
            State::String { bel_ends } => match byte {
                0x07 if bel_ends => State::Ground,
                0x1b => State::StringEsc { bel_ends },
                _ => State::String { bel_ends },
            },
            State::StringEsc { bel_ends } => match byte {
                b'\\' => State::Ground,
                0x1b => State::StringEsc { bel_ends },
                // Anything else ends the string and starts a new sequence.
                b'[' => State::Csi,
                b']' => State::String { bel_ends: true },
                b'P' | b'_' | b'^' | b'X' => State::String { bel_ends: false },
                _ => State::Ground,
            },
        };
    }

    fn utf8(&mut self, byte: u8) {
        self.utf8_pending = match byte {
            0xc2..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf4 => 3,
            0x80..=0xbf => self.utf8_pending.saturating_sub(1),
            _ => 0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ground_after(bytes: &[u8]) -> bool {
        let mut tracker = VtTracker::default();
        tracker.observe(bytes);
        tracker.is_ground()
    }

    #[test]
    fn plain_text_and_finished_sequences_are_at_rest() {
        assert!(ground_after(b""));
        assert!(ground_after(b"hello\r\n"));
        assert!(ground_after(b"\x1b[31mred\x1b[0m"));
        assert!(ground_after(b"\x1b[?2004h\x1b]0;title\x07"));
        assert!(ground_after(b"\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"));
        assert!(ground_after(b"\x1b(B\x1b=\x1b>"));
        assert!(ground_after(b"\x1bPq#0;2;0;0;0\x1b\\"));
        assert!(ground_after("ünï — 日本".as_bytes()));
    }

    #[test]
    fn a_stream_cut_inside_a_sequence_or_a_character_is_not() {
        assert!(!ground_after(b"text\x1b"));
        assert!(!ground_after(b"text\x1b["));
        assert!(!ground_after(b"text\x1b[38;5;"));
        assert!(!ground_after(b"\x1b]0;a title"));
        assert!(!ground_after(b"\x1b]0;a title\x1b"));
        assert!(!ground_after(b"\x1bPq#0"));
        assert!(!ground_after(b"\x1b(")); // waiting for the charset
        assert!(!ground_after(&"日".as_bytes()[..2]));
    }

    #[test]
    fn a_sequence_split_over_chunks_is_followed_across_them() {
        let mut tracker = VtTracker::default();
        tracker.observe(b"abc\x1b[3");
        assert!(!tracker.is_ground());
        tracker.observe(b"1m");
        assert!(tracker.is_ground());
    }

    #[test]
    fn cancel_and_a_new_escape_leave_no_sequence_open() {
        assert!(ground_after(b"\x1b[12\x18"));
        assert!(ground_after(b"\x1b]0;x\x1a"));
        assert!(!ground_after(b"\x1b[12\x1b"));
        assert!(ground_after(b"\x1b[12\x1b[0m"));
        assert!(ground_after(b"\x1b]0;x\x1b[0m"), "an ESC [ inside a string ends it and starts a CSI that is then finished");
    }
}
