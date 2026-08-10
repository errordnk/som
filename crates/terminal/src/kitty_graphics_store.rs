//! Storage and decoding for Kitty terminal graphics protocol images.
//!
//! Consumes [`crate::kitty_graphics::KittyGraphicsCommand`]s (already
//! parsed out of raw APC bytes) and turns `Transmit` commands into decoded,
//! GPU-upload-ready pixel data; tracks `Place`/`Delete` commands against
//! that data. Rendering (actually painting decoded images to screen) is a
//! separate concern — see `KITTY_GRAPHICS_PLAN.md` Stage 3 — this module
//! only owns the data.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use gpui::RenderImage;
use image::{Delay, Frame, RgbaImage};
use smallvec::SmallVec;

use crate::kitty_graphics::{
    AnimationCommand, AnimationState, DeleteCommand, FrameCommand, FrameGap, ImageFormat,
    KittyGraphicsCommand, PlaceCommand, TransmissionMedium, TransmitCommand,
};

/// Resolves a parsed [`FrameGap`] to the actual [`Delay`] a frame should be
/// held for — see `FrameGap`'s doc comment for the spec citations behind
/// each case. `Gapless` (spec: "advance to the next frame immediately") has
/// no real zero-duration representation that wouldn't also look like "no
/// delay was specified"; a single millisecond is the shortest delay this
/// store's `Duration`-based time-check (`DecodedImage::current_frame`)
/// can meaningfully act on, close enough to "immediate" for any paint rate
/// GPUI actually runs at.
fn delay_from_gap(gap: FrameGap) -> Delay {
    match gap {
        FrameGap::Default => Delay::from_numer_denom_ms(40, 1),
        FrameGap::Milliseconds(ms) => Delay::from_numer_denom_ms(ms.max(1), 1),
        FrameGap::Gapless => Delay::from_numer_denom_ms(1, 1),
    }
}

/// Total memory budget for image data held by one terminal's [`ImageStore`],
/// in bytes of decoded (not base64/PNG-compressed) pixel data.
///
/// Kitty itself enforces client-side quotas for the same reason: a remote
/// program (this is routinely the far end of an SSH session for Som — see
/// `KITTY_GRAPHICS_PLAN.md`'s "Транспортные особенности Som" section) could
/// otherwise exhaust local memory just by sending enough `Transmit`
/// commands, whether maliciously or from a buggy client-side tool. 320MB
/// is generous for realistic terminal image use (a handful of screen-sized
/// images) while still bounding worst-case damage.
const MAX_STORE_BYTES: usize = 320 * 1024 * 1024;

/// One decoded image, GPU-upload-ready and cached as an `Arc` so repeated
/// paint calls (every frame the image is on screen) never re-decode or
/// re-copy pixel data — only the first `Transmit`/`Frame` command pays that
/// cost.
///
/// Frames beyond the first are populated by Kitty's `a=f` frame-append
/// action (`ImageStore::apply_frame`) — animated images (e.g. a GIF
/// re-encoded by `crates/somcat`) arrive as one `a=T` base frame followed by
/// a series of `a=f` frames, each with its own display duration (`z=`,
/// Kitty's "gap" key). `render_image` itself needs no special handling for
/// this — `gpui::RenderImage` was already built around a `SmallVec<[Frame; 1]>`
/// (see `RenderImage::new`) specifically so multi-frame support could slot
/// in here without restructuring that type.
pub struct DecodedImage {
    pub render_image: Arc<RenderImage>,
    /// Decoded byte size, summed across all frames — cheaper to track
    /// incrementally than to recompute by walking frames on every quota
    /// check.
    byte_size: usize,
    /// Animation playback state — `None` for a plain (single-frame or
    /// not-yet-animating) image. `Cell`-wrapped so a renderer can advance
    /// playback (`ImageStore::all_placements`/`image` only ever hand out
    /// shared `&DecodedImage` references — see `terminal_element.rs`'s
    /// `paint_kitty_placements`, which reads through `Entity<Terminal>::
    /// read`, not `update`, since painting must not require mutable access
    /// to terminal state) without needing `&mut self` at paint time.
    animation: Cell<Option<AnimationPlayback>>,
}

/// Renamed from the protocol's own `AnimationState` (`kitty_graphics::
/// AnimationState` — the parsed `s=1`/`s=2`/`s=3` value) to avoid a name
/// collision: this is the STORE's derived runtime playback state, not a
/// verbatim copy of the wire value.
#[derive(Clone, Copy)]
struct AnimationPlayback {
    playing: bool,
    /// Whether reaching the last frame should loop back to the first
    /// (`kitty_graphics::AnimationState::Running`, `s=3`) or hold there
    /// waiting for more frames (`::Loading`, `s=2`) — see
    /// `DecodedImage::current_frame`'s use of this.
    loop_when_done: bool,
    current_frame: usize,
    /// When `current_frame` was last advanced — `None` means "just reset,
    /// advance on the very next paint check" (mirrors `gpui::elements::img`'s
    /// `ImgState::last_frame_time`).
    last_advance: Option<Instant>,
}

impl DecodedImage {
    fn from_single_frame(buffer: RgbaImage) -> Self {
        let byte_size = buffer.as_raw().len();
        let render_image = Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1)));
        Self { render_image, byte_size, animation: Cell::new(None) }
    }

    /// Frames are stored, but NOT set playing — per spec, `a=f` only
    /// transmits frame data; playback only actually starts once a
    /// separate `a=a` (`SetState` with `Loading`/`Running`) arrives. This
    /// mirrors `ImageStore::apply_animation`'s own call order: finalize
    /// frames first (this function), then apply the playback state the
    /// triggering `a=a` command actually carried.
    fn from_frames(frames: Vec<Frame>) -> Self {
        let byte_size = frames.iter().map(|f| f.buffer().as_raw().len()).sum();
        let render_image = Arc::new(RenderImage::new(SmallVec::from_vec(frames)));
        Self { render_image, byte_size, animation: Cell::new(None) }
    }

    /// The frame index a renderer should paint right now, advancing
    /// playback as a side effect if enough time has passed since the last
    /// frame switch — mirrors `gpui::elements::img`'s `request_layout` time
    /// check, just triggered from `terminal_element.rs`'s paint pass
    /// instead of a dedicated layout phase. Always `0` for a non-animated
    /// (or currently-paused) image.
    pub fn current_frame(&self) -> usize {
        let Some(mut state) = self.animation.get() else {
            return 0;
        };
        let frame_count = self.render_image.frame_count();
        if !state.playing || frame_count <= 1 {
            return state.current_frame;
        }
        // Loading mode (s=2): once the last transmitted frame is reached,
        // hold there and wait for more frames rather than looping — see
        // AnimationPlayback::loop_when_done's doc comment.
        if !state.loop_when_done && state.current_frame == frame_count - 1 {
            return state.current_frame;
        }

        let now = Instant::now();
        match state.last_advance {
            None => state.last_advance = Some(now),
            Some(last) => {
                let elapsed = now - last;
                let frame_duration = Duration::from(self.render_image.delay(state.current_frame));
                if elapsed >= frame_duration {
                    let next_frame = state.current_frame + 1;
                    state.current_frame = if next_frame >= frame_count {
                        0 // only reachable when loop_when_done is true
                    } else {
                        next_frame
                    };
                    // Carry the remainder forward rather than resetting to
                    // `now` outright, so a frame with an unusually short
                    // gap doesn't get silently stretched to match however
                    // infrequently paint happens to run.
                    state.last_advance = Some(now - (elapsed - frame_duration));
                }
            },
        }
        self.animation.set(Some(state));
        state.current_frame
    }

    /// True while there's more than one frame and playback isn't paused —
    /// the signal `terminal_element.rs` uses to decide whether to call
    /// `window.request_animation_frame()` after painting this image, since
    /// nothing else would otherwise prompt GPUI to repaint on the next tick.
    pub fn is_animating(&self) -> bool {
        self.animation.get().is_some_and(|s| s.playing) && self.render_image.frame_count() > 1
    }
}

/// A `Place` command's data, keyed by `(image_id, placement_id)` in
/// [`ImageStore::placements`].
///
/// Grid-relative positioning (which row/column this is anchored to) isn't
/// tracked here as `anchor` — grid coordinates, not pixels, so this survives
/// a font-size/window resize unchanged (the pixel position is recomputed
/// from `anchor` + current `TerminalBounds` at paint time, not stored).
///
/// Deliberately typed as plain `(i32, usize)` rather than pulling in
/// `alacritty_terminal`'s `Point`/`Line`/`Column` types here — this module
/// otherwise has no dependency on `alacritty_terminal` at all, and staying
/// that way keeps it easy to unit-test without any `Term` scaffolding (see
/// this file's tests, none of which construct a real terminal). The caller
/// (`Terminal::process_event`, which already has `alacritty_terminal` types
/// in scope) converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementInfo {
    /// (line, column) the cursor was at when the `Place` command ran —
    /// Kitty's protocol anchors cursor-relative placements (as opposed to
    /// the Stage 4 unicode-placeholder mode) to wherever the cursor was at
    /// that moment, not to a fixed screen position.
    pub anchor: (i32, usize),
    pub z_index: i32,
    pub columns: Option<u32>,
    pub rows: Option<u32>,
}

/// In-progress state for an image whose `Transmit` command(s) haven't
/// finished arriving yet (`m=1`, more chunks pending).
struct PendingTransmit {
    format: ImageFormat,
    /// `s=`/`v=` from the FIRST chunk — continuation chunks are bare
    /// `m=1;<payload>` with no control keys at all, so the dimensions (like
    /// the format and id) have to be carried forward from where they were
    /// declared. Required to decode the raw pixel formats; see
    /// `decode_pixels`.
    width: Option<u32>,
    height: Option<u32>,
    /// Base64 text concatenated across every chunk seen so far — decoding
    /// (both the base64 layer and, for PNG, the image codec layer) only
    /// happens once the final chunk (`m=0`/absent) arrives, since a partial
    /// base64 string isn't valid base64 and a partial PNG isn't a valid PNG.
    payload_base64: String,
}

/// Animation frames accumulated so far for an image (`a=f` commands),
/// finalized into a fresh `DecodedImage`/`RenderImage` once building is
/// done — `RenderImage` has no "append a frame" operation (its `SmallVec`
/// of `image::Frame`s is fixed at construction, see `RenderImage::new`), so
/// frames have to collect here first rather than mutating the already-
/// stored image in place.
struct PendingFrames {
    /// The format every `a=f` payload for this image decodes as — Kitty's
    /// spec allows a frame to override `f=` independently of the base
    /// image, but this implementation (and its only real sender,
    /// `crates/somcat`) always reuses the base image's format, so that's
    /// the only case handled; see `apply_frame`.
    format: ImageFormat,
    width: Option<u32>,
    height: Option<u32>,
    /// One entry per completed frame so far, in arrival order — the base
    /// (`a=T`) frame is seeded in here first (see `apply_transmit`) so
    /// `finish_animation`/further `a=f` commands only ever have to append,
    /// never special-case "is this the first frame."
    frames: Vec<image::Frame>,
    /// Base64 text concatenated across a chunked frame's continuation APC
    /// strings (`m=1` on a `a=f`) — same accumulate-then-decode-once
    /// approach as `PendingTransmit::payload_base64`.
    in_progress_payload: String,
    /// The `z=` gap from THIS frame's very first chunk, remembered across
    /// however many continuation chunks follow. Per spec, only the first
    /// chunk of a multi-chunk `a=f` carries control keys at all —
    /// continuation and final chunks are bare `m=1;<payload>`/
    /// `m=0;<payload>`, so `FrameCommand::gap` on THOSE chunks is always
    /// `FrameGap::Default` (nothing to parse `z=` out of), never the
    /// sender's real value. Using the last-received chunk's `gap` directly
    /// (as opposed to this remembered one) would silently discard a real
    /// `z=` on every chunked frame — confirmed as a real bug found
    /// alongside the `Transmit`-misrouting fix in `ImageStore::apply`.
    in_progress_gap: Option<FrameGap>,
}

/// Per-terminal store of Kitty graphics protocol images and placements.
pub struct ImageStore {
    images: HashMap<u32, DecodedImage>,
    placements: HashMap<(u32, u32), PlacementInfo>,
    pending: HashMap<u32, PendingTransmit>,
    /// The id of whichever transmission most recently started (`m=1`) and
    /// hasn't finished (`m=0`) yet — needed because, PER THE OFFICIAL
    /// SPEC'S OWN REFERENCE SHELL-SCRIPT EXAMPLE
    /// (<https://sw.kovidgoyal.net/kitty/graphics-protocol/#the-transmission-medium>),
    /// only the FIRST chunk of a multi-chunk transmission carries `i=` at
    /// all — every later chunk is JUST `m=1;<payload>` or `m=0;<payload>`,
    /// with no id, no format, nothing else. Real, spec-compliant clients
    /// (this project's own `crates/somcat`, and presumably `yazi`/`kitten
    /// icat`) therefore never repeat `i=` on continuation chunks — a
    /// continuation chunk arriving with `transmit.id == None` must resume
    /// THIS pending transmission, not be silently discarded for "having no
    /// id to store it under" (an actual bug this field fixes — a chunk
    /// with no `i=` doesn't mean "anonymous image", it means "continue the
    /// most recent multi-chunk transmission"). `None` when no transmission
    /// is currently in progress.
    active_pending_id: Option<u32>,
    /// Animation frames accumulated so far per image id — lazily seeded
    /// (from the already-stored base image, see
    /// `seed_frames_from_stored_image`) on the FIRST `a=f` command for an
    /// id, appended to by every `a=f` after that, and consumed once `a=a`
    /// starts playback (`finish_pending_animation`). See `PendingFrames`'s
    /// doc comment for why frames can't just be appended onto the
    /// already-stored `DecodedImage` in place.
    pending_frames: HashMap<u32, PendingFrames>,
    /// Mirrors `active_pending_id` but for a chunked `a=f` frame transmission
    /// in progress — the same "only the first chunk carries `i=`" rule
    /// applies to frames as to `Transmit`.
    active_frame_id: Option<u32>,
    /// `(format, width, height)` a completed `a=T` transmission used to
    /// decode — remembered per image id so a later `a=f` (which, like a
    /// Transmit continuation chunk, may omit `f=`/`s=`/`v=` entirely) knows
    /// how to interpret its own payload without the sender repeating them.
    image_formats: HashMap<u32, (ImageFormat, Option<u32>, Option<u32>)>,
    /// The most recently animated image's id — `a=a`'s `i=` key is
    /// optional (Kitty's own clients often omit it, acting on "whichever
    /// image was most recently the target of an animation-related
    /// command"), so this is the fallback `apply_animation` resolves
    /// against when a command omits `i=`.
    last_animated_id: Option<u32>,
    total_bytes: usize,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self {
            images: HashMap::new(),
            placements: HashMap::new(),
            pending: HashMap::new(),
            active_pending_id: None,
            pending_frames: HashMap::new(),
            active_frame_id: None,
            image_formats: HashMap::new(),
            last_animated_id: None,
            total_bytes: 0,
        }
    }
}

impl ImageStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn image(&self, id: u32) -> Option<&DecodedImage> {
        self.images.get(&id)
    }

    pub fn placement(&self, id: u32, placement_id: u32) -> Option<&PlacementInfo> {
        self.placements.get(&(id, placement_id))
    }

    /// Every currently-live placement paired with its image data, for a
    /// renderer to walk and paint. Placements whose image was deleted (or
    /// never successfully decoded — see `apply_transmit`'s discard paths)
    /// are silently skipped rather than yielded with missing data, since
    /// there's nothing meaningful to paint for them.
    pub fn all_placements(
        &self,
    ) -> impl Iterator<Item = (u32, u32, &PlacementInfo, &DecodedImage)> {
        self.placements.iter().filter_map(move |(&(id, placement_id), info)| {
            self.images.get(&id).map(|image| (id, placement_id, info, image))
        })
    }

    /// Apply one parsed [`KittyGraphicsCommand`] to this store.
    ///
    /// Chunked transmissions (`m=1` on all but the last APC string) are
    /// handled transparently — a caller just feeds every `Transmit` command
    /// in the order its APC strings arrived; this accumulates base64 text
    /// across chunks under the hood and only decodes once complete.
    ///
    /// `cursor` is the grid position (line, column) the terminal's cursor
    /// is at right now — only consulted for `Place` commands (to anchor a
    /// cursor-relative placement), ignored otherwise. Pass the real current
    /// cursor position, not a stale/cached one: Kitty anchors a placement to
    /// wherever the cursor is at the moment `a=p` is processed, which can
    /// differ from where it was when the image was transmitted.
    pub fn apply(&mut self, command: KittyGraphicsCommand, cursor: (i32, usize)) {
        match command {
            // `kitty_graphics::parse_command` can't tell an action-less
            // continuation chunk (bare `m=1;<payload>`/`m=0;<payload>`,
            // `a=` entirely absent — legal per spec for ANY multi-chunk
            // command, not just `a=T`/`a=t`) apart from a genuine `a=t`
            // continuation: it's deliberately stateless and has no way to
            // know which multi-chunk transmission is actually in progress,
            // so it always guesses `Transmit` (see
            // `TransmitCommand::explicit_action`'s doc comment). This
            // store DOES track that state, so it's the one place that can
            // correct the guess: an action-less chunk arriving while an
            // `a=f` frame transmission is still open belongs to THAT frame,
            // not to `Transmit`. Without this correction, every animation
            // frame's continuation chunks silently vanished into
            // `apply_transmit` (which has no matching `active_pending_id`
            // for them and just drops the bytes) — confirmed as the actual
            // cause of a real animated GIF never finishing: `is_animating()`
            // stayed false forever because `pending_frames` only ever had
            // each frame's FIRST 4096-byte chunk, never the rest.
            KittyGraphicsCommand::Transmit(transmit)
                if !transmit.explicit_action && self.active_frame_id.is_some() =>
            {
                self.apply_frame(FrameCommand {
                    id: transmit.id,
                    gap: FrameGap::Default,
                    more_chunks: transmit.more_chunks,
                    payload_base64: transmit.payload_base64,
                });
            },
            KittyGraphicsCommand::Transmit(transmit) => self.apply_transmit(transmit),
            KittyGraphicsCommand::Place(place) => self.apply_place(place, cursor),
            KittyGraphicsCommand::Delete(delete) => self.apply_delete(delete),
            KittyGraphicsCommand::Frame(frame) => self.apply_frame(frame),
            KittyGraphicsCommand::Animation(animation) => self.apply_animation(animation),
            // Handled by the caller (`Terminal::process_event`), which owns
            // the PTY write side this store deliberately doesn't have — see
            // `crate::kitty_graphics::query_response`.
            KittyGraphicsCommand::Query(_) => {},
        }
    }

    fn apply_transmit(&mut self, transmit: TransmitCommand) {
        if transmit.medium != TransmissionMedium::Direct {
            // File/temp-file/shared-memory transmission — see
            // TransmissionMedium::Unsupported's doc comment for why this is
            // rejected rather than honored. Nothing to store.
            return;
        }

        // A chunk with no `i=` is NOT necessarily an anonymous image — per
        // the official spec's own reference shell-script example
        // (`send_chunked`/`transmit_png` at
        // https://sw.kovidgoyal.net/kitty/graphics-protocol/#the-transmission-medium),
        // only the FIRST chunk of a real multi-chunk transmission carries
        // `i=` at all; every continuation chunk is bare `m=1;<payload>` or
        // `m=0;<payload>`. Resolve such a chunk against whichever
        // transmission is currently in progress (see
        // `active_pending_id`'s doc comment) BEFORE falling back to
        // "no id at all, nothing to do with this" — that fallback is only
        // correct for a genuinely id-less, non-continuation Transmit.
        let id = match transmit.id.or(if transmit.more_chunks || self.active_pending_id.is_some() {
            self.active_pending_id
        } else {
            None
        }) {
            Some(id) => id,
            None => {
                // Anonymous images (referenced only by I=, the image
                // *number*, not i=) aren't modeled yet — see
                // TransmitCommand::id's doc comment. Without an id (and no
                // in-progress transmission to resume) there's no key to
                // store this under.
                return;
            },
        };

        // The format (f=) key is only meaningful on the FIRST chunk of a
        // transmission per the spec — later chunks may omit it (falling
        // back to the parser's default, RGBA), so decoding must use
        // whatever format the pending transmission was originally opened
        // with, not necessarily this specific chunk's `transmit.format`.
        let (format, width, height, full_base64) = if transmit.more_chunks {
            let pending = self.pending.entry(id).or_insert_with(|| PendingTransmit {
                format: transmit.format,
                width: transmit.width,
                height: transmit.height,
                payload_base64: String::new(),
            });
            pending.payload_base64.push_str(&transmit.payload_base64);
            self.active_pending_id = Some(id);
            return;
        } else if let Some(mut pending) = self.pending.remove(&id) {
            pending.payload_base64.push_str(&transmit.payload_base64);
            self.active_pending_id = None;
            (pending.format, pending.width, pending.height, pending.payload_base64)
        } else {
            self.active_pending_id = None;
            (
                transmit.format,
                transmit.width,
                transmit.height,
                transmit.payload_base64,
            )
        };

        let Ok(raw_bytes) = base64::engine::general_purpose::STANDARD.decode(&full_base64) else {
            log::debug!("kitty graphics: image {id} had invalid base64 payload, discarding");
            return;
        };

        let Some(buffer) = decode_pixels(format, &raw_bytes, width, height) else {
            log::debug!(
                "kitty graphics: image {id} ({format:?}, {} bytes) failed to decode, discarding",
                raw_bytes.len()
            );
            return;
        };

        // Remembered so a later `a=f` frame for this same id (which per
        // spec may omit `f=`/`s=`/`v=` entirely, just like a Transmit
        // continuation chunk) knows how to decode its payload — see
        // `apply_frame`.
        self.image_formats.insert(id, (format, width, height));
        self.insert_image(id, DecodedImage::from_single_frame(buffer));
    }

    fn apply_frame(&mut self, frame: FrameCommand) {
        let id = match frame.id.or(if frame.more_chunks || self.active_frame_id.is_some() {
            self.active_frame_id
        } else {
            None
        }) {
            Some(id) => id,
            None => return,
        };

        if !self.pending_frames.contains_key(&id) {
            let (format, width, height) =
                self.image_formats.get(&id).copied().unwrap_or((ImageFormat::Rgba, None, None));
            let frames = self.seed_frames_from_stored_image(id);
            self.pending_frames.insert(
                id,
                PendingFrames {
                    format,
                    width,
                    height,
                    frames,
                    in_progress_payload: String::new(),
                    in_progress_gap: None,
                },
            );
        }
        let pending = self.pending_frames.get_mut(&id).expect("just inserted above if absent");

        // The very first chunk of a new frame is the only one that can
        // carry a real `z=` (see `PendingFrames::in_progress_gap`'s doc
        // comment) — recognized here as "no payload accumulated yet for
        // this frame", independent of whether that first chunk itself
        // happens to be chunked further.
        if pending.in_progress_payload.is_empty() {
            pending.in_progress_gap = Some(frame.gap);
        }

        if frame.more_chunks {
            pending.in_progress_payload.push_str(&frame.payload_base64);
            self.active_frame_id = Some(id);
            return;
        }
        self.active_frame_id = None;

        let full_base64 = std::mem::take(&mut pending.in_progress_payload) + &frame.payload_base64;
        let gap = pending.in_progress_gap.take().unwrap_or(frame.gap);
        let Ok(raw_bytes) = base64::engine::general_purpose::STANDARD.decode(&full_base64) else {
            log::debug!("kitty graphics: image {id} animation frame had invalid base64 payload, discarding");
            return;
        };
        let Some(buffer) = decode_pixels(pending.format, &raw_bytes, pending.width, pending.height) else {
            log::debug!(
                "kitty graphics: image {id} animation frame ({:?}, {} bytes) failed to decode, discarding",
                pending.format,
                raw_bytes.len()
            );
            return;
        };

        pending.frames.push(Frame::from_parts(buffer, 0, 0, delay_from_gap(gap)));
    }

    /// The base image (already decoded and stored via `a=T`) becomes frame
    /// 0 of the animation the first time an `a=f` command arrives for its
    /// id — copied out of the already-decoded `RenderImage` rather than
    /// re-decoding the original transmit payload, since that's long gone
    /// by the time a frame command shows up.
    fn seed_frames_from_stored_image(&self, id: u32) -> Vec<Frame> {
        self.images
            .get(&id)
            .and_then(|image| image.render_image.as_bytes(0).map(|bytes| (bytes.to_vec(), image.render_image.size(0))))
            .map(|(bytes, size)| {
                let buffer = RgbaImage::from_raw(size.width.0 as u32, size.height.0 as u32, bytes)
                    .expect("stored RenderImage frame 0 byte length must match its own dimensions");
                vec![Frame::new(buffer)]
            })
            .unwrap_or_default()
    }

    fn apply_animation(&mut self, animation: AnimationCommand) {
        match animation {
            AnimationCommand::SetState { id, state } => {
                let Some(id) = id.or(self.last_animated_id) else { return };
                // Both Loading (s=2) and Running (s=3) actually play the
                // animation (they only differ in what happens once the
                // last transmitted frame is reached — loop vs wait for
                // more) — either one is the point at which whatever's
                // accumulated in `pending_frames` needs to become the real
                // multi-frame `RenderImage`. Stopped (s=1) never needs
                // this: a frame-less image was never getting animated in
                // the first place, and a genuinely mid-animation stop
                // shouldn't re-finalize (harmless no-op either way, since
                // `finish_pending_animation` only acts once there ARE
                // pending frames).
                if matches!(state, AnimationState::Loading | AnimationState::Running) {
                    self.finish_pending_animation(id);
                }
                if let Some(image) = self.images.get(&id) {
                    let mut playback = image.animation.get().unwrap_or(AnimationPlayback {
                        playing: false,
                        loop_when_done: false,
                        current_frame: 0,
                        last_advance: None,
                    });
                    playback.playing = !matches!(state, AnimationState::Stopped);
                    playback.loop_when_done = matches!(state, AnimationState::Running);
                    if playback.playing {
                        playback.last_advance = None;
                    }
                    image.animation.set(Some(playback));
                }
            },
            AnimationCommand::SetCurrentFrame { id, frame } => {
                let Some(id) = id.or(self.last_animated_id) else { return };
                self.finish_pending_animation(id);
                if let Some(image) = self.images.get(&id) {
                    let mut playback = image.animation.get().unwrap_or(AnimationPlayback {
                        playing: false,
                        loop_when_done: false,
                        current_frame: 0,
                        last_advance: None,
                    });
                    // 1-indexed per spec; out-of-range clamps to the last
                    // frame rather than panicking/wrapping unexpectedly.
                    let count = image.render_image.frame_count();
                    playback.current_frame = (frame.max(1) as usize - 1).min(count.saturating_sub(1));
                    playback.last_advance = None;
                    image.animation.set(Some(playback));
                }
            },
            AnimationCommand::SetFrameGap { id, frame, gap } => {
                let Some(id) = id.or(self.last_animated_id) else { return };
                // The root frame (frame 1) is the one case this can target
                // BEFORE any a=f ever arrived (the base a=T image has no
                // gap-setting z= of its own — see FrameCommand's doc
                // comment) — seed pending_frames from the stored image if
                // it isn't already in progress, same as apply_frame does,
                // so `a=a,i=N,r=1,z=<gap>` sent right after `a=T` (before
                // any real animation frame) has a frame 1 to set the gap
                // on.
                if !self.pending_frames.contains_key(&id) {
                    let (format, width, height) =
                        self.image_formats.get(&id).copied().unwrap_or((ImageFormat::Rgba, None, None));
                    let frames = self.seed_frames_from_stored_image(id);
                    if frames.is_empty() {
                        return;
                    }
                    self.pending_frames.insert(
                        id,
                        PendingFrames {
                            format,
                            width,
                            height,
                            frames,
                            in_progress_payload: String::new(),
                            in_progress_gap: None,
                        },
                    );
                }
                let Some(pending) = self.pending_frames.get_mut(&id) else { return };
                let Some(index) = (frame.max(1) as usize).checked_sub(1) else { return };
                if let Some(existing) = pending.frames.get_mut(index) {
                    let buffer = existing.buffer().clone();
                    *existing = Frame::from_parts(buffer, 0, 0, delay_from_gap(gap));
                }
            },
        }
    }

    /// Finalizes whatever frames have accumulated in `pending_frames` for
    /// `id` into a brand new `DecodedImage`, replacing the single-frame one
    /// `a=T` originally stored — a no-op if no `a=f` commands ever arrived
    /// for this id (the common, non-animated case).
    fn finish_pending_animation(&mut self, id: u32) {
        let Some(pending) = self.pending_frames.remove(&id) else { return };
        if pending.frames.len() < 2 {
            // Only the seeded base frame, no real animation frames landed —
            // leave the existing single-frame image alone.
            return;
        }
        self.last_animated_id = Some(id);
        self.insert_image(id, DecodedImage::from_frames(pending.frames));
    }

    fn insert_image(&mut self, id: u32, image: DecodedImage) {
        log::trace!(
            "kitty graphics: storing image {id} ({} bytes decoded)",
            image.byte_size
        );
        // Replacing an existing image (same id transmitted again) must
        // account for the old one leaving the budget, not just the new one
        // entering it — otherwise repeated re-transmission of the same id
        // would look like ever-growing usage even though only one copy is
        // ever actually held.
        if let Some(old) = self.images.remove(&id) {
            self.total_bytes = self.total_bytes.saturating_sub(old.byte_size);
        }

        if self.total_bytes.saturating_add(image.byte_size) > MAX_STORE_BYTES {
            log::warn!(
                "kitty graphics: rejecting image {id} ({} bytes) — would exceed the {MAX_STORE_BYTES}-byte store budget ({} already used)",
                image.byte_size,
                self.total_bytes
            );
            return;
        }

        self.total_bytes += image.byte_size;
        self.images.insert(id, image);
    }

    fn apply_place(&mut self, place: PlaceCommand, cursor: (i32, usize)) {
        let (Some(id), Some(placement_id)) = (place.id, place.placement_id) else {
            // A placement with no explicit id/placement_id defaults to
            // "the most recently transmitted image" per spec — anonymous-
            // image tracking isn't modeled yet (see TransmitCommand::id's
            // doc comment), so there's no way to resolve which image this
            // would refer to. Nothing to record here.
            return;
        };
        self.placements.insert(
            (id, placement_id),
            PlacementInfo {
                anchor: cursor,
                z_index: place.z_index,
                columns: place.columns,
                rows: place.rows,
            },
        );
    }

    fn apply_delete(&mut self, delete: DeleteCommand) {
        match delete {
            DeleteCommand::All { free_data } => {
                self.placements.clear();
                if free_data {
                    self.images.clear();
                    self.total_bytes = 0;
                }
            },
            DeleteCommand::ById { id, free_data } => {
                self.placements.retain(|&(img_id, _), _| img_id != id);
                if free_data && let Some(image) = self.images.remove(&id) {
                    self.total_bytes = self.total_bytes.saturating_sub(image.byte_size);
                }
            },
            DeleteCommand::ByPlacement { id, placement_id, free_data } => {
                self.placements.remove(&(id, placement_id));
                // A placement-specific delete only frees the underlying
                // image data if no OTHER placement still references it —
                // otherwise a still-visible placement would go dark.
                let still_referenced = self.placements.keys().any(|&(img_id, _)| img_id == id);
                if free_data
                    && !still_referenced
                    && let Some(image) = self.images.remove(&id)
                {
                    self.total_bytes = self.total_bytes.saturating_sub(image.byte_size);
                }
            },
        }
    }
}

/// Decode raw (post-base64) image bytes into pixel data, already converted
/// to the BGRA byte order `gpui::RenderImage` expects — mirrors the same
/// RGBA→BGRA byte-swap `gpui::platform`'s clipboard-image decoding does
/// (`pixel.swap(0, 2)` per pixel), since `image::Frame`/`RgbaImage` only
/// ever produce RGBA.
fn decode_pixels(
    format: ImageFormat,
    bytes: &[u8],
    width: Option<u32>,
    height: Option<u32>,
) -> Option<RgbaImage> {
    match format {
        ImageFormat::Png => {
            let mut buffer =
                image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
                    .ok()?
                    .into_rgba8();
            swap_to_bgra(&mut buffer);
            Some(buffer)
        },
        ImageFormat::Rgb | ImageFormat::Rgba => {
            // Raw formats carry no width/height in the pixel data itself, so
            // the sender has to supply them via the `s=`/`v=` control keys
            // (see `TransmitCommand::width`). Without them there's no way to
            // slice the flat byte array into rows — reject rather than guess.
            let (width, height) = (width?, height?);
            let source_channels = match format {
                ImageFormat::Rgb => 3usize,
                _ => 4usize,
            };
            let expected = (width as usize)
                .checked_mul(height as usize)?
                .checked_mul(source_channels)?;
            // Trailing slack is tolerated (a sender may pad its final chunk),
            // but a short payload means the image genuinely didn't all
            // arrive, and laying it out would read past the end.
            if bytes.len() < expected {
                return None;
            }

            let mut buffer = RgbaImage::new(width, height);
            for (destination, source) in buffer
                .as_flat_samples_mut()
                .samples
                .chunks_exact_mut(4)
                .zip(bytes[..expected].chunks_exact(source_channels))
            {
                // Written straight out as BGRA (the order
                // `gpui::RenderImage` wants) rather than building RGBA and
                // swapping afterwards like the PNG path has to.
                destination[0] = source[2];
                destination[1] = source[1];
                destination[2] = source[0];
                destination[3] = if source_channels == 4 { source[3] } else { 0xFF };
            }
            Some(buffer)
        },
    }
}

fn swap_to_bgra(buffer: &mut RgbaImage) {
    for pixel in buffer.as_flat_samples_mut().samples.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty_graphics::{self};

    /// A minimal valid 1x1 white PNG, base64-encoded — small enough to
    /// embed directly in test source rather than reading a fixture file.
    /// Generated via `image::ImageBuffer::from_pixel(1, 1, Rgba([255; 4]))`
    /// + `write_to(..., ImageFormat::Png)`, then base64-encoded — verified
    /// round-trippable through `image::load_from_memory_with_format`
    /// before being pasted here (a hand-typed/AI-recalled PNG byte string
    /// is NOT safe to trust — PNG's IDAT chunks are CRC-checked, and a
    /// single wrong byte makes the whole thing invalid in a way that's
    /// easy to not notice until a decode test actually runs it).
    const TINY_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAEElEQVR4AQEFAPr/AP////8J+wP9o9FJCgAAAABJRU5ErkJggg==";

    fn transmit_command(bytes: &[u8]) -> KittyGraphicsCommand {
        kitty_graphics::parse_command(bytes).expect("test fixture must parse")
    }

    #[test]
    fn transmit_and_decode_png() {
        let mut store = ImageStore::new();
        let apc = format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}");
        store.apply(transmit_command(apc.as_bytes()), (0, 0));

        let image = store.image(1).expect("image 1 should be stored");
        assert_eq!(image.render_image.frame_count(), 1);
        let size = image.render_image.size(0);
        assert_eq!((size.width.0, size.height.0), (1, 1));
    }

    #[test]
    fn transmit_and_decode_raw_rgba_the_way_yazi_sends_it() {
        // `yazi` transmits raw pixels (`f=32` RGBA / `f=24` RGB) with the
        // dimensions in `s=`/`v=`, NOT PNG — see
        // `yazi-adapter/src/drivers/kgp.rs`. Those keys used to be dropped
        // during parsing, which made `decode_pixels` bail out for every raw
        // transmission and left yazi's previews permanently blank.
        use base64::Engine as _;

        // 2x1 RGBA: one opaque red pixel, one half-transparent blue pixel.
        let raw: [u8; 8] = [255, 0, 0, 255, 0, 0, 255, 128];
        let payload = base64::engine::general_purpose::STANDARD.encode(raw);
        let apc = format!("Ga=T,f=32,s=2,v=1,i=9;{payload}");
        let mut store = ImageStore::new();
        store.apply(transmit_command(apc.as_bytes()), (0, 0));

        let image = store.image(9).expect("raw RGBA image should decode and be stored");
        let size = image.render_image.size(0);
        assert_eq!((size.width.0, size.height.0), (2, 1), "s=/v= give the dimensions");

        // Stored as BGRA (what `gpui::RenderImage` renders), so the red
        // pixel's blue and red channels are swapped relative to the input.
        let bytes = image.render_image.as_bytes(0).expect("frame 0 must have pixels");
        assert_eq!(&bytes[0..4], &[0, 0, 255, 255], "red RGBA -> BGRA");
        assert_eq!(&bytes[4..8], &[255, 0, 0, 128], "blue RGBA with alpha -> BGRA");
    }

    #[test]
    fn transmit_and_decode_raw_rgb_fills_in_opaque_alpha() {
        use base64::Engine as _;

        // 1x1 RGB (no alpha channel in the source at all).
        let raw: [u8; 3] = [10, 20, 30];
        let payload = base64::engine::general_purpose::STANDARD.encode(raw);
        let apc = format!("Ga=T,f=24,s=1,v=1,i=11;{payload}");
        let mut store = ImageStore::new();
        store.apply(transmit_command(apc.as_bytes()), (0, 0));

        let image = store.image(11).expect("raw RGB image should decode and be stored");
        let bytes = image.render_image.as_bytes(0).expect("frame 0 must have pixels");
        assert_eq!(
            &bytes[0..4],
            &[30, 20, 10, 255],
            "RGB -> BGRA with alpha forced fully opaque"
        );
    }

    #[test]
    fn raw_transmission_without_dimensions_is_discarded() {
        use base64::Engine as _;

        let payload = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        let apc = format!("Ga=T,f=32,i=13;{payload}");
        let mut store = ImageStore::new();
        store.apply(transmit_command(apc.as_bytes()), (0, 0));

        assert!(
            store.image(13).is_none(),
            "raw pixel data with no s=/v= can't be laid out into rows, so it must be rejected \
             rather than guessed at"
        );
    }

    #[test]
    fn raw_transmission_shorter_than_its_declared_size_is_discarded() {
        use base64::Engine as _;

        // Declares 4x4 RGBA (64 bytes) but only supplies 8.
        let payload = base64::engine::general_purpose::STANDARD.encode([0u8; 8]);
        let apc = format!("Ga=T,f=32,s=4,v=4,i=15;{payload}");
        let mut store = ImageStore::new();
        store.apply(transmit_command(apc.as_bytes()), (0, 0));

        assert!(
            store.image(15).is_none(),
            "a truncated payload must be rejected, not read past the end of"
        );
    }

    #[test]
    fn chunked_raw_rgba_keeps_the_first_chunks_dimensions() {
        // The dimensions, like the format and id, only appear on the first
        // chunk — every continuation chunk yazi sends is a bare
        // `m=1;<payload>`. Losing them mid-transmission would break exactly
        // the multi-chunk case every real image hits.
        use base64::Engine as _;

        let raw: Vec<u8> = (0..2 * 2 * 4).map(|i| i as u8).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let (first, second) = encoded.split_at(encoded.len() / 2);

        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=32,s=2,v=2,i=17,m=1;{first}").as_bytes()),
            (0, 0),
        );
        store.apply(transmit_command(format!("Gm=0;{second}").as_bytes()), (0, 0));

        let image = store.image(17).expect("chunked raw RGBA should decode");
        let size = image.render_image.size(0);
        assert_eq!((size.width.0, size.height.0), (2, 2));
    }

    #[test]
    fn invalid_base64_is_discarded_not_panicking() {
        let mut store = ImageStore::new();
        store.apply(transmit_command(b"Ga=T,f=100,i=1;not valid base64!!!"), (0, 0));
        assert!(store.image(1).is_none());
    }

    #[test]
    fn invalid_png_bytes_are_discarded_not_panicking() {
        let mut store = ImageStore::new();
        // Valid base64, but decodes to bytes that aren't a valid PNG.
        let apc = "Ga=T,f=100,i=1;AAAAAAAA";
        store.apply(transmit_command(apc.as_bytes()), (0, 0));
        assert!(store.image(1).is_none());
    }

    #[test]
    fn chunked_transmission_concatenates_before_decoding() {
        let mut store = ImageStore::new();
        let half = TINY_PNG_BASE64.len() / 2;
        let (first_half, second_half) = TINY_PNG_BASE64.split_at(half);

        let first_apc = format!("Ga=T,f=100,i=1,m=1;{first_half}");
        store.apply(transmit_command(first_apc.as_bytes()), (0, 0));
        // Nothing decoded yet — still waiting on the final chunk.
        assert!(store.image(1).is_none());

        let last_apc = format!("Ga=T,f=100,i=1,m=0;{second_half}");
        store.apply(transmit_command(last_apc.as_bytes()), (0, 0));
        assert!(store.image(1).is_some());
    }

    #[test]
    fn chunked_transmission_works_when_continuation_chunks_omit_the_id() {
        // Regression test for a REAL bug (found 2026-07-25 via a real
        // `somcat`/large-photo test, see project_kitty_graphics_protocol
        // memory) — `chunked_transmission_concatenates_before_decoding`
        // above (and every other pre-existing chunk test) manually
        // repeats `i=1` on EVERY chunk, which is NOT what the official
        // spec's own reference shell-script implementation
        // (`send_chunked`/`transmit_png` at
        // https://sw.kovidgoyal.net/kitty/graphics-protocol/#the-transmission-medium)
        // actually does: `i=`/`f=` are ONLY ever sent on the FIRST chunk;
        // every continuation chunk is bare `m=1;<payload>`/`m=0;<payload>`
        // with no id at all. Before the fix, `apply_transmit` bailed out
        // immediately on any Transmit with `id: None` — silently
        // discarding every id-less continuation chunk, which corrupted
        // EVERY real multi-chunk transmission from a spec-compliant
        // client (this project's own `crates/somcat`, presumably also
        // `yazi`/`kitten icat`) despite the OTHER (unrealistic) chunk
        // tests passing.
        let mut store = ImageStore::new();
        let half = TINY_PNG_BASE64.len() / 2;
        let (first_half, second_half) = TINY_PNG_BASE64.split_at(half);

        let first_apc = format!("Ga=T,f=100,i=1,m=1;{first_half}");
        store.apply(transmit_command(first_apc.as_bytes()), (0, 0));
        assert!(store.image(1).is_none());

        // The final chunk deliberately has NO `i=` at all — matching the
        // official spec's own reference implementation, not a hand-picked
        // edge case.
        let last_apc = format!("Ga=T,m=0;{second_half}");
        store.apply(transmit_command(last_apc.as_bytes()), (0, 0));
        assert!(
            store.image(1).is_some(),
            "an id-less continuation chunk must resume the in-progress transmission, not be discarded"
        );
    }

    #[test]
    fn three_chunks_all_omitting_id_after_the_first_still_concatenate_correctly() {
        // Same bug, more chunks — makes sure `active_pending_id` survives
        // across more than one id-less continuation, not just exactly
        // one.
        let mut store = ImageStore::new();
        let third = TINY_PNG_BASE64.len() / 3;
        let (first, rest) = TINY_PNG_BASE64.split_at(third);
        let (second, third_part) = rest.split_at(third);

        store.apply(transmit_command(format!("Ga=T,f=100,i=7,m=1;{first}").as_bytes()), (0, 0));
        assert!(store.image(7).is_none());
        store.apply(transmit_command(format!("Ga=T,m=1;{second}").as_bytes()), (0, 0));
        assert!(store.image(7).is_none());
        store.apply(transmit_command(format!("Ga=T,m=0;{third_part}").as_bytes()), (0, 0));
        assert!(store.image(7).is_some());
    }

    #[test]
    fn chunked_transmission_uses_first_chunks_format_when_later_chunks_omit_it() {
        // Per spec, f= is only meaningful on the first chunk — later chunks
        // may (and commonly do) omit it. If the final chunk's bare "a=T,i=1,m=0"
        // were misread as defaulting to f=32 (RGBA) instead of remembering
        // the PNG format from chunk one, decoding would fail (raw RGBA
        // decode isn't even implemented — see decode_pixels).
        let mut store = ImageStore::new();
        let half = TINY_PNG_BASE64.len() / 2;
        let (first_half, second_half) = TINY_PNG_BASE64.split_at(half);

        store.apply(transmit_command(
            format!("Ga=T,f=100,i=1,m=1;{first_half}").as_bytes(),
        ), (0, 0));
        // Final chunk deliberately omits f= entirely.
        store.apply(transmit_command(format!("Ga=T,i=1,m=0;{second_half}").as_bytes()), (0, 0));

        assert!(
            store.image(1).is_some(),
            "must decode using chunk one's PNG format, not the final chunk's default RGBA"
        );
    }

    #[test]
    fn retransmitting_same_id_replaces_without_double_counting_budget() {
        let mut store = ImageStore::new();
        let apc = format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}");
        store.apply(transmit_command(apc.as_bytes()), (0, 0));
        let first_total = store.total_bytes;
        assert!(first_total > 0);

        store.apply(transmit_command(apc.as_bytes()), (0, 0));
        assert_eq!(
            store.total_bytes, first_total,
            "re-transmitting the same id must not double-count its bytes toward the budget"
        );
    }

    #[test]
    fn oversized_image_is_rejected() {
        // A store with a budget too small to hold even a 1x1 RGBA image
        // (4 bytes) must reject it rather than silently exceeding budget.
        let mut store = ImageStore { total_bytes: MAX_STORE_BYTES, ..ImageStore::new() };
        let apc = format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}");
        store.apply(transmit_command(apc.as_bytes()), (0, 0));
        assert!(store.image(1).is_none());
    }

    #[test]
    fn place_records_placement_info() {
        let mut store = ImageStore::new();
        store.apply(transmit_command(
            format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}").as_bytes(),
        ), (0, 0));
        // Cursor at (line 7, column 3) when the Place command runs.
        store.apply(transmit_command(b"Ga=p,i=1,p=1,z=-1,c=10,r=5"), (7, 3));

        let placement = store.placement(1, 1).expect("placement should be recorded");
        assert_eq!(placement.anchor, (7, 3), "placement must anchor to the cursor position AT PLACE TIME");
        assert_eq!(placement.z_index, -1);
        assert_eq!(placement.columns, Some(10));
        assert_eq!(placement.rows, Some(5));
    }

    #[test]
    fn place_anchors_to_cursor_at_place_time_not_transmit_time() {
        // The cursor may move between when an image is transmitted and when
        // it's placed — the anchor must reflect PLACE time, not transmit time.
        let mut store = ImageStore::new();
        store.apply(transmit_command(
            format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}").as_bytes(),
        ), (0, 0));
        store.apply(transmit_command(b"Ga=p,i=1,p=1"), (12, 40));

        let placement = store.placement(1, 1).unwrap();
        assert_eq!(placement.anchor, (12, 40));
    }

    #[test]
    fn delete_all_keeps_data_by_default() {
        let mut store = ImageStore::new();
        store.apply(transmit_command(
            format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}").as_bytes(),
        ), (0, 0));
        store.apply(transmit_command(b"Ga=p,i=1,p=1"), (0, 0));

        store.apply(transmit_command(b"Ga=d"), (0, 0));
        assert!(store.placement(1, 1).is_none(), "placements should be cleared");
        assert!(store.image(1).is_some(), "image data should survive a lowercase delete-all");
    }

    #[test]
    fn delete_all_uppercase_frees_data() {
        let mut store = ImageStore::new();
        store.apply(transmit_command(
            format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}").as_bytes(),
        ), (0, 0));
        store.apply(transmit_command(b"Ga=d,d=A"), (0, 0));
        assert!(store.image(1).is_none());
        assert_eq!(store.total_bytes, 0);
    }

    #[test]
    fn delete_by_placement_keeps_data_if_other_placements_remain() {
        let mut store = ImageStore::new();
        store.apply(transmit_command(
            format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}").as_bytes(),
        ), (0, 0));
        store.apply(transmit_command(b"Ga=p,i=1,p=1"), (0, 0));
        store.apply(transmit_command(b"Ga=p,i=1,p=2"), (0, 0));

        store.apply(transmit_command(b"Ga=d,d=P,i=1,p=1"), (0, 0));
        assert!(store.placement(1, 1).is_none());
        assert!(store.placement(1, 2).is_some());
        assert!(
            store.image(1).is_some(),
            "image data must survive while another placement still references it"
        );
    }

    #[test]
    fn delete_by_placement_uppercase_frees_data_once_last_reference_gone() {
        let mut store = ImageStore::new();
        store.apply(transmit_command(
            format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}").as_bytes(),
        ), (0, 0));
        store.apply(transmit_command(b"Ga=p,i=1,p=1"), (0, 0));
        store.apply(transmit_command(b"Ga=p,i=1,p=2"), (0, 0));

        // Freeing placement 1 (uppercase P) must NOT free image data yet —
        // placement 2 still references it.
        store.apply(transmit_command(b"Ga=d,d=P,i=1,p=1"), (0, 0));
        assert!(store.placement(1, 1).is_none());
        assert!(store.image(1).is_some(), "placement 2 still references image 1");

        // Freeing placement 2 (the last remaining reference) must now free it.
        store.apply(transmit_command(b"Ga=d,d=P,i=1,p=2"), (0, 0));
        assert!(store.placement(1, 2).is_none());
        assert!(store.image(1).is_none(), "no placements reference image 1 anymore");
        assert_eq!(store.total_bytes, 0);
    }

    #[test]
    fn delete_by_placement_lowercase_never_frees_data() {
        let mut store = ImageStore::new();
        store.apply(transmit_command(
            format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}").as_bytes(),
        ), (0, 0));
        store.apply(transmit_command(b"Ga=p,i=1,p=1"), (0, 0));

        // Lowercase d=p: even with zero remaining placements, data is kept.
        store.apply(transmit_command(b"Ga=d,d=p,i=1,p=1"), (0, 0));
        assert!(store.placement(1, 1).is_none());
        assert!(store.image(1).is_some(), "lowercase d=p must never free image data");
    }

    /// One 1x1 raw-RGBA pixel, base64-encoded — used as an animation frame
    /// payload since raw RGBA needs no PNG container, just `s=1,v=1`
    /// dimensions carried alongside it (see `decode_pixels`'s raw-format
    /// path).
    fn rgba_pixel_base64(r: u8, g: u8, b: u8) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode([r, g, b, 255])
    }

    #[test]
    fn animation_frame_appended_and_started_produces_multi_frame_image() {
        let mut store = ImageStore::new();
        // Base image (frame 0): opaque red.
        let base = rgba_pixel_base64(255, 0, 0);
        store.apply(transmit_command(format!("Ga=T,f=32,s=1,v=1,i=1;{base}").as_bytes()), (0, 0));
        assert_eq!(store.image(1).unwrap().render_image.frame_count(), 1);

        // One extra animation frame: opaque green, 50ms gap.
        let green = rgba_pixel_base64(0, 255, 0);
        store.apply(transmit_command(format!("Ga=f,i=1,z=50;{green}").as_bytes()), (0, 0));
        // Not finalized into the RenderImage until a=a runs playback.
        assert_eq!(store.image(1).unwrap().render_image.frame_count(), 1);

        // s=3: run normally (loop) — see kitty_graphics::AnimationState's
        // doc comment for why s=1 (which used to be misread as "start"
        // here) actually means stop.
        store.apply(transmit_command(b"Ga=a,i=1,s=3"), (0, 0));
        let image = store.image(1).expect("image must still be present after animation starts");
        assert_eq!(image.render_image.frame_count(), 2, "base frame + one a=f frame");
        assert!(image.is_animating(), "s=3 must start looping playback");

        // Frame 0 is the original red pixel, frame 1 the appended green one.
        assert_eq!(&image.render_image.as_bytes(0).unwrap()[0..4], &[0, 0, 255, 255], "red -> BGRA");
        assert_eq!(&image.render_image.as_bytes(1).unwrap()[0..4], &[0, 255, 0, 255], "green -> BGRA");
    }

    #[test]
    fn animation_bare_a_with_no_s_c_or_r_is_a_no_op_and_never_finalizes_frames() {
        // Regression guard for the exact bug that made animation appear
        // completely non-functional in a real client test: a prior,
        // unverified implementation treated bare "a=a" (no s=) as "start",
        // when per spec it's a no-op (s defaults to 0, "don't change
        // state") — frames must stay pending, never becoming visible.
        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=32,s=1,v=1,i=1;{}", rgba_pixel_base64(255, 0, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(
            transmit_command(format!("Ga=f,i=1,z=50;{}", rgba_pixel_base64(0, 255, 0)).as_bytes()),
            (0, 0),
        );
        // Bare a=a (no s=/c=/r=) must fail to parse as a command at all
        // (see kitty_graphics::tests::parse_animation_bare_with_no_s_c_or_r_is_a_no_op),
        // so there is nothing to feed into `apply` here in the first
        // place — this test documents that expectation from the store's
        // side: nothing must have changed.
        assert_eq!(store.image(1).unwrap().render_image.frame_count(), 1, "frame must still be pending");
        assert!(!store.image(1).unwrap().is_animating());
    }

    #[test]
    fn animation_s1_stops_not_starts() {
        // The exact inversion bug: s=1 must STOP, never start, playback.
        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=32,s=1,v=1,i=1;{}", rgba_pixel_base64(255, 0, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(
            transmit_command(format!("Ga=f,i=1,z=50;{}", rgba_pixel_base64(0, 255, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(transmit_command(b"Ga=a,i=1,s=1"), (0, 0));
        // s=1 alone never even has pending frames to finalize (only
        // Loading/Running do that) — the image must still be the
        // single-frame original, definitely not animating.
        assert_eq!(store.image(1).unwrap().render_image.frame_count(), 1);
        assert!(!store.image(1).unwrap().is_animating());
    }

    #[test]
    fn animation_s2_loading_mode_holds_on_last_frame_instead_of_looping() {
        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=32,s=1,v=1,i=1;{}", rgba_pixel_base64(255, 0, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(
            transmit_command(format!("Ga=f,i=1,z=1;{}", rgba_pixel_base64(0, 255, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(transmit_command(b"Ga=a,i=1,s=2"), (0, 0));
        let image = store.image(1).unwrap();
        assert_eq!(image.render_image.frame_count(), 2, "s=2 must also finalize pending frames");
        assert!(image.is_animating());

        // The very first current_frame() call after playback starts only
        // establishes the elapsed-time baseline (last_advance was None) —
        // it doesn't itself advance the frame, mirroring
        // gpui::elements::img's own ImgState::last_frame_time behavior.
        assert_eq!(image.current_frame(), 0);

        // Advance past frame 0's 1ms gap — must land on frame 1 (the
        // last frame) and then STAY there rather than wrapping back to 0,
        // since loading mode never loops.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(image.current_frame(), 1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(image.current_frame(), 1, "loading mode must hold on the last frame, not loop");
    }

    #[test]
    fn animation_multiple_frames_preserve_order_and_gap() {
        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=32,s=1,v=1,i=1;{}", rgba_pixel_base64(255, 0, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(
            transmit_command(format!("Ga=f,i=1,z=30;{}", rgba_pixel_base64(0, 255, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(
            transmit_command(format!("Ga=f,i=1,z=70;{}", rgba_pixel_base64(0, 0, 255)).as_bytes()),
            (0, 0),
        );
        store.apply(transmit_command(b"Ga=a,i=1,s=3"), (0, 0));

        let image = store.image(1).unwrap();
        assert_eq!(image.render_image.frame_count(), 3);
        assert_eq!(&image.render_image.as_bytes(2).unwrap()[0..4], &[255, 0, 0, 255], "blue -> BGRA");
        assert_eq!(
            std::time::Duration::from(image.render_image.delay(1)),
            std::time::Duration::from_millis(30)
        );
        assert_eq!(
            std::time::Duration::from(image.render_image.delay(2)),
            std::time::Duration::from_millis(70)
        );
    }

    #[test]
    fn animation_root_frame_gap_set_via_r1_and_z_before_any_a_f() {
        // The ONLY way to give the root frame (from a=T, which has its own
        // unrelated z= meaning z-index) a nonzero gap — spec's own
        // example is a=a,i=N,r=1,z=<gap>, usable even before any a=f frame
        // has arrived (this seeds pending_frames from the stored image,
        // see apply_animation's SetFrameGap arm).
        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=32,s=1,v=1,i=1;{}", rgba_pixel_base64(255, 0, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(transmit_command(b"Ga=a,i=1,r=1,z=48"), (0, 0));
        store.apply(
            transmit_command(format!("Ga=f,i=1,z=10;{}", rgba_pixel_base64(0, 255, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(transmit_command(b"Ga=a,i=1,s=3"), (0, 0));

        let image = store.image(1).unwrap();
        assert_eq!(image.render_image.frame_count(), 2);
        assert_eq!(
            std::time::Duration::from(image.render_image.delay(0)),
            std::time::Duration::from_millis(48),
            "root frame's gap must be settable even though a=T's own z= means something else"
        );
    }

    #[test]
    fn single_frame_image_never_animates() {
        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=100,i=1;{TINY_PNG_BASE64}").as_bytes()),
            (0, 0),
        );
        let image = store.image(1).unwrap();
        assert!(!image.is_animating());
        assert_eq!(image.current_frame(), 0);
    }

    #[test]
    fn animation_stop_pauses_on_current_frame() {
        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=32,s=1,v=1,i=1;{}", rgba_pixel_base64(255, 0, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(
            transmit_command(format!("Ga=f,i=1,z=10;{}", rgba_pixel_base64(0, 255, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(transmit_command(b"Ga=a,i=1,s=3"), (0, 0));
        assert!(store.image(1).unwrap().is_animating());

        store.apply(transmit_command(b"Ga=a,i=1,s=1"), (0, 0));
        assert!(!store.image(1).unwrap().is_animating(), "s=1 must stop playback");
    }

    #[test]
    fn animation_set_current_frame_via_c_jumps_without_resuming_playback() {
        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=32,s=1,v=1,i=1;{}", rgba_pixel_base64(255, 0, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(
            transmit_command(format!("Ga=f,i=1,z=10;{}", rgba_pixel_base64(0, 255, 0)).as_bytes()),
            (0, 0),
        );
        store.apply(transmit_command(b"Ga=a,i=1,s=3"), (0, 0));
        store.apply(transmit_command(b"Ga=a,i=1,s=1"), (0, 0)); // stop first

        // c=2 -> jump to frame index 1 (1-indexed in the protocol).
        store.apply(transmit_command(b"Ga=a,i=1,c=2"), (0, 0));
        let image = store.image(1).unwrap();
        assert!(!image.is_animating(), "set-current-frame must not resume playback on its own");
        assert_eq!(image.current_frame(), 1);
    }

    #[test]
    fn chunked_animation_frame_concatenates_before_decoding() {
        let mut store = ImageStore::new();
        store.apply(
            transmit_command(format!("Ga=T,f=32,s=1,v=1,i=1;{}", rgba_pixel_base64(255, 0, 0)).as_bytes()),
            (0, 0),
        );

        let green = rgba_pixel_base64(0, 255, 0);
        let half = green.len() / 2;
        let (first_half, second_half) = green.split_at(half);
        store.apply(transmit_command(format!("Ga=f,i=1,m=1;{first_half}").as_bytes()), (0, 0));
        store.apply(transmit_command(format!("Ga=f,m=0;{second_half}").as_bytes()), (0, 0));

        store.apply(transmit_command(b"Ga=a,i=1,s=3"), (0, 0));
        let image = store.image(1).unwrap();
        assert_eq!(image.render_image.frame_count(), 2);
        assert_eq!(&image.render_image.as_bytes(1).unwrap()[0..4], &[0, 255, 0, 255]);
    }
}
