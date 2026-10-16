//! State behind the top-level audio "now playing" widget: the track's
//! tags, its playlist position, the cover art, and where each clickable
//! part was painted.
//!
//! The data comes from two places: `somsrv` pushes `SrvResponse::
//! NowPlaying` (what an internet-radio stream is playing, with a cover
//! URL) and `SrvResponse::Playlist` (this track is entry `index` of
//! `len`) into a placement's `SrvProgressState`; [`AudioCover`] then
//! fetches the cover through the same `FetchResource` + decode path a
//! markdown-embedded image uses. With no cover (nothing found, or the
//! fetch failed) the widget shows [`placeholder_image`].

use crate::rich_content_player;
use crate::rich_content_srv_channel::{self, SrvProgressState};
use crate::rich_content_transport::ContentType;
use gpui::{Bounds, Pixels, RenderImage};
use smallvec::SmallVec;
use std::sync::Arc;
use std::time::Duration;

/// Everything the widget paints for one audio placement.
#[derive(Clone)]
pub struct AudioNowPlaying {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub year: Option<u32>,
    /// Length of the current track; `ZERO` when unknown (no progress bar
    /// then).
    pub duration: Duration,
    /// How far into the current track playback is, by the station's own
    /// clock; meaningful only with a known `duration`.
    pub elapsed: Duration,
    /// `(index, len)` of this track in its playlist, when it has one.
    pub playlist: Option<(u32, u32)>,
    /// The decoded cover once fetched; `None` while it loads or when
    /// there is none (the caller then paints [`placeholder_image`]).
    pub cover: Option<Arc<RenderImage>>,
    /// `true` while a cover is still being fetched/decoded - the paint
    /// path keeps repainting until it settles.
    pub cover_loading: bool,
    /// `true` when the stand-in image should be painted: no cover was
    /// found (or none came in time) - the very last fallback.
    pub show_placeholder: bool,
    /// `true` once everything the widget shows is known: the station's
    /// answer is in and the cover is either on screen or settled as
    /// "none". Times are only shown from then on - before it they would
    /// be a meaningless `0:00`.
    pub ready: bool,
    /// `true` for a track of a CUE sheet: the bar covers just this track
    /// and a click on it seeks inside it, though the track has no clock
    /// of its own.
    pub cue_track: bool,
    /// The track has a known length and plays from a finite stream (a
    /// Yandex Music track): the time shown is the player's own and the bar
    /// can be seeked, unlike a live station's track timed by its clock.
    pub finite_track: bool,
    /// The track belongs to a streaming service's radio: the like and the
    /// wave icons are shown.
    pub radio: Option<RadioIcons>,
    /// Whether the previous / next button works. A CUE sheet is not a
    /// circle: its first track has no previous, its last no next.
    pub can_prev: bool,
    pub can_next: bool,
}

/// The index of the CUE track `position_ms` falls in: the last one that
/// has started (the first one for a position before any of them).
pub fn cue_track_index(tracks: &[somsrv::protocol::CueTrack], position_ms: u64) -> usize {
    tracks.partition_point(|track| track.start_ms <= position_ms).saturating_sub(1)
}

/// What a radio widget's two icons show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RadioIcons {
    /// The track is liked.
    pub liked: bool,
    /// The radio is the one built from a track, not the session's own.
    pub track_wave: bool,
}

/// Where the widget's clickable parts were painted this frame.
#[derive(Clone, Copy, Default)]
pub struct AudioWidgetBounds {
    /// Clicking the cover toggles play/pause.
    pub cover: Option<Bounds<Pixels>>,
    pub prev: Option<Bounds<Pixels>>,
    pub next: Option<Bounds<Pixels>>,
    /// The progress bar; clicking seeks when the track has a duration.
    pub bar: Option<Bounds<Pixels>>,
    /// The like icon of a radio widget.
    pub like: Option<Bounds<Pixels>>,
    /// The icon that switches to the radio built from the playing track.
    pub wave: Option<Bounds<Pixels>>,
}

/// `(duration, elapsed)` of a station's current track, `remaining_ms`
/// having been true `age` ago - the countdown is carried forward by the
/// local clock between two pushes.
pub fn track_times(duration_ms: u32, remaining_ms: u32, age: Duration) -> (Duration, Duration) {
    if duration_ms == 0 {
        return (Duration::ZERO, Duration::ZERO);
    }
    let duration = Duration::from_millis(duration_ms as u64);
    let remaining = Duration::from_millis(remaining_ms as u64).saturating_sub(age).min(duration);
    (duration, duration - remaining)
}

/// How long a cover may take before the stand-in is shown instead of
/// empty space.
const COVER_TIMEOUT: Duration = Duration::from_secs(8);

/// Places the tracks of a live stream on the stream's own frame timeline,
/// so the widget shows what is being HEARD, not what the station's clock
/// says is on air right now.
///
/// What a station reports (`NowPlayingInfo`) is true at the live edge; the
/// speakers play audio that was decoded some seconds ago (network burst,
/// decoder and device buffers). Each report is therefore pinned to the
/// frame count decoded when it arrived: the track ends `remaining` seconds
/// of audio after that point. Whatever track the playhead is inside is
/// the one shown - tags, cover and time left switch exactly when its
/// audio does, however far behind the live edge playback is, and a pause
/// freezes everything with the playhead.
#[derive(Default)]
pub struct TrackTimeline {
    spans: Vec<TrackSpan>,
    last_generation: u64,
}

struct TrackSpan {
    info: somsrv::protocol::NowPlayingInfo,
    /// Frame (counted from the start of the stream) where the track starts.
    start_frame: f64,
    /// Frame where it ends; infinite for a track of unknown length (a
    /// station's in-stream title), which lasts until the next one starts.
    end_frame: f64,
}

/// How early (seconds) a track may be shown before its computed start.
const START_TOLERANCE_SECS: f64 = 1.0;

fn same_track(a: &somsrv::protocol::NowPlayingInfo, b: &somsrv::protocol::NowPlayingInfo) -> bool {
    if a.duration_ms == 0 && b.duration_ms == 0 {
        // A track of unknown length is told apart by artist and title
        // alone: its album and cover arrive later, with a lookup.
        return a.artist == b.artist && a.title == b.title;
    }
    a.artist == b.artist && a.title == b.title && a.album == b.album && a.duration_ms == b.duration_ms
}

impl TrackTimeline {
    /// Feeds one station report (identified by `generation`, `age` old),
    /// pinned to `decoded_frames` - the frames decoded right now - at
    /// `rate` frames per second. Repeats of an already seen report are
    /// ignored; a report of a known track corrects that track's end.
    pub fn observe(
        &mut self,
        generation: u64,
        info: &somsrv::protocol::NowPlayingInfo,
        age: Duration,
        decoded_frames: f64,
        rate: f64,
    ) {
        if generation == self.last_generation || rate <= 0.0 {
            return;
        }
        self.last_generation = generation;
        let decoded_at_arrival = (decoded_frames - age.as_secs_f64() * rate).max(0.0);

        // A track with a known length ends `remaining` seconds of audio
        // after the report; one without lasts from the report until the
        // next track starts.
        let (start_frame, end_frame) = if info.duration_ms > 0 {
            let end = decoded_at_arrival + info.remaining_ms as f64 / 1000.0 * rate;
            (end - info.duration_ms as f64 / 1000.0 * rate, end)
        } else {
            (decoded_at_arrival, f64::INFINITY)
        };

        if let Some(span) = self.spans.iter_mut().find(|span| same_track(&span.info, info)) {
            // A fresh report of the same track corrects its end (the
            // start of an open-ended one stays where it first appeared).
            span.info = info.clone();
            if info.duration_ms > 0 {
                span.start_frame = start_frame;
                span.end_frame = end_frame;
            }
            return;
        }
        // A new track closes every open-ended one before it.
        for span in &mut self.spans {
            if span.end_frame.is_infinite() {
                span.end_frame = span.end_frame.min(start_frame);
            }
        }
        self.spans.push(TrackSpan { info: info.clone(), start_frame, end_frame });
    }

    /// The track the playhead is inside, with how much of it is left
    /// (`None` for a track of unknown length), or `None` when playback is
    /// between tracks: past every track known so far (the next one has
    /// not been reported yet) or in a gap before a reported one starts -
    /// a station's voice inserts and jingles sit between songs and belong
    /// to no track, so nothing is shown for them rather than the next
    /// song ahead of time. Tracks already left behind are forgotten.
    pub fn current(
        &mut self,
        playhead: f64,
        rate: f64,
    ) -> Option<(somsrv::protocol::NowPlayingInfo, Option<Duration>)> {
        if rate <= 0.0 {
            return None;
        }
        self.spans.retain(|span| span.end_frame > playhead);
        // A track counts from its start, give or take the second a
        // station's whole-second clock can be off by.
        let tolerance = START_TOLERANCE_SECS * rate;
        let span = self
            .spans
            .iter()
            .filter(|span| playhead >= span.start_frame - tolerance)
            .min_by(|a, b| a.end_frame.total_cmp(&b.end_frame))?;
        // Time left only means something for a track whose length is
        // known; an open-ended one that was merely closed by the next
        // title has no bar to draw.
        let remaining = (span.end_frame.is_finite() && span.info.duration_ms > 0)
            .then(|| Duration::from_secs_f64(((span.end_frame - playhead) / rate).max(0.0)));
        Some((span.info.clone(), remaining))
    }
}

/// The time left as the widget DRAWS it, kept apart from the one the
/// timeline computes. The timeline is always right about when a track
/// ends (it is what switches tracks); but each fresh station report moves
/// its end by a fraction of a second or a second (the station's clock
/// ticks in whole seconds), and following that would make the number on
/// screen jitter. So the number counts down with the playhead on its own
/// and is moved onto the recomputed time only when the two differ by more
/// than [`DISPLAY_DRIFT_SECS`].
#[derive(Default)]
pub struct DisplayedRemaining {
    track: Option<(String, String, u32)>,
    /// The time left at the moment the number was last set...
    remaining: Duration,
    /// ...and where the playhead was then, in frames.
    playhead_frames: f64,
}

/// How far (seconds) the number on screen may sit from the recomputed time
/// left before it is moved.
const DISPLAY_DRIFT_SECS: f64 = 1.0;

impl DisplayedRemaining {
    /// The time left to draw, given the timeline's `model` value for the
    /// track `info` with the playhead at `playhead_frames` (`rate` frames
    /// per second).
    pub fn update(
        &mut self,
        info: &somsrv::protocol::NowPlayingInfo,
        model: Duration,
        playhead_frames: f64,
        rate: f64,
    ) -> Duration {
        let track = (info.artist.clone(), info.title.clone(), info.duration_ms);
        if rate > 0.0 && self.track.as_ref() == Some(&track) {
            let walked = (playhead_frames - self.playhead_frames) / rate;
            let local = (self.remaining.as_secs_f64() - walked).max(0.0);
            if (local - model.as_secs_f64()).abs() <= DISPLAY_DRIFT_SECS {
                return Duration::from_secs_f64(local);
            }
        }
        self.track = Some(track);
        self.remaining = model;
        self.playhead_frames = playhead_frames;
        model
    }
}

/// One placement's cover-art fetch and its decoded image.
pub struct AudioCover {
    started: std::time::Instant,
    url: String,
    session_id: u32,
    file_id: u32,
    state: Arc<SrvProgressState>,
    content_type: Option<ContentType>,
    player: Option<rich_content_player::RichContentPlayer>,
}

impl AudioCover {
    /// Starts fetching `url` as a Som-minted resource. Subscribes BEFORE
    /// requesting, the order `request_fetch_resource` requires.
    pub fn start(url: &str) -> Self {
        let (session_id, file_id) = rich_content_srv_channel::mint_som_ids();
        let state = rich_content_srv_channel::spawn_progress_listener(session_id, file_id);
        rich_content_srv_channel::request_fetch_resource(session_id, file_id, url.to_string(), None);
        Self { started: std::time::Instant::now(), url: url.to_string(), session_id, file_id, state, content_type: None, player: None }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// `true` once the fetch failed, or has produced no picture within
    /// [`COVER_TIMEOUT`] - the caller then shows the stand-in. A picture
    /// that still arrives later replaces it.
    pub fn failed(&self) -> bool {
        self.state.fetch_failure().is_some() || (self.player.is_none() && self.started.elapsed() > COVER_TIMEOUT)
    }

    /// The decoded cover if it is ready, advancing the decode as bytes
    /// arrive. A failed fetch or an undecodable image stays `None`.
    pub fn image(&mut self) -> Option<Arc<RenderImage>> {
        self.state.request_whole_range_once_if_needed(self.session_id, self.file_id);
        if self.state.fetch_failure().is_some() {
            return None;
        }
        if self.content_type.is_none()
            && let Some((content_type, _metadata)) = self.state.take_metadata()
        {
            self.content_type = Some(content_type);
        }
        let content_type = self.content_type?;
        if !matches!(content_type, ContentType::Png | ContentType::Jpeg | ContentType::Gif) {
            return None;
        }
        let bytes = self.state.bytes_received_so_far();
        let player = rich_content_player::refresh_or_create(
            self.player.take(),
            &bytes,
            self.state.contiguous_len(),
            content_type,
            self.state.total_size(),
        )?;
        let image = player.render_image().clone();
        self.player = Some(player);
        Some(image)
    }

    /// Ends the fetch and returns the decoded image (if any) so the
    /// paint path can release its GPU copy.
    pub fn finish(mut self) -> Option<Arc<RenderImage>> {
        self.state.stop();
        rich_content_srv_channel::unsubscribe(self.session_id, self.file_id);
        self.player.take().map(|player| player.render_image().clone())
    }
}

/// The stand-in shown when a track has no cover: the embedded `dna.png`,
/// without its faint background hatching, inverted for a dark theme.
/// Built once per polarity - a `RenderImage` is a fresh GPU texture each
/// time it is constructed.
pub fn placeholder_image(is_light: bool) -> Option<Arc<RenderImage>> {
    static DARK: std::sync::OnceLock<Option<Arc<RenderImage>>> = std::sync::OnceLock::new();
    static LIGHT: std::sync::OnceLock<Option<Arc<RenderImage>>> = std::sync::OnceLock::new();
    let cell = if is_light { &LIGHT } else { &DARK };
    cell.get_or_init(|| {
        let bytes = assets::Assets::get("images/dna.png")?;
        let mut image = image::load_from_memory(&bytes.data).ok()?.to_rgba8();
        // The source carries a diagonal hatch at very low alpha; against
        // a dark fill the inverted hatch shows as stray stripes.
        const HATCH_ALPHA_CUTOFF: u8 = 30;
        for pixel in image.pixels_mut() {
            if pixel[3] < HATCH_ALPHA_CUTOFF {
                pixel[3] = 0;
            }
            if !is_light {
                pixel[0] = 255 - pixel[0];
                pixel[1] = 255 - pixel[1];
                pixel[2] = 255 - pixel[2];
            }
            // gpui renders BGRA.
            pixel.0.swap(0, 2);
        }
        Some(Arc::new(RenderImage::new(SmallVec::from_vec(vec![image::Frame::new(image)]))))
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue_tracks(starts: &[u64]) -> Vec<somsrv::protocol::CueTrack> {
        starts
            .iter()
            .map(|&start_ms| somsrv::protocol::CueTrack { start_ms, ..Default::default() })
            .collect()
    }

    #[test]
    fn the_cue_track_is_the_last_one_that_has_started() {
        let tracks = cue_tracks(&[0, 179_000, 438_000]);
        assert_eq!(cue_track_index(&tracks, 0), 0);
        assert_eq!(cue_track_index(&tracks, 178_999), 0);
        assert_eq!(cue_track_index(&tracks, 179_000), 1);
        assert_eq!(cue_track_index(&tracks, 5_000_000), 2);
    }

    #[test]
    fn before_the_first_cue_track_starts_the_first_is_shown() {
        // A hidden lead-in: the first track starts two seconds in.
        let tracks = cue_tracks(&[2_000, 100_000]);
        assert_eq!(cue_track_index(&tracks, 500), 0);
        assert_eq!(cue_track_index(&[], 500), 0);
    }

    fn info(title: &str, duration_ms: u32, remaining_ms: u32) -> somsrv::protocol::NowPlayingInfo {
        somsrv::protocol::NowPlayingInfo {
            session_id: 1,
            file_id: 2,
            artist: "A".into(),
            title: title.into(),
            album: "B".into(),
            year: None,
            duration_ms,
            remaining_ms,
            cover_url: None,
            cover_lookup_pending: false,
            next: None,
        }
    }

    #[test]
    fn the_end_of_a_track_is_pinned_to_the_audio_decoded_when_it_was_reported() {
        let mut timeline = TrackTimeline::default();
        // 100 frames/s; 3000 frames (30 s) already decoded, track has 20 s left.
        timeline.observe(1, &info("one", 200_000, 20_000), Duration::ZERO, 3000.0, 100.0);

        // The speakers are 10 s behind the live edge (playhead at frame 2000):
        // 30 s of this track remain for the listener, not 20.
        let (shown, remaining) = timeline.current(2000.0, 100.0).unwrap();
        assert_eq!(shown.title, "one");
        assert_eq!(remaining, Some(Duration::from_secs(30)));
    }

    #[test]
    fn the_next_track_appears_only_when_the_playhead_reaches_it() {
        let mut timeline = TrackTimeline::default();
        timeline.observe(1, &info("one", 200_000, 20_000), Duration::ZERO, 3000.0, 100.0);
        // The live edge has moved on: a new track was reported 20 s later.
        timeline.observe(2, &info("two", 180_000, 170_000), Duration::ZERO, 5000.0, 100.0);

        // Playhead still inside "one" (ends at frame 5000): shows "one".
        assert_eq!(timeline.current(4000.0, 100.0).unwrap().0.title, "one");
        // Past it: now "two".
        assert_eq!(timeline.current(5100.0, 100.0).unwrap().0.title, "two");
    }

    #[test]
    fn a_voice_insert_between_songs_shows_nothing() {
        let mut timeline = TrackTimeline::default();
        // 100 frames/s. "one" ends at frame 5000.
        timeline.observe(1, &info("one", 200_000, 20_000), Duration::ZERO, 3000.0, 100.0);
        // "two" (180 s) is reported while the insert still plays: it ends
        // at 5000 + 54 s insert + 180 s = frame 28400, so it starts at 10400.
        timeline.observe(2, &info("two", 180_000, 254_000), Duration::ZERO, 3000.0, 100.0);

        assert_eq!(timeline.current(4000.0, 100.0).unwrap().0.title, "one");
        // Inside the insert: neither track.
        assert!(timeline.current(7000.0, 100.0).is_none());
        // The song starts.
        assert_eq!(timeline.current(10400.0, 100.0).unwrap().0.title, "two");
    }

    #[test]
    fn a_crossfade_still_hands_over_at_the_end_of_the_first_track() {
        let mut timeline = TrackTimeline::default();
        timeline.observe(1, &info("one", 200_000, 20_000), Duration::ZERO, 3000.0, 100.0);
        // "two" starts 3 s before "one" ends (crossfade): ends at 5000+17000.
        timeline.observe(2, &info("two", 200_000, 190_000), Duration::ZERO, 3000.0, 100.0);

        assert_eq!(timeline.current(4800.0, 100.0).unwrap().0.title, "one");
        assert_eq!(timeline.current(5001.0, 100.0).unwrap().0.title, "two");
    }

    #[test]
    fn past_every_known_track_nothing_is_shown() {
        let mut timeline = TrackTimeline::default();
        timeline.observe(1, &info("one", 200_000, 1_000), Duration::ZERO, 1000.0, 100.0);

        assert!(timeline.current(1200.0, 100.0).is_none());
    }

    #[test]
    fn a_repeated_report_does_not_move_the_track() {
        let mut timeline = TrackTimeline::default();
        timeline.observe(1, &info("one", 200_000, 20_000), Duration::ZERO, 3000.0, 100.0);
        // Same generation again, much later: ignored.
        timeline.observe(1, &info("one", 200_000, 5_000), Duration::ZERO, 9000.0, 100.0);

        let (_, remaining) = timeline.current(3000.0, 100.0).unwrap();
        assert_eq!(remaining, Some(Duration::from_secs(20)));
    }

    #[test]
    fn the_displayed_time_ignores_a_correction_of_a_second_or_less() {
        let mut shown = DisplayedRemaining::default();
        let track = info("one", 200_000, 0);
        // 100 frames/s. First sight: 20 s left at frame 3000.
        assert_eq!(shown.update(&track, Duration::from_secs(20), 3000.0, 100.0), Duration::from_secs(20));
        // 5 s later the timeline says 14.2 s (a 0.8 s correction of its
        // end): the number keeps counting from what it had, 15 s.
        assert_eq!(shown.update(&track, Duration::from_millis(14_200), 3500.0, 100.0), Duration::from_secs(15));
        // Exactly one second off is still no reason to move it.
        assert_eq!(shown.update(&track, Duration::from_secs(14), 3500.0, 100.0), Duration::from_secs(15));
    }

    #[test]
    fn the_displayed_time_follows_a_correction_of_more_than_a_second() {
        let mut shown = DisplayedRemaining::default();
        let track = info("one", 200_000, 0);
        shown.update(&track, Duration::from_secs(20), 3000.0, 100.0);
        // 5 s later the timeline says 12 s: three seconds off -> moved.
        assert_eq!(shown.update(&track, Duration::from_secs(12), 3500.0, 100.0), Duration::from_secs(12));
        // And it counts down from there.
        assert_eq!(shown.update(&track, Duration::from_millis(11_300), 3600.0, 100.0), Duration::from_secs(11));
    }

    #[test]
    fn a_new_track_resets_the_displayed_time() {
        let mut shown = DisplayedRemaining::default();
        shown.update(&info("one", 200_000, 0), Duration::from_secs(20), 3000.0, 100.0);
        let next = info("two", 180_000, 0);
        assert_eq!(shown.update(&next, Duration::from_secs(180), 5000.0, 100.0), Duration::from_secs(180));
    }

    #[test]
    fn the_displayed_time_follows_a_seek() {
        let mut shown = DisplayedRemaining::default();
        let track = info("one", 200_000, 0);
        shown.update(&track, Duration::from_secs(100), 1000.0, 100.0);
        // The playhead jumps back 30 s: the model says 130 s, the local
        // count says 130 s too - no jump between the two.
        assert_eq!(shown.update(&track, Duration::from_secs(130), -2000.0 + 1000.0, 100.0), Duration::from_secs(130));
    }

    #[test]
    fn a_fresh_report_of_the_same_track_corrects_its_end() {
        let mut timeline = TrackTimeline::default();
        timeline.observe(1, &info("one", 200_000, 20_000), Duration::ZERO, 3000.0, 100.0);
        // Five seconds later the station says 14 s are left (clock drift).
        timeline.observe(2, &info("one", 200_000, 14_000), Duration::ZERO, 3500.0, 100.0);

        let (_, remaining) = timeline.current(3500.0, 100.0).unwrap();
        assert_eq!(remaining, Some(Duration::from_secs(14)));
    }

    #[test]
    fn the_age_of_a_report_is_taken_off_the_decoded_count() {
        let mut timeline = TrackTimeline::default();
        // Reported 2 s ago: at that moment 2800 frames had been decoded.
        timeline.observe(1, &info("one", 200_000, 20_000), Duration::from_secs(2), 3000.0, 100.0);

        let (_, remaining) = timeline.current(2800.0, 100.0).unwrap();
        assert_eq!(remaining, Some(Duration::from_secs(20)));
    }

    #[test]
    fn a_title_without_a_length_lasts_until_the_next_title_starts() {
        let mut timeline = TrackTimeline::default();
        // 100 frames/s. "one" is reported when 1000 frames were decoded,
        // "two" when 4000 were - the listener is 5 s behind.
        timeline.observe(1, &info("one", 0, 0), Duration::ZERO, 1000.0, 100.0);
        timeline.observe(2, &info("two", 0, 0), Duration::ZERO, 4000.0, 100.0);

        // Still hearing "one" until frame 4000.
        let (shown, remaining) = timeline.current(3000.0, 100.0).unwrap();
        assert_eq!(shown.title, "one");
        assert_eq!(remaining, None, "no length, no time left");
        assert_eq!(timeline.current(4100.0, 100.0).unwrap().0.title, "two");
    }

    #[test]
    fn the_album_arriving_later_does_not_start_a_new_open_track() {
        let mut timeline = TrackTimeline::default();
        timeline.observe(1, &info("one", 0, 0), Duration::ZERO, 1000.0, 100.0);
        let mut with_album = info("one", 0, 0);
        with_album.album = "Found Later".into();
        timeline.observe(2, &with_album, Duration::ZERO, 1200.0, 100.0);

        let (shown, _) = timeline.current(1100.0, 100.0).unwrap();
        assert_eq!(shown.album, "Found Later");
        // Still the one track, started at the first report.
        assert_eq!(timeline.current(1001.0, 100.0).unwrap().0.title, "one");
    }

    #[test]
    fn a_report_without_a_duration_is_shown_from_the_first_report_on() {
        let mut timeline = TrackTimeline::default();
        timeline.observe(1, &info("one", 0, 0), Duration::ZERO, 3000.0, 100.0);

        assert_eq!(timeline.current(3000.0, 100.0).unwrap().0.title, "one");
    }

    #[test]
    fn elapsed_is_duration_minus_what_remains() {
        let (duration, elapsed) = track_times(600_000, 540_000, Duration::ZERO);
        assert_eq!(duration, Duration::from_secs(600));
        assert_eq!(elapsed, Duration::from_secs(60));
    }

    #[test]
    fn countdown_is_carried_forward_by_the_age_of_the_push() {
        let (_, elapsed) = track_times(600_000, 540_000, Duration::from_secs(5));
        assert_eq!(elapsed, Duration::from_secs(65));
    }

    #[test]
    fn a_stale_push_never_reports_more_than_the_whole_track() {
        let (duration, elapsed) = track_times(600_000, 1_000, Duration::from_secs(30));
        assert_eq!(elapsed, duration);
    }

    #[test]
    fn remaining_longer_than_the_track_is_clamped() {
        let (duration, elapsed) = track_times(100_000, 900_000, Duration::ZERO);
        assert_eq!(duration, Duration::from_secs(100));
        assert_eq!(elapsed, Duration::ZERO);
    }

    #[test]
    fn unknown_duration_has_no_times() {
        assert_eq!(track_times(0, 5_000, Duration::ZERO), (Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn placeholder_decodes_for_both_polarities() {
        assert!(placeholder_image(true).is_some());
        assert!(placeholder_image(false).is_some());
    }
}
