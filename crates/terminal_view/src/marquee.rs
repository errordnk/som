//! Back-and-forth scrolling of a line of text that is wider than the box
//! it is shown in (the artist/album line and the track title of the
//! "now playing" widget).

/// How fast the text slides, in logical pixels per second.
const SPEED_PX_PER_SEC: f32 = 40.0;
/// How long it rests at each end before turning back.
const PAUSE_SECS: f32 = 1.5;

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

/// Seconds since the first call - the shared clock every scrolling line
/// reads, so they all move in step.
pub fn now_secs() -> f32 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f32()
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
    fn the_offset_never_leaves_the_overflow_range() {
        for step in 0..2000 {
            let offset = offset_at(137.0, step as f32 * 0.037);
            assert!((0.0..=137.0).contains(&offset), "offset {offset} out of range");
        }
    }
}
