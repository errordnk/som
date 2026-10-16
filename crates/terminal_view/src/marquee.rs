//! Back-and-forth scrolling of a line of text that is wider than the box
//! it is shown in (the artist/album line and the track title of the
//! "now playing" widget): rest at the start, slide to the end, rest, slide
//! back, rest, and so on. While it rests nothing moves, so nothing needs
//! drawing - see [`repaint_in`].

/// How fast the text slides, in logical pixels per second.
const SPEED_PX_PER_SEC: f32 = 40.0;
/// How long it rests at each end before turning back.
const PAUSE_SECS: f32 = 5.0;
/// How often a sliding line is redrawn.
const SLIDE_FRAME: std::time::Duration = std::time::Duration::from_millis(40);

/// How far (>= 0) the text is shifted to the left `secs` seconds into a
/// scroll cycle, for text that overflows its box by `overflow` pixels:
/// rest at the start, slide to the end, rest, slide back, repeat. `0`
/// when the text fits.
pub fn offset_at(overflow: f32, secs: f32) -> f32 {
    if overflow <= 0.0 {
        return 0.0;
    }
    let travel = overflow / SPEED_PX_PER_SEC;
    let cycle = 2.0 * (PAUSE_SECS + travel);
    let t = secs.rem_euclid(cycle);
    if t < PAUSE_SECS {
        0.0
    } else if t < PAUSE_SECS + travel {
        (t - PAUSE_SECS) * SPEED_PX_PER_SEC
    } else if t < 2.0 * PAUSE_SECS + travel {
        overflow
    } else {
        overflow - (t - (2.0 * PAUSE_SECS + travel)) * SPEED_PX_PER_SEC
    }
}

/// How long until the line of text that overflows by `overflow` pixels,
/// `secs` seconds into its cycle, next needs to be drawn: while it slides,
/// the next frame; while it rests, the moment the rest is over.
pub fn repaint_in(overflow: f32, secs: f32) -> std::time::Duration {
    if overflow <= 0.0 {
        return std::time::Duration::MAX;
    }
    let travel = overflow / SPEED_PX_PER_SEC;
    let cycle = 2.0 * (PAUSE_SECS + travel);
    let t = secs.rem_euclid(cycle);
    let rest_left = if t < PAUSE_SECS {
        Some(PAUSE_SECS - t)
    } else if t >= PAUSE_SECS + travel && t < 2.0 * PAUSE_SECS + travel {
        Some(2.0 * PAUSE_SECS + travel - t)
    } else {
        None
    };
    match rest_left {
        // A little past the end of the rest, so the frame sees the move.
        Some(left) => std::time::Duration::from_secs_f32(left + 0.02),
        None => SLIDE_FRAME,
    }
}

/// Seconds since `text` was first shown - or since it came back after not
/// being shown for a few seconds: a new title starts with its rest at the
/// beginning, however long the program has been running.
pub fn clock_secs(text: &str) -> f32 {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// A line not drawn for this long starts over when it returns.
    const FORGET_AFTER: Duration = Duration::from_secs(3);
    thread_local! {
        static SEEN: std::cell::RefCell<HashMap<String, (Instant, Instant)>> = std::cell::RefCell::new(HashMap::new());
    }
    SEEN.with(|seen| {
        let mut seen = seen.borrow_mut();
        let now = Instant::now();
        seen.retain(|_, (_, last_drawn)| now.duration_since(*last_drawn) < FORGET_AFTER);
        let (started, last_drawn) = seen.entry(text.to_string()).or_insert((now, now));
        *last_drawn = now;
        now.duration_since(*started).as_secs_f32()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_that_fits_does_not_move() {
        assert_eq!(offset_at(0.0, 3.0), 0.0);
        assert_eq!(offset_at(-5.0, 3.0), 0.0);
    }

    #[test]
    fn it_rests_at_the_start_first() {
        assert_eq!(offset_at(100.0, 0.0), 0.0);
        assert_eq!(offset_at(100.0, PAUSE_SECS - 0.1), 0.0);
    }

    #[test]
    fn it_slides_to_the_end_then_rests_there() {
        // 100 px at 40 px/s: 2.5 s of travel after the first rest.
        assert!((offset_at(100.0, PAUSE_SECS + 1.25) - 50.0).abs() < 0.01);
        assert_eq!(offset_at(100.0, PAUSE_SECS + 2.5 + 0.5), 100.0);
    }

    #[test]
    fn it_slides_back_and_the_cycle_repeats() {
        let cycle = 2.0 * (PAUSE_SECS + 2.5);
        let back = PAUSE_SECS + 2.5 + PAUSE_SECS + 1.25;
        assert!((offset_at(100.0, back) - 50.0).abs() < 0.01);
        assert_eq!(offset_at(100.0, cycle), 0.0);
        assert_eq!(offset_at(100.0, cycle + 0.1), 0.0);
    }

    #[test]
    fn a_resting_line_wakes_when_its_rest_is_over() {
        // 100 px: 2.5 s of travel. 1 s into the first rest, 4 s remain.
        let wait = repaint_in(100.0, 1.0).as_secs_f32();
        assert!((wait - 4.02).abs() < 0.01, "waits {wait} s");
        // At the far end, 2 s into the rest: 3 s remain.
        let wait = repaint_in(100.0, PAUSE_SECS + 2.5 + 2.0).as_secs_f32();
        assert!((wait - 3.02).abs() < 0.01, "waits {wait} s");
    }

    #[test]
    fn a_sliding_line_is_drawn_every_frame() {
        assert_eq!(repaint_in(100.0, PAUSE_SECS + 1.0), SLIDE_FRAME);
        assert_eq!(repaint_in(100.0, 2.0 * PAUSE_SECS + 2.5 + 1.0), SLIDE_FRAME);
    }

    #[test]
    fn a_line_that_fits_never_wakes() {
        assert_eq!(repaint_in(0.0, 3.0), std::time::Duration::MAX);
    }

    #[test]
    fn the_clock_of_a_new_text_starts_at_zero() {
        assert!(clock_secs("a title nobody has shown yet") < 0.1);
    }

    #[test]
    fn the_offset_never_leaves_the_overflow_range() {
        for step in 0..2000 {
            let offset = offset_at(137.0, step as f32 * 0.037);
            assert!((0.0..=137.0).contains(&offset), "offset {offset} out of range");
        }
    }
}
