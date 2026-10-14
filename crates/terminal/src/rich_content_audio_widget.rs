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
