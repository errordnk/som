//! The relay's one output: the shell's own bytes and the link's frames go to
//! the same pty, and a frame must never land in the middle of an escape
//! sequence of the shell's (see `vt`).

use super::vt::VtTracker;
use std::io::{self, Write};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a frame waits for the shell's output to be between sequences
/// before it is written anyway (a sequence that never finishes must not shut
/// the link out for good).
const GROUND_PATIENCE: Duration = Duration::from_millis(300);

pub struct PtyOutput<W: Write> {
    state: Mutex<State<W>>,
    at_rest: Condvar,
}

struct State<W> {
    out: W,
    tracker: VtTracker,
}

impl<W: Write> PtyOutput<W> {
    pub fn new(out: W) -> Self {
        Self { state: Mutex::new(State { out, tracker: VtTracker::default() }), at_rest: Condvar::new() }
    }

    /// Writes bytes of the shell's own output.
    pub fn write_shell(&self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.out.write_all(bytes)?;
        state.out.flush()?;
        state.tracker.observe(bytes);
        if state.tracker.is_ground() {
            self.at_rest.notify_all();
        }
        Ok(())
    }

    /// Writes a frame, once the shell's output is between sequences.
    pub fn write_frame(&self, frame: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + GROUND_PATIENCE;
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while !state.tracker.is_ground() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
            state = self.at_rest.wait_timeout(state, left).unwrap_or_else(|poisoned| poisoned.into_inner()).0;
        }
        state.out.write_all(frame)?;
        state.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_frame_waits_for_the_end_of_the_sequence_the_shell_is_in() {
        let capture = Capture::default();
        let output = Arc::new(PtyOutput::new(capture.clone()));
        output.write_shell(b"text\x1b[38;5;").unwrap();

        let writer = output.clone();
        let frame = std::thread::spawn(move || writer.write_frame(b"<FRAME>").unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(capture.0.lock().unwrap().as_slice(), b"text\x1b[38;5;", "the frame is not written inside a sequence");

        output.write_shell(b"196mred").unwrap();
        frame.join().unwrap();
        assert_eq!(capture.0.lock().unwrap().as_slice(), b"text\x1b[38;5;196mred<FRAME>");
    }

    #[test]
    fn a_frame_is_not_kept_for_ever_by_a_sequence_that_never_ends() {
        let capture = Capture::default();
        let output = PtyOutput::new(capture.clone());
        output.write_shell(b"\x1b]0;a title that never ends").unwrap();
        let started = Instant::now();
        output.write_frame(b"<FRAME>").unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(capture.0.lock().unwrap().ends_with(b"<FRAME>"));
    }

    #[test]
    fn frames_and_shell_bytes_keep_their_order_when_the_stream_is_at_rest() {
        let capture = Capture::default();
        let output = PtyOutput::new(capture.clone());
        output.write_shell(b"a").unwrap();
        output.write_frame(b"<1>").unwrap();
        output.write_shell(b"b").unwrap();
        output.write_frame(b"<2>").unwrap();
        assert_eq!(capture.0.lock().unwrap().as_slice(), b"a<1>b<2>");
    }
}
