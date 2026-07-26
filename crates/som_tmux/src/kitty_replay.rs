//! Tracks Kitty terminal graphics protocol `Transmit` APC sequences seen on
//! the HOLDER's PTY stream, so a (re)connecting RELAY can be handed the raw
//! bytes of every still-relevant transmit again — see
//! `KITTY_GRAPHICS_PLAN.md` Stage 6.
//!
//! **Why this exists at all.** A HOLDER's own `alacritty_terminal::Term` has
//! no concept of Kitty graphics beyond a transient per-APC-string byte
//! buffer (`Term::apc_buffer`, drained into an `Event::ApcString` the moment
//! each APC string ends) — decoded images live exclusively in the real
//! Som-side `Terminal`'s `kitty_graphics_store::ImageStore`
//! (`crates/terminal/src/terminal.rs`), which this crate deliberately does
//! not depend on (see `parse_cursor_shape`'s doc comment in `session.rs` for
//! the same "duplicate the small bit, don't pull in GPUI" reasoning). That
//! means `Term::snapshot()`/`TermState` (what carries a (re)connecting
//! RELAY's grid/cursor/mode state today) structurally cannot carry image
//! data — it was never there to snapshot in the first place. Without this
//! module, a `yazi`/`icat`-style program's images would render correctly
//! for whichever RELAY connection was live WHEN the `Transmit` APC arrived,
//! then vanish forever (blank space, or for Unicode-placeholder-mode
//! images, orphaned placeholder glyphs referencing an image id nobody ever
//! told the new connection about) on every subsequent reconnect — even
//! though the exact same image is still sitting in that connection's
//! scrollback/current screen.
//!
//! **What this actually stores.** The complete, unmodified raw APC bytes
//! (`ESC _ G ... ESC \`) of every successful direct-transmission `Transmit`
//! command (`a=T`/`a=t`), keyed by image id — replaying the ORIGINAL bytes
//! (not re-encoding a parsed representation) means a (re)connecting RELAY's
//! real Som-side `Terminal`/`kitty_graphics::parse_command` decodes them
//! through the exact same path a live PTY read would have, with no risk of
//! this module's own (deliberately minimal) parsing disagreeing with the
//! real one over some edge case. Chunked transmissions (`m=1` continuing to
//! `m=0`) are stored as the concatenation of every chunk's raw bytes in
//! arrival order, replayed back as that same sequence of separate APC
//! strings — not reassembled into one, since `ImageStore::apply_transmit`
//! already knows how to reassemble chunks itself and doing it twice would
//! be redundant, riskier, and provide no benefit.
//!
//! **What this deliberately does NOT track.** `Place`/`Delete` commands, or
//! anonymous (id-less) transmits — see `record`'s doc comment. Chunked
//! transmissions targeting the 8-bit/256-color placement-id encoding, raw
//! `t=f`/`t=t` file transmission (already unsupported client-side, see
//! `crate::kitty_graphics::TransmissionMedium::Unsupported`'s doc comment
//! for why), and the query action (`a=q`, which never transmits real data)
//! are outside this module's scope for the same reasons they're outside
//! `kitty_graphics::parse_command`'s.
use std::collections::HashMap;
use std::sync::Mutex;

/// One in-progress or completed transmit's accumulated raw APC bytes
/// (including each chunk's own `ESC _ G ... ESC \` wrapper — replay just
/// resends these verbatim, chunk boundaries and all).
#[derive(Default, Clone)]
struct RecordedTransmit {
    chunks: Vec<u8>,
}

/// Thread-safe store, shared (via `Arc`) between the HOLDER's PTY-event pump
/// thread (which calls `record` as `Event::ApcString`s arrive) and every
/// `handle_relay` connection thread (which calls `replay_bytes` once, right
/// after sending `Snapshot`, before subscribing to the live raw-byte
/// stream — see `server.rs::handle_relay`).
#[derive(Default)]
pub struct KittyReplayBuffer {
    by_image_id: Mutex<HashMap<u32, RecordedTransmit>>,
}

impl KittyReplayBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one APC string's raw payload bytes (the same shape
    /// `Term::apc_buffer` hands to `Event::ApcString` — i.e. `Ga=T,...`,
    /// NOT wrapped in `ESC _`/`ESC \` yet) into this buffer.
    ///
    /// Only `Transmit` commands (`a=T`/`a=t`, or the bare-defaults-to-
    /// transmit form) with both an explicit `i=` id AND direct (`t=d` or
    /// omitted) transmission medium are recorded — anonymous images
    /// (referenced only by `I=`, the image *number*) have no stable key to
    /// record under, `Place`/`Delete`/`Query` carry no image bytes of their
    /// own to replay, and `t=f`/`t=t` file transmission is already refused
    /// client-side (see this module's doc comment) so there is nothing
    /// meaningful to preserve for it here either. A non-matching command is
    /// silently ignored, not an error — most APC strings on a real PTY
    /// stream (if any at all) simply aren't Kitty graphics commands.
    pub fn record(&self, apc_payload: &[u8]) {
        let Some(id) = transmit_image_id(apc_payload) else { return };
        let more_chunks = has_more_chunks(apc_payload);

        let mut map = self.by_image_id.lock().unwrap();
        let entry = map.entry(id).or_default();
        if !more_chunks {
            // A new (non-continuing) transmit for an id that already has
            // recorded chunks means the sender re-transmitted this image
            // from scratch (same semantics as
            // `ImageStore::insert_image`'s "replace, don't double-count"
            // handling) — start this id's record over rather than
            // appending, so replay doesn't resend a stale chunk sequence
            // followed by an unrelated fresh one.
            if !has_more_chunks_flag_was_set(&entry.chunks) {
                entry.chunks.clear();
            }
        }
        append_wrapped(&mut entry.chunks, apc_payload);
    }

    /// The raw bytes (already `ESC _`/`ESC \`-wrapped, ready to send as-is)
    /// of every COMPLETE transmit currently recorded — i.e. skips any image
    /// still mid-chunking (`m=1` seen but no terminating `m=0` yet), since
    /// replaying a partial chunk sequence would hand the client invalid
    /// base64/PNG data. A still-chunking transmit will complete naturally
    /// on the live byte stream this replay precedes, same as it would have
    /// for a client that was connected the whole time.
    pub fn replay_bytes(&self) -> Vec<u8> {
        let map = self.by_image_id.lock().unwrap();
        let mut out = Vec::new();
        for entry in map.values() {
            if has_more_chunks_flag_was_set(&entry.chunks) {
                continue;
            }
            out.extend_from_slice(&entry.chunks);
        }
        out
    }
}

/// Appends one APC payload's full escape-sequence form (`ESC _` + payload +
/// `ESC \`) to `buf`.
fn append_wrapped(buf: &mut Vec<u8>, apc_payload: &[u8]) {
    buf.extend_from_slice(b"\x1b_");
    buf.extend_from_slice(apc_payload);
    buf.extend_from_slice(b"\x1b\\");
}

/// Extremely small, deliberately non-exhaustive control-data scan — NOT a
/// replacement for `crate::kitty_graphics::parse_command` (which this crate
/// intentionally doesn't depend on, see this module's doc comment). Only
/// answers "is this a Transmit with medium=direct and an explicit `i=`
/// id?", returning that id, or `None` for anything else (including
/// malformed input — this must never panic on arbitrary PTY bytes).
fn transmit_image_id(apc_payload: &[u8]) -> Option<u32> {
    let payload = apc_payload.strip_prefix(b"G")?;
    let payload = payload.strip_prefix(b",").unwrap_or(payload);
    let control_bytes = match payload.iter().position(|&b| b == b';') {
        Some(idx) => &payload[..idx],
        None => payload,
    };
    let control_text = std::str::from_utf8(control_bytes).ok()?;

    let mut action = None;
    let mut id = None;
    let mut medium = None;
    for pair in control_text.split(',') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "a" => action = Some(value),
            "i" => id = value.parse::<u32>().ok(),
            "t" => medium = Some(value),
            _ => {},
        }
    }

    // Bare (no `a=` at all) defaults to transmit-only, same as
    // `kitty_graphics::parse_command`.
    if !matches!(action, None | Some("t") | Some("T")) {
        return None;
    }
    // `t=d` (direct) or omitted (spec default) — anything else (`f`/`t`/`s`)
    // is a medium this implementation never stores real data for anyway.
    if !matches!(medium, None | Some("d")) {
        return None;
    }
    id
}

fn has_more_chunks(apc_payload: &[u8]) -> bool {
    let Some(payload) = apc_payload.strip_prefix(b"G") else { return false };
    let payload = payload.strip_prefix(b",").unwrap_or(payload);
    let control_bytes = match payload.iter().position(|&b| b == b';') {
        Some(idx) => &payload[..idx],
        None => payload,
    };
    let Ok(control_text) = std::str::from_utf8(control_bytes) else { return false };
    control_text.split(',').any(|pair| pair == "m=1")
}

/// Re-derives "is the LAST chunk in this recorded byte sequence still
/// waiting on more?" by scanning backward for the last complete APC string
/// and checking its own `m=` flag — avoids a separate bool field that could
/// drift out of sync with `chunks` itself.
fn has_more_chunks_flag_was_set(recorded_bytes: &[u8]) -> bool {
    let Some(last_start) = find_last_apc_start(recorded_bytes) else { return false };
    let after_start = &recorded_bytes[last_start + 2..]; // skip "\x1b_"
    let end = after_start.windows(2).position(|w| w == b"\x1b\\").unwrap_or(after_start.len());
    has_more_chunks(&after_start[..end])
}

fn find_last_apc_start(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).enumerate().rev().find(|(_, w)| *w == b"\x1b_").map(|(idx, _)| idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_replays_a_simple_transmit() {
        let buffer = KittyReplayBuffer::new();
        buffer.record(b"Ga=T,f=100,i=7;AAAA");

        let replayed = buffer.replay_bytes();
        assert_eq!(replayed, b"\x1b_Ga=T,f=100,i=7;AAAA\x1b\\".to_vec());
    }

    #[test]
    fn ignores_commands_with_no_id() {
        let buffer = KittyReplayBuffer::new();
        buffer.record(b"Ga=T,f=100;AAAA"); // no i=
        assert!(buffer.replay_bytes().is_empty());
    }

    #[test]
    fn ignores_non_transmit_actions() {
        let buffer = KittyReplayBuffer::new();
        buffer.record(b"Ga=p,i=7,p=1");
        buffer.record(b"Ga=d,d=a");
        buffer.record(b"Ga=q,i=7");
        assert!(buffer.replay_bytes().is_empty());
    }

    #[test]
    fn ignores_file_transmission_medium() {
        let buffer = KittyReplayBuffer::new();
        buffer.record(b"Ga=t,i=7,t=f;/etc/passwd");
        assert!(buffer.replay_bytes().is_empty());
    }

    #[test]
    fn does_not_replay_an_incomplete_chunked_transmit() {
        let buffer = KittyReplayBuffer::new();
        buffer.record(b"Ga=T,i=1,m=1;AAAA");
        assert!(
            buffer.replay_bytes().is_empty(),
            "an image still mid-chunking must not be replayed as if it were complete"
        );
    }

    #[test]
    fn replays_a_completed_chunked_transmit_as_the_full_original_sequence() {
        let buffer = KittyReplayBuffer::new();
        buffer.record(b"Ga=T,i=1,m=1;AAAA");
        buffer.record(b"Ga=T,i=1,m=0;BBBB");

        let replayed = buffer.replay_bytes();
        assert_eq!(
            replayed,
            b"\x1b_Ga=T,i=1,m=1;AAAA\x1b\\\x1b_Ga=T,i=1,m=0;BBBB\x1b\\".to_vec(),
            "chunks must replay in original arrival order, each with its own wrapper, not merged into one"
        );
    }

    #[test]
    fn retransmitting_the_same_id_replaces_the_old_recording() {
        let buffer = KittyReplayBuffer::new();
        buffer.record(b"Ga=T,i=1,f=100;OLDDATA");
        buffer.record(b"Ga=T,i=1,f=100;NEWDATA");

        let replayed = buffer.replay_bytes();
        assert_eq!(replayed, b"\x1b_Ga=T,i=1,f=100;NEWDATA\x1b\\".to_vec());
    }

    #[test]
    fn multiple_distinct_image_ids_all_replay() {
        let buffer = KittyReplayBuffer::new();
        buffer.record(b"Ga=T,i=1;AAAA");
        buffer.record(b"Ga=T,i=2;BBBB");

        // HashMap iteration order isn't guaranteed — check both are present
        // rather than asserting a specific concatenation order.
        let text = String::from_utf8_lossy(&buffer.replay_bytes()).into_owned();
        assert!(text.contains("i=1;AAAA"));
        assert!(text.contains("i=2;BBBB"));
    }

    #[test]
    fn malformed_bytes_do_not_panic() {
        let buffer = KittyReplayBuffer::new();
        buffer.record(&[0xFF, 0xFE, 0x00]);
        buffer.record(b"not even close to valid");
        buffer.record(b"");
        assert!(buffer.replay_bytes().is_empty());
    }
}
