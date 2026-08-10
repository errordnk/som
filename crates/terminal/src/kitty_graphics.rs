//! Parser for the Kitty terminal graphics protocol's control-data format.
//!
//! Spec: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>. This module
//! only turns raw APC bytes (everything between `ESC _ G` and the string
//! terminator, as delivered by `Term::apc_buffer` — see the `errordnk/vte`/
//! `errordnk/alacritty` forks' `apc_hook`/`apc_put`/`apc_unhook`) into a
//! structured [`KittyGraphicsCommand`]. It does not decode image data, keep
//! any state across chunks, or touch the terminal grid — see
//! `KITTY_GRAPHICS_PLAN.md` Stage 2 onward for those.

use std::collections::HashMap;

/// One fully-parsed Kitty graphics protocol command.
///
/// Deliberately does not attempt to model every control key from the spec —
/// only what Stage 1's scope covers (transmit, place, delete, query). Fields
/// not relevant to a given `a=` action are simply absent from that variant
/// rather than present-but-unused, so a caller can't accidentally read a
/// meaningless field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyGraphicsCommand {
    /// `a=T` (transmit and display) or bare transmit-only forms. Carries
    /// the still-base64-encoded payload — decoding happens in Stage 2, not
    /// here, since a chunked transmission (`m=1`) needs its payload
    /// concatenated across multiple APC strings before it's valid base64.
    Transmit(TransmitCommand),
    /// `a=p` — display an already-transmitted image.
    Place(PlaceCommand),
    /// `a=d` — delete image(s)/placement(s).
    Delete(DeleteCommand),
    /// `a=q` — capability query. A terminal that understands the protocol
    /// at all must answer (see `KITTY_GRAPHICS_PLAN.md` Stage 5) by
    /// echoing back whichever identifiers the sender used.
    Query(QueryCommand),
    /// `a=f` — append an animation frame to an already-transmitted image.
    /// Same chunked-payload shape as `Transmit` (an `m=1` sequence of
    /// bare-payload continuation APC strings can follow), which is why
    /// this carries its own `more_chunks`/`payload_base64` rather than
    /// reusing `TransmitCommand` wholesale — a frame has no `f=`/medium of
    /// its own beyond what the base image already established.
    Frame(FrameCommand),
    /// `a=a` — animation control (start/stop looping, jump to a frame).
    Animation(AnimationCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryCommand {
    /// `i=` — echoed back verbatim in the response per spec, so a sender
    /// juggling multiple in-flight queries can match responses to requests.
    pub id: Option<u32>,
    /// `I=` — the "image number" variant of the same idea; also echoed
    /// back verbatim when present. Not otherwise used anywhere else in
    /// this implementation (see `TransmitCommand::id`'s doc comment on why
    /// image-number-only anonymous images aren't modeled).
    pub image_number: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// `f=24` — raw RGB, 3 bytes/pixel.
    Rgb,
    /// `f=32` (the protocol's default when `f` is omitted) — raw RGBA, 4 bytes/pixel.
    Rgba,
    /// `f=100` — PNG-encoded.
    Png,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransmissionMedium {
    /// `t=d` (the default when `t` is omitted) — payload is inline, base64 in this APC string.
    Direct,
    /// Any other medium (`f`/`t`/`s` — file, temp file, shared memory).
    ///
    /// Deliberately NOT decomposed into its own variants and deliberately
    /// unsupported for now: `t=f`/`t=t` let a remote program hand the
    /// terminal a *local filesystem path* to read — fine for a program
    /// running on your own machine, but Som terminals are routinely SSH
    /// sessions (`"tmux": true` or otherwise), where honoring that would let
    /// a remote process read arbitrary files on the machine actually running
    /// Som. See `KITTY_GRAPHICS_PLAN.md`'s "Transmission" bullet under
    /// "Объём протокола". `t=s` (POSIX shared memory) is a same-host-only
    /// mechanism that doesn't make sense across an SSH session either.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransmitCommand {
    /// `i=` — id the sender assigns this image, referenced by later `Place`/`Delete` commands.
    /// `None` when omitted (the protocol allows anonymous images referenced only by `I=`, the
    /// image *number* — not modeled yet, out of Stage 1's scope; such commands are parsed but
    /// their `id` field is `None`).
    pub id: Option<u32>,
    pub format: ImageFormat,
    pub medium: TransmissionMedium,
    /// `m=1` (more chunks follow) vs `m=0`/absent (this is the final or only chunk).
    pub more_chunks: bool,
    /// Also display the image immediately after transmit (`a=T`) — distinguishes
    /// from a plain `a=t`, upload-only-then-`Place`-separately.
    pub display_immediately: bool,
    /// `U=1` — the sender intends to display this image exclusively via
    /// Unicode placeholder cells (`crate::kitty_graphics_placeholder`)
    /// rather than a normal cursor-relative placement. Kitty's own
    /// behavior (and this implementation's) is to skip the usual
    /// auto-placement that `display_immediately` would otherwise trigger
    /// when this flag is set, since the sender is about to write
    /// placeholder characters into the grid itself to control positioning
    /// cell-by-cell instead.
    pub unicode_placeholder: bool,
    /// `s=`/`v=` — the image's pixel width/height. Optional for `f=100`
    /// (PNG), whose own header carries the dimensions, but MANDATORY for the
    /// raw pixel formats (`f=24` RGB / `f=32` RGBA): those payloads are just
    /// a flat byte array with no self-describing header, so without these
    /// keys there's no way to know how to slice them into rows. `yazi` sends
    /// raw RGBA (`f=32,s=W,v=H`), so dropping these here would make its
    /// images undecodable.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Raw base64 text from this APC string's payload section, NOT yet decoded
    /// or concatenated with any other chunk.
    pub payload_base64: String,
    /// Whether this chunk actually carried an `a=` key, vs. defaulting to
    /// `t` because `a=` was entirely absent (a continuation chunk of ANY
    /// multi-chunk command — `a=T`, `a=t`, or `a=f` — omits `a=` per spec,
    /// not just `a=t`/`a=T`'s own continuations). This parser is
    /// deliberately stateless (see this module's doc comment) and has no
    /// way to know which multi-chunk transmission is actually in progress,
    /// so it can't itself route an action-less chunk to the right variant —
    /// it has to guess `t` and let the caller (`kitty_graphics_store`,
    /// which DOES track that state via `active_pending_id`/
    /// `active_frame_id`) correct the guess. Skipping that correction was a
    /// real, confirmed bug: every `a=f` frame's continuation chunks were
    /// silently misrouted here and dropped, so a real animated GIF's frames
    /// past the first chunk of each never made it into `ImageStore` at all.
    pub explicit_action: bool,
}

/// A frame's `z=` gap, resolved to its three distinct meanings per
/// <https://sw.kovidgoyal.net/kitty/graphics-protocol/#animation-frames>'s
/// "Keys for animation frame loading" table: `z=0`/absent both mean "use
/// the protocol's default" (which is NOT "reuse the previous frame's gap"
/// — that rule doesn't exist in the spec — it's a flat 40ms), while a
/// negative value means "gapless" (advance to the next frame immediately,
/// no wait at all). Modeled as its own enum rather than a raw
/// `Option<i32>` so a caller can't accidentally treat `Gapless` as "0ms",
/// which per spec is a materially different, faster-than-representable
/// behavior (immediate) rather than a very short timed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameGap {
    /// `z=0` or absent — 40ms, the spec's stated default.
    Default,
    /// `z=<positive>` — this many milliseconds.
    Milliseconds(u32),
    /// `z=<negative>` — advance immediately, no wait.
    Gapless,
}

impl FrameGap {
    fn from_z(z: Option<i32>) -> Self {
        match z {
            None | Some(0) => Self::Default,
            Some(n) if n < 0 => Self::Gapless,
            Some(n) => Self::Milliseconds(n as u32),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameCommand {
    /// `i=` — the image this frame belongs to. Like a `Transmit`
    /// continuation chunk, only the first APC string of a (possibly
    /// chunked) frame transmission carries this; later ones resolve
    /// against whichever frame transmission is in progress, mirroring
    /// `ImageStore::active_pending_id`'s reasoning for `Transmit`.
    pub id: Option<u32>,
    /// `z=` — this frame's own display duration before advancing to the
    /// next one. See [`FrameGap`]'s doc comment for its three-way meaning.
    pub gap: FrameGap,
    /// `m=1` — more chunks of this SAME frame's payload follow.
    pub more_chunks: bool,
    /// Raw base64 payload for this chunk, not yet decoded/concatenated.
    pub payload_base64: String,
}

/// `s=` on `a=a` — see [`AnimationCommand::SetState`]'s doc comment for the
/// full per-value spec citation. Kitty's own three states, verbatim
/// (`kitty/graphics.c`'s `ANIMATION_STOPPED`/`ANIMATION_LOADING`/
/// `ANIMATION_RUNNING`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationState {
    /// `s=1` — stop the animation, freezing on whichever frame is current.
    Stopped,
    /// `s=2` — run the animation, but in "loading" mode: on reaching the
    /// last transmitted frame, wait for more frames to arrive instead of
    /// looping back to the first one.
    Loading,
    /// `s=3` — run the animation normally: after the last frame, loop back
    /// to the first.
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationCommand {
    /// `s=1`/`s=2`/`s=3` — change (or, for a client-driven animation with
    /// no `s=` at all, leave unchanged — see `parse_animation`) playback
    /// state. Spec: "The animation state is controlled by the `s` key.
    /// `s=1` stops the animation. `s=2` runs the animation, but in
    /// *loading* mode... `s=3` runs the animation normally... after the
    /// last frame, the terminal loops back to the first frame."
    SetState { id: Option<u32>, state: AnimationState },
    /// `c=<n>` — make the 1-based `n`th frame the current one (client-
    /// driven animation, no autoplay). Spec: "To change the current
    /// frame, use the `c` key... This will make the seventh frame... the
    /// current frame", table: "The 1-based frame number of the frame that
    /// should be made the current frame."
    SetCurrentFrame { id: Option<u32>, frame: u32 },
    /// `r=<n>,z=<gap>` — set the `n`th frame's gap after the fact. The
    /// ONLY way to give the root frame (the base image from `a=T`/`a=t`,
    /// which itself has no gap-setting `z=` of its own — there `z=` would
    /// collide with z-index) a nonzero gap: spec's own example is
    /// `a=a,i=7,r=3,z=48` ("sets the gap for the third frame... to 48
    /// milliseconds") and "the first frame or *root frame* is created
    /// with the base image data and has no gap, so its gap must be set
    /// using this control code."
    SetFrameGap { id: Option<u32>, frame: u32, gap: FrameGap },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceCommand {
    pub id: Option<u32>,
    pub placement_id: Option<u32>,
    /// `z=` — z-index relative to text; negative draws under it.
    pub z_index: i32,
    /// `c=`/`r=` — display size in terminal cells, when the sender wants to override
    /// the image's natural pixel size mapped through cell metrics.
    pub columns: Option<u32>,
    pub rows: Option<u32>,
    /// `U=1` — same meaning as [`TransmitCommand::unicode_placeholder`]:
    /// this placement is (or will be) positioned via Unicode placeholder
    /// cells rather than being anchored to the cursor position at the time
    /// this `Place` command was received.
    pub unicode_placeholder: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteCommand {
    /// `d=a`/`d=A` — delete all placements (uppercase additionally frees image data).
    All { free_data: bool },
    /// `d=i`/`d=I` — delete by image id.
    ById { id: u32, free_data: bool },
    /// `d=p`/`d=P` — delete by (image id, placement id) pair.
    ByPlacement { id: u32, placement_id: u32, free_data: bool },
}

/// Parse one APC string's raw bytes (control data + `;` + payload) into a
/// [`KittyGraphicsCommand`].
///
/// Returns `None` for anything that isn't recognizably a Kitty graphics
/// command — including APC strings from something else entirely (this
/// module has no way to know APC is "supposed to" mean Kitty graphics;
/// that's the caller's job to have already decided, per-protocol, before
/// routing bytes here) or graphics commands this stage doesn't support
/// (unrecognized `a=` action, or a `t=` medium we intentionally reject).
pub fn parse_command(bytes: &[u8]) -> Option<KittyGraphicsCommand> {
    // Every Kitty graphics APC string starts with a leading `G` marking it
    // as a graphics command specifically (distinguishing it from any other
    // protocol that might also use APC) — real terminal output looks like
    // `ESC _ G a=T,f=100;<payload> ESC \`, i.e. the bytes handed to this
    // function (everything between apc_hook/apc_unhook) are `Ga=T,f=100;...`,
    // NOT `a=T,f=100;...`. A leading comma after `G` (`G,a=T,...`) is also
    // valid per the spec and stripped the same way, since it's just the
    // control-data separator appearing before the first real pair.
    let bytes = bytes.strip_prefix(b"G")?;
    let bytes = bytes.strip_prefix(b",").unwrap_or(bytes);

    // Split off the payload (after the first unescaped `;`) from control data.
    // Kitty's control-data keys/values never contain `;`, so the first one
    // found unambiguously starts the payload section (which itself may be
    // empty, e.g. a bare `a=d` delete command has no payload at all).
    let semicolon = bytes.iter().position(|&b| b == b';');
    let (control_bytes, payload_bytes) = match semicolon {
        Some(idx) => (&bytes[..idx], &bytes[idx + 1..]),
        None => (bytes, &bytes[..0]),
    };

    let controls = parse_control_data(control_bytes)?;
    let explicit_action = controls.contains_key("a");
    let action = controls.get("a").copied().unwrap_or("t");

    match action {
        "q" => Some(KittyGraphicsCommand::Query(QueryCommand {
            id: parse_u32(&controls, "i"),
            image_number: parse_u32(&controls, "I"),
        })),
        "d" => parse_delete(&controls).map(KittyGraphicsCommand::Delete),
        "p" => Some(KittyGraphicsCommand::Place(parse_place(&controls))),
        "t" | "T" => parse_transmit(&controls, payload_bytes, action == "T", explicit_action)
            .map(KittyGraphicsCommand::Transmit),
        "f" => parse_frame(&controls, payload_bytes).map(KittyGraphicsCommand::Frame),
        "a" => parse_animation(&controls).map(KittyGraphicsCommand::Animation),
        _ => None,
    }
}

/// Parse `key=value,key=value,...` into a lookup table.
///
/// Returns `None` (rather than silently dropping malformed pairs) if the
/// control data doesn't parse as valid UTF-8 or contains a comma-separated
/// entry with no `=` — a genuinely malformed command shouldn't be
/// misinterpreted as a differently-shaped valid one.
fn parse_control_data(bytes: &[u8]) -> Option<HashMap<&str, &str>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.is_empty() {
        return Some(HashMap::new());
    }
    let mut map = HashMap::new();
    for pair in text.split(',') {
        let (key, value) = pair.split_once('=')?;
        if key.is_empty() {
            return None;
        }
        map.insert(key, value);
    }
    Some(map)
}

fn parse_u32(controls: &HashMap<&str, &str>, key: &str) -> Option<u32> {
    controls.get(key).and_then(|v| v.parse().ok())
}

fn parse_i32(controls: &HashMap<&str, &str>, key: &str) -> Option<i32> {
    controls.get(key).and_then(|v| v.parse().ok())
}

fn parse_transmit(
    controls: &HashMap<&str, &str>,
    payload_bytes: &[u8],
    display_immediately: bool,
    explicit_action: bool,
) -> Option<TransmitCommand> {
    let format = match controls.get("f").copied().unwrap_or("32") {
        "24" => ImageFormat::Rgb,
        "32" => ImageFormat::Rgba,
        "100" => ImageFormat::Png,
        _ => return None,
    };
    let medium = match controls.get("t").copied().unwrap_or("d") {
        "d" => TransmissionMedium::Direct,
        _ => TransmissionMedium::Unsupported,
    };
    let more_chunks = controls.get("m").copied() == Some("1");
    let unicode_placeholder = controls.get("U").copied() == Some("1");
    let payload_base64 = std::str::from_utf8(payload_bytes).ok()?.to_string();

    Some(TransmitCommand {
        id: parse_u32(controls, "i"),
        format,
        medium,
        more_chunks,
        display_immediately,
        unicode_placeholder,
        width: parse_u32(controls, "s"),
        height: parse_u32(controls, "v"),
        payload_base64,
        explicit_action,
    })
}

fn parse_frame(controls: &HashMap<&str, &str>, payload_bytes: &[u8]) -> Option<FrameCommand> {
    Some(FrameCommand {
        id: parse_u32(controls, "i"),
        gap: FrameGap::from_z(parse_i32(controls, "z")),
        more_chunks: controls.get("m").copied() == Some("1"),
        payload_base64: std::str::from_utf8(payload_bytes).ok()?.to_string(),
    })
}

/// `s=0`/absent (no `c=` or `r=` either) is a legitimate no-op per spec —
/// "The animation state is controlled by the `s` key" table's own default
/// row (`s` default `0`) plus `kitty/graphics.c`'s own dispatch
/// (`if (g->animation_state) { switch (...) }` — state 0 skips the switch
/// entirely, leaving playback untouched). Returns `None` for that case
/// rather than inventing a `Start`/default action the spec doesn't
/// describe — see this file's `KITTY_GRAPHICS_PLAN.md`-linked doc comments
/// elsewhere in this crate for the same "don't guess" principle applied to
/// unrecognized `a=`/`d=` values.
fn parse_animation(controls: &HashMap<&str, &str>) -> Option<AnimationCommand> {
    let id = parse_u32(controls, "i");

    if let Some(frame) = parse_u32(controls, "r")
        && controls.contains_key("z")
    {
        return Some(AnimationCommand::SetFrameGap {
            id,
            frame,
            gap: FrameGap::from_z(parse_i32(controls, "z")),
        });
    }

    match controls.get("s").copied() {
        Some("1") => return Some(AnimationCommand::SetState { id, state: AnimationState::Stopped }),
        Some("2") => return Some(AnimationCommand::SetState { id, state: AnimationState::Loading }),
        Some("3") => return Some(AnimationCommand::SetState { id, state: AnimationState::Running }),
        _ => {},
    }

    if let Some(frame) = parse_u32(controls, "c") {
        return Some(AnimationCommand::SetCurrentFrame { id, frame });
    }

    None
}

fn parse_place(controls: &HashMap<&str, &str>) -> PlaceCommand {
    PlaceCommand {
        id: parse_u32(controls, "i"),
        placement_id: parse_u32(controls, "p"),
        z_index: parse_i32(controls, "z").unwrap_or(0),
        columns: parse_u32(controls, "c"),
        rows: parse_u32(controls, "r"),
        unicode_placeholder: controls.get("U").copied() == Some("1"),
    }
}

/// Build the raw APC response bytes for a `a=q` capability query, per
/// `KITTY_GRAPHICS_PLAN.md` Stage 5.
///
/// Kitty's own terminal replies to a query with the same identifiers the
/// sender used (so a client juggling multiple in-flight queries can match
/// the response back to its request) plus a payload of either `OK` or an
/// error message. Som always answers `OK` — a query never actually
/// transmits/decodes any real image data (per spec, senders keep the
/// payload trivially small, e.g. a 1x1 placeholder blob), so there's
/// nothing for this implementation to legitimately reject; a query is
/// purely "does this terminal understand the protocol at all."
///
/// Returns the complete escape sequence (`ESC _ G ... ESC \`), ready to
/// hand straight to `Terminal::write_to_pty` — not just the control-data
/// portion `parse_command` deals with, since a response is something this
/// module produces rather than parses.
pub fn query_response(query: QueryCommand) -> Vec<u8> {
    let mut response = Vec::with_capacity(24);
    response.extend_from_slice(b"\x1b_G");
    let mut wrote_key = false;
    if let Some(id) = query.id {
        response.extend_from_slice(format!("i={id}").as_bytes());
        wrote_key = true;
    }
    if let Some(image_number) = query.image_number {
        if wrote_key {
            response.push(b',');
        }
        response.extend_from_slice(format!("I={image_number}").as_bytes());
    }
    response.extend_from_slice(b";OK");
    response.extend_from_slice(b"\x1b\\");
    response
}

fn parse_delete(controls: &HashMap<&str, &str>) -> Option<DeleteCommand> {
    let d = controls.get("d").copied().unwrap_or("a");
    let (kind, free_data) = {
        let mut chars = d.chars();
        let kind = chars.next()?;
        let free_data = kind.is_ascii_uppercase();
        (kind.to_ascii_lowercase(), free_data)
    };

    match kind {
        'a' => Some(DeleteCommand::All { free_data }),
        'i' => Some(DeleteCommand::ById { id: parse_u32(controls, "i")?, free_data }),
        'p' => Some(DeleteCommand::ByPlacement {
            id: parse_u32(controls, "i")?,
            placement_id: parse_u32(controls, "p")?,
            free_data,
        }),
        // Other delete-by-position variants (n/N, c/C, f/F, q/Q, r/R, x/X,
        // y/Y, z/Z — delete by cursor position, cell coordinates, etc.) are
        // out of Stage 1's scope; a real placement store to delete *from*
        // doesn't exist yet (that's Stage 2), so there's nothing meaningful
        // for these variants to do yet.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yazis_exact_first_chunk() {
        // Captured verbatim off the wire from `yazi` running inside Som
        // (see `yazi-adapter/src/drivers/kgp.rs`'s `encode`): every control
        // key it sends, in its order, including the ones Som doesn't act on
        // (`q=2` suppress-responses, `C=1` don't-move-cursor). A parser that
        // rejects the whole command because of an unrecognized key would
        // silently drop the ONLY chunk carrying the image id, format and
        // dimensions — every continuation chunk after it is a bare
        // `m=1;<payload>`, so the image could never be reassembled.
        let apc = b"Gq=2,a=T,C=1,U=1,f=32,s=512,v=512,i=24836,m=1;AAAA";
        let command = parse_command(apc).expect("yazi's first chunk must parse");
        match command {
            KittyGraphicsCommand::Transmit(t) => {
                assert_eq!(t.id, Some(24836), "image id must survive");
                assert_eq!(t.format, ImageFormat::Rgba, "f=32 is raw RGBA");
                assert!(t.more_chunks, "m=1 means more chunks follow");
                assert!(t.display_immediately, "a=T displays immediately");
                assert!(t.unicode_placeholder, "U=1 selects unicode placeholder mode");
                assert_eq!(t.width, Some(512), "s=512 is the pixel width");
                assert_eq!(t.height, Some(512), "v=512 is the pixel height");
            }
            other => panic!("expected Transmit, got {other:?}"),
        }
    }

    #[test]
    fn query_response_with_no_identifiers() {
        assert_eq!(
            query_response(QueryCommand { id: None, image_number: None }),
            b"\x1b_G;OK\x1b\\".to_vec()
        );
    }

    #[test]
    fn query_response_echoes_id() {
        assert_eq!(
            query_response(QueryCommand { id: Some(31), image_number: None }),
            b"\x1b_Gi=31;OK\x1b\\".to_vec()
        );
    }

    #[test]
    fn query_response_echoes_id_and_image_number() {
        assert_eq!(
            query_response(QueryCommand { id: Some(31), image_number: Some(7) }),
            b"\x1b_Gi=31,I=7;OK\x1b\\".to_vec()
        );
    }

    #[test]
    fn query_response_echoes_image_number_only() {
        assert_eq!(
            query_response(QueryCommand { id: None, image_number: Some(7) }),
            b"\x1b_GI=7;OK\x1b\\".to_vec()
        );
    }

    #[test]
    fn parse_query() {
        assert_eq!(
            parse_command(b"Ga=q"),
            Some(KittyGraphicsCommand::Query(QueryCommand { id: None, image_number: None }))
        );
    }

    #[test]
    fn parse_query_echoes_id_and_image_number() {
        assert_eq!(
            parse_command(b"Ga=q,i=7,I=3"),
            Some(KittyGraphicsCommand::Query(QueryCommand {
                id: Some(7),
                image_number: Some(3)
            }))
        );
    }

    #[test]
    fn parse_simple_transmit_and_display() {
        let cmd = parse_command(b"Ga=T,f=100,i=7;AAA=").unwrap();
        assert_eq!(
            cmd,
            KittyGraphicsCommand::Transmit(TransmitCommand {
                id: Some(7),
                format: ImageFormat::Png,
                medium: TransmissionMedium::Direct,
                more_chunks: false,
                display_immediately: true,
                unicode_placeholder: false,
                width: None,
                height: None,
                payload_base64: "AAA=".to_string(),
                explicit_action: true,
            })
        );
    }

    #[test]
    fn parse_transmit_only_defaults_format_and_medium() {
        // Bare "a=t" (no f=, no t=) should default to RGBA direct transmission,
        // per spec defaults, and NOT auto-display (a=t vs a=T).
        let cmd = parse_command(b"Ga=t,i=3;AAAAAAAA").unwrap();
        match cmd {
            KittyGraphicsCommand::Transmit(t) => {
                assert_eq!(t.format, ImageFormat::Rgba);
                assert_eq!(t.medium, TransmissionMedium::Direct);
                assert!(!t.display_immediately);
                assert_eq!(t.id, Some(3));
            },
            _ => panic!("expected Transmit"),
        }
    }

    #[test]
    fn parse_action_defaults_to_transmit_when_a_omitted() {
        // The spec's own default for a bare "i=1,f=24;..." (no a= at all)
        // is transmit-only.
        let cmd = parse_command(b"Gi=1,f=24;AAA=").unwrap();
        match cmd {
            KittyGraphicsCommand::Transmit(t) => {
                assert_eq!(t.format, ImageFormat::Rgb);
                assert!(!t.display_immediately);
            },
            _ => panic!("expected Transmit"),
        }
    }

    #[test]
    fn chunked_transmission_more_flag() {
        let first = parse_command(b"Ga=T,i=1,m=1;AAAA").unwrap();
        let last = parse_command(b"Ga=T,i=1,m=0;BBBB").unwrap();
        match (first, last) {
            (
                KittyGraphicsCommand::Transmit(TransmitCommand { more_chunks: true, .. }),
                KittyGraphicsCommand::Transmit(TransmitCommand { more_chunks: false, .. }),
            ) => {},
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn file_transmission_medium_marked_unsupported_not_rejected_outright() {
        // t=f (file) must parse successfully (so a well-formed command isn't
        // treated as garbage) but land in the Unsupported bucket — the
        // caller decides what "unsupported" means (ignore, or answer with a
        // protocol-level error response), not this parser. Security note:
        // see TransmissionMedium::Unsupported's doc comment for why file
        // transmission specifically must never be honored over SSH.
        let cmd = parse_command(b"Ga=t,i=1,t=f;/etc/passwd").unwrap();
        match cmd {
            KittyGraphicsCommand::Transmit(t) => {
                assert_eq!(t.medium, TransmissionMedium::Unsupported);
            },
            _ => panic!("expected Transmit"),
        }
    }

    #[test]
    fn parse_place() {
        let cmd = parse_command(b"Ga=p,i=7,p=2,z=-1,c=10,r=5").unwrap();
        assert_eq!(
            cmd,
            KittyGraphicsCommand::Place(PlaceCommand {
                id: Some(7),
                placement_id: Some(2),
                z_index: -1,
                columns: Some(10),
                rows: Some(5),
                unicode_placeholder: false,
            })
        );
    }

    #[test]
    fn parse_place_unicode_placeholder_flag() {
        let cmd = parse_command(b"Ga=p,i=7,U=1").unwrap();
        match cmd {
            KittyGraphicsCommand::Place(p) => assert!(p.unicode_placeholder),
            _ => panic!("expected Place"),
        }
    }

    #[test]
    fn parse_transmit_unicode_placeholder_flag() {
        let cmd = parse_command(b"Ga=T,i=7,U=1;AAA=").unwrap();
        match cmd {
            KittyGraphicsCommand::Transmit(t) => assert!(t.unicode_placeholder),
            _ => panic!("expected Transmit"),
        }
    }

    #[test]
    fn parse_delete_all_lowercase_keeps_data() {
        assert_eq!(
            parse_command(b"Ga=d"),
            Some(KittyGraphicsCommand::Delete(DeleteCommand::All { free_data: false }))
        );
    }

    #[test]
    fn parse_delete_all_uppercase_frees_data() {
        assert_eq!(
            parse_command(b"Ga=d,d=A"),
            Some(KittyGraphicsCommand::Delete(DeleteCommand::All { free_data: true }))
        );
    }

    #[test]
    fn parse_delete_by_id() {
        assert_eq!(
            parse_command(b"Ga=d,d=i,i=42"),
            Some(KittyGraphicsCommand::Delete(DeleteCommand::ById { id: 42, free_data: false }))
        );
        assert_eq!(
            parse_command(b"Ga=d,d=I,i=42"),
            Some(KittyGraphicsCommand::Delete(DeleteCommand::ById { id: 42, free_data: true }))
        );
    }

    #[test]
    fn parse_delete_by_placement() {
        assert_eq!(
            parse_command(b"Ga=d,d=p,i=42,p=3"),
            Some(KittyGraphicsCommand::Delete(DeleteCommand::ByPlacement {
                id: 42,
                placement_id: 3,
                free_data: false
            }))
        );
    }

    #[test]
    fn delete_by_id_without_id_fails_gracefully() {
        // Malformed: d=i requires i= to be present. Must return None, not panic.
        assert_eq!(parse_command(b"Ga=d,d=i"), None);
    }

    #[test]
    fn unknown_action_returns_none() {
        assert_eq!(parse_command(b"Ga=zzz"), None);
    }

    #[test]
    fn malformed_control_data_returns_none() {
        // A comma-separated entry with no '=' at all.
        assert_eq!(parse_command(b"Ga=q,garbage"), None);
    }

    #[test]
    fn invalid_utf8_control_data_returns_none() {
        let mut bytes = vec![b'G'];
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        bytes.extend_from_slice(b"a=q");
        assert_eq!(parse_command(&bytes), None);
    }

    #[test]
    fn bytes_without_leading_g_are_not_a_graphics_command() {
        // Real Kitty graphics APC strings always start with G — bytes that
        // don't must be rejected outright rather than misparsed as if they
        // did (e.g. some other, non-Kitty use of APC).
        assert_eq!(parse_command(b"a=q"), None);
    }

    #[test]
    fn empty_control_data_defaults_to_bare_transmit() {
        // An APC string that's just ";payload" (no control data at all) is
        // technically well-formed per the grammar — action defaults to "t".
        let cmd = parse_command(b"G;AAA=").unwrap();
        match cmd {
            KittyGraphicsCommand::Transmit(t) => {
                assert_eq!(t.payload_base64, "AAA=");
                assert!(!t.display_immediately);
            },
            _ => panic!("expected Transmit"),
        }
    }

    #[test]
    fn parse_frame_append() {
        let cmd = parse_command(b"Ga=f,i=7,z=100;AAA=").unwrap();
        assert_eq!(
            cmd,
            KittyGraphicsCommand::Frame(FrameCommand {
                id: Some(7),
                gap: FrameGap::Milliseconds(100),
                more_chunks: false,
                payload_base64: "AAA=".to_string(),
            })
        );
    }

    #[test]
    fn parse_frame_append_chunked() {
        let cmd = parse_command(b"Ga=f,i=7,m=1;AAA=").unwrap();
        match cmd {
            KittyGraphicsCommand::Frame(f) => {
                assert_eq!(f.id, Some(7));
                assert!(f.more_chunks);
                assert_eq!(f.gap, FrameGap::Default);
            },
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn parse_frame_zero_gap_is_default_not_gapless() {
        let cmd = parse_command(b"Ga=f,i=7,z=0;AAA=").unwrap();
        match cmd {
            KittyGraphicsCommand::Frame(f) => assert_eq!(f.gap, FrameGap::Default),
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn parse_frame_negative_gap_is_gapless() {
        let cmd = parse_command(b"Ga=f,i=7,z=-1;AAA=").unwrap();
        match cmd {
            KittyGraphicsCommand::Frame(f) => assert_eq!(f.gap, FrameGap::Gapless),
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn parse_animation_bare_with_no_s_c_or_r_is_a_no_op() {
        // Per spec, s defaults to 0 ("not specified, don't change state")
        // — a bare "a=a,i=7" with none of s=/c=/r=+z= present carries no
        // actionable instruction at all, so this must return None, NOT
        // invent a "start" action the spec doesn't describe.
        assert_eq!(parse_command(b"Ga=a,i=7"), None);
    }

    #[test]
    fn parse_animation_s1_stops() {
        // Spec: "s=1 stops the animation." (NOT "starts" — this is the
        // exact inversion a prior, unverified implementation got wrong.)
        assert_eq!(
            parse_command(b"Ga=a,i=7,s=1"),
            Some(KittyGraphicsCommand::Animation(AnimationCommand::SetState {
                id: Some(7),
                state: AnimationState::Stopped
            }))
        );
    }

    #[test]
    fn parse_animation_s2_runs_in_loading_mode() {
        // Spec: "s=2 runs the animation, but in loading mode... instead of
        // looping, the terminal will wait for the arrival of more frames."
        assert_eq!(
            parse_command(b"Ga=a,i=7,s=2"),
            Some(KittyGraphicsCommand::Animation(AnimationCommand::SetState {
                id: Some(7),
                state: AnimationState::Loading
            }))
        );
    }

    #[test]
    fn parse_animation_s3_runs_and_loops() {
        // Spec: "s=3 runs the animation normally, after the last frame,
        // the terminal loops back to the first frame."
        assert_eq!(
            parse_command(b"Ga=a,i=7,s=3"),
            Some(KittyGraphicsCommand::Animation(AnimationCommand::SetState {
                id: Some(7),
                state: AnimationState::Running
            }))
        );
    }

    #[test]
    fn parse_animation_set_current_frame_via_c() {
        // Spec's own example: "<ESC>_Ga=a,i=3,c=7<ESC>\ This will make the
        // seventh frame in the image with id 3 the current frame." — NOT
        // r=, which (in a=a context) means something else entirely (see
        // parse_animation_set_frame_gap_via_r_and_z below).
        assert_eq!(
            parse_command(b"Ga=a,i=3,c=7"),
            Some(KittyGraphicsCommand::Animation(AnimationCommand::SetCurrentFrame {
                id: Some(3),
                frame: 7
            }))
        );
    }

    #[test]
    fn parse_animation_set_frame_gap_via_r_and_z() {
        // Spec's own example: "<ESC>_Ga=a,i=7,r=3,z=48<ESC>\ This sets the
        // gap for the third frame of the image with id 7 to 48
        // milliseconds." — this is the ONLY way to set the root frame's
        // gap (a=T/a=t's own z= means z-index, not gap).
        assert_eq!(
            parse_command(b"Ga=a,i=7,r=3,z=48"),
            Some(KittyGraphicsCommand::Animation(AnimationCommand::SetFrameGap {
                id: Some(7),
                frame: 3,
                gap: FrameGap::Milliseconds(48)
            }))
        );
    }

    #[test]
    fn parse_animation_r_without_z_is_not_a_frame_gap_command() {
        // r= alone (no z=) doesn't match the spec's set-frame-gap shape —
        // must not be misparsed as SetFrameGap with a bogus default gap.
        assert_eq!(parse_command(b"Ga=a,i=7,r=3"), None);
    }

    #[test]
    fn no_semicolon_at_all_still_parses_control_only_commands() {
        // Query/delete/place commands have no payload, so a real-world APC
        // string for them may omit the trailing `;` entirely.
        assert_eq!(
            parse_command(b"Ga=q"),
            Some(KittyGraphicsCommand::Query(QueryCommand { id: None, image_number: None }))
        );
    }
}
