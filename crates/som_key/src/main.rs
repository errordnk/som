//! Diagnostic tool: draws a physical keyboard and highlights exactly what
//! GPUI decodes for every key press/release, going through the identical
//! code path Som uses (platform events -> `capture_key_down`/`on_key_up`/
//! `on_modifiers_changed`). Useful for telling apart "the physical
//! keyboard/KVM/OS remapper chain is sending something unexpected" from
//! "Som's own keymap matching is broken" without touching Som's actual
//! keymap code.
//!
//! The drawn layout matches the platform som-key is running on (macOS,
//! Windows, or Linux) since the bottom modifier row and a few key labels
//! differ per platform.

use gpui::{
    App, AppContext as _, Bounds, Context, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, KeyUpEvent, ModifiersChangedEvent, ParentElement, Render, Side,
    Styled, Window, WindowBounds, WindowOptions, div, prelude::FluentBuilder as _, px, rgb, size,
};
use gpui_platform::application;
use std::collections::HashSet;
use std::time::Duration;

/// How long a toggle key (CapsLock/NumLock/ScrollLock) stays visually
/// "pressed" after each press that flips its toggle state — whether that
/// flips the LED on OR off. Windows (and other platforms) only ever
/// report ONE `ModifiersChangedEvent` per physical press of these keys,
/// never a matching key-up when the key is physically released, since the
/// toggle state doesn't change again until the *next* press. There is no
/// reliable cross-platform signal for "this toggle key was just
/// released," so this flashes the key briefly instead of trying to track
/// true press/release for it. The LED indicator (see
/// `render_lock_indicators`) shows the actual persistent on/off state;
/// this flash is purely "a keypress just happened here," independent of
/// which direction the toggle moved.
const TOGGLE_KEY_FLASH_DURATION: Duration = Duration::from_millis(150);

/// Which physical keyboard form factor to draw. Selected via a command-line
/// argument (`--60`, `--80`/`--tkl`, `--100`/`--full`) since macOS doesn't
/// hand out a way to ask "what keyboard is this" without an Input
/// Monitoring permission prompt tied to IOHIDManager — not worth the
/// friction for a diagnostic tool. Defaults to Full100 (with the numpad)
/// so the tool is immediately useful for numpad-specific keys without
/// requiring a flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum KeyboardProfile {
    Compact60,
    Tkl80,
    Full100,
}

fn keyboard_profile_from_args() -> KeyboardProfile {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--60" | "--compact" | "--60%" => return KeyboardProfile::Compact60,
            "--80" | "--tkl" | "--80%" => return KeyboardProfile::Tkl80,
            "--100" | "--full" | "--100%" => return KeyboardProfile::Full100,
            _ => {}
        }
    }
    KeyboardProfile::Full100
}

/// Identifies one physical key on the drawn layout.
///
/// `Char` covers keys whose `Keystroke.key` string is the same on every
/// layout (arrows, space, enter, escape, function keys, ...) — those are
/// matched directly against the string. `Physical` covers letter/number/
/// symbol keys, whose printed character *does* depend on layout — those
/// are matched by `Keystroke.physical_key` (a layout-independent USB HID
/// Usage ID) instead, which is the only thing that stays stable when the
/// active keyboard layout changes (e.g. a Russian layout reports "й" where
/// a US layout reports "q" for the same physical key). The four modifier
/// keys are special-cased into left/right pairs because neither `key` nor
/// `physical_key` distinguishes sides — only `ModifiersChangedEvent`'s
/// `Modifiers::side` can, and only for bare modifier presses. Numpad keys
/// are special-cased too: a numpad "1" and a top-row "1" produce the same
/// `key` string, and only `Keystroke.is_numpad` tells them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum KeyId {
    Char(&'static str),
    Physical(u32),
    /// Like `Physical`, matched by USB HID Usage ID instead of `Keystroke.key`
    /// — but for keys with a fixed label that never changes with layout/
    /// Shift/CapsLock (Print Screen, Scroll Lock, Pause). Needed because
    /// macOS reports these as F13/F14/F15 (`ks.key == "f13"` etc.), not as
    /// any string this program could match — only the physical_key USB HID
    /// code (0x46/0x47/0x48) identifies them consistently.
    PhysicalStatic(u32),
    Numpad(&'static str),
    Modifier(ModifierKind, Side),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ModifierKind {
    Control,
    Alt,
    Shift,
    Platform,
    Function,
}

/// One cell in the drawn layout: its default label, the identity used to
/// track pressed state, its relative width (1.0 = a normal 1u key), and
/// whether its label should render at the smaller "doesn't fit at full
/// size" font — set explicitly rather than inferred from label length, so
/// groups of keys with mixed label lengths (e.g. the nav cluster's
/// "ins"/"home") can still render at one consistent size.
///
/// Keys whose `id` is `KeyId::Physical(usb_hid)` have a layout-dependent
/// printed character. When the user has actually pressed that physical key
/// at least once, `SomKey` remembers what character it typed (keyed by the
/// USB HID Usage ID) and displays *that* instead of the static `label`
/// below — this is what makes a key show "Й" under a Russian layout, "Q"
/// with Shift held, etc., without this program needing to know anything
/// about any specific layout. Keys with any other `id` variant (arrows,
/// function keys, space, modifiers, numpad, ...) always show their static
/// label — those don't have a layout-dependent printed form.
struct KeyCell {
    label: &'static str,
    id: KeyId,
    width: f32,
    small_text: bool,
}

fn key(label: &'static str, key: &'static str) -> KeyCell {
    KeyCell { label, id: KeyId::Char(key), width: 1.0, small_text: label.len() > 4 }
}

fn wkey(label: &'static str, key: &'static str, width: f32) -> KeyCell {
    KeyCell { label, id: KeyId::Char(key), width, small_text: label.len() > 4 }
}

/// A key whose printed character depends on layout/Shift/CapsLock — letters
/// and the number/symbol row. `label` is only the pre-first-keypress
/// placeholder; once the user presses this physical key, its live-observed
/// character takes over (see the `KeyCell` docs).
fn ukey(label: &'static str, usb_hid: u32) -> KeyCell {
    KeyCell { label, id: KeyId::Physical(usb_hid), width: 1.0, small_text: false }
}

fn ukey_w(label: &'static str, usb_hid: u32, width: f32) -> KeyCell {
    KeyCell { label, id: KeyId::Physical(usb_hid), width, small_text: false }
}

/// A key identified by USB HID Usage ID (needed because macOS/Windows/Linux
/// don't agree on a `key` string for it) but whose label never changes.
fn ukey_static(label: &'static str, usb_hid: u32) -> KeyCell {
    KeyCell { label, id: KeyId::PhysicalStatic(usb_hid), width: 1.0, small_text: label.len() > 4 }
}

fn modifier(label: &'static str, kind: ModifierKind, side: Side, width: f32) -> KeyCell {
    KeyCell { label, id: KeyId::Modifier(kind, side), width, small_text: true }
}

/// Marks a cell to always render with the smaller label font, regardless
/// of its text length — used for groups (like the nav cluster) whose
/// members have mixed label lengths but must look visually consistent.
fn small(mut cell: KeyCell) -> KeyCell {
    cell.small_text = true;
    cell
}

/// The 6 rows of a standard ANSI keyboard: function row, number row, and
/// four alpha rows down to the bottom modifier row. Platform-specific rows
/// (function-key row labels, bottom modifier row) are built separately.
fn number_row() -> Vec<KeyCell> {
    vec![
        ukey("`", 0x35),
        ukey("1", 0x1E),
        ukey("2", 0x1F),
        ukey("3", 0x20),
        ukey("4", 0x21),
        ukey("5", 0x22),
        ukey("6", 0x23),
        ukey("7", 0x24),
        ukey("8", 0x25),
        ukey("9", 0x26),
        ukey("0", 0x27),
        ukey("-", 0x2D),
        ukey("=", 0x2E),
        wkey("delete", "backspace", 2.0),
    ]
}

/// The function row, grouped the way most keyboards silkscreen it: esc on
/// its own, then F1-F4 / F5-F8 / F9-F12 / F13 as separate clusters with a
/// wider gap between each — rendered by `render_function_row`, not
/// `render_row`, since it needs per-group spacing `render_row`'s flat list
/// can't express.
fn function_row_groups() -> Vec<Vec<KeyCell>> {
    vec![
        vec![key("esc", "escape")],
        vec![key("F1", "f1"), key("F2", "f2"), key("F3", "f3"), key("F4", "f4")],
        vec![key("F5", "f5"), key("F6", "f6"), key("F7", "f7"), key("F8", "f8")],
        vec![key("F9", "f9"), key("F10", "f10"), key("F11", "f11"), key("F12", "f12")],
        vec![key("F13", "f13")],
    ]
}

fn qwerty_row() -> Vec<KeyCell> {
    vec![
        wkey("tab", "tab", 1.6),
        ukey("q", 0x14),
        ukey("w", 0x1A),
        ukey("e", 0x08),
        ukey("r", 0x15),
        ukey("t", 0x17),
        ukey("y", 0x1C),
        ukey("u", 0x18),
        ukey("i", 0x0C),
        ukey("o", 0x12),
        ukey("p", 0x13),
        ukey("[", 0x2F),
        ukey("]", 0x30),
        ukey_w("\\", 0x31, 1.4),
    ]
}

fn asdf_row() -> Vec<KeyCell> {
    vec![
        wkey("capslock", "capslock", 1.9),
        ukey("a", 0x04),
        ukey("s", 0x16),
        ukey("d", 0x07),
        ukey("f", 0x09),
        ukey("g", 0x0A),
        ukey("h", 0x0B),
        ukey("j", 0x0D),
        ukey("k", 0x0E),
        ukey("l", 0x0F),
        ukey(";", 0x33),
        ukey("'", 0x34),
        wkey("return", "enter", 2.1),
    ]
}

fn zxcv_row() -> Vec<KeyCell> {
    vec![
        modifier("shift", ModifierKind::Shift, Side::Left, 2.4),
        ukey("z", 0x1D),
        ukey("x", 0x1B),
        ukey("c", 0x06),
        ukey("v", 0x19),
        ukey("b", 0x05),
        ukey("n", 0x11),
        ukey("m", 0x10),
        ukey(",", 0x36),
        ukey(".", 0x37),
        ukey("/", 0x38),
        modifier("shift", ModifierKind::Shift, Side::Right, 2.6),
    ]
}

#[cfg(target_os = "macos")]
fn bottom_row() -> Vec<KeyCell> {
    vec![
        modifier("control", ModifierKind::Control, Side::Left, 1.3),
        modifier("option", ModifierKind::Alt, Side::Left, 1.3),
        modifier("command", ModifierKind::Platform, Side::Left, 1.3),
        wkey("space", "space", 6.0),
        modifier("command", ModifierKind::Platform, Side::Right, 1.275),
        modifier("fn", ModifierKind::Function, Side::Right, 1.275),
        modifier("option", ModifierKind::Alt, Side::Right, 1.275),
        modifier("control", ModifierKind::Control, Side::Right, 1.275),
    ]
}

#[cfg(target_os = "windows")]
fn bottom_row() -> Vec<KeyCell> {
    vec![
        modifier("ctrl", ModifierKind::Control, Side::Left, 1.3),
        modifier("win", ModifierKind::Platform, Side::Left, 1.3),
        modifier("alt", ModifierKind::Alt, Side::Left, 1.3),
        wkey("space", "space", 6.0),
        modifier("alt", ModifierKind::Alt, Side::Right, 1.275),
        modifier("fn", ModifierKind::Function, Side::Right, 1.275),
        modifier("win", ModifierKind::Platform, Side::Right, 1.275),
        modifier("ctrl", ModifierKind::Control, Side::Right, 1.275),
    ]
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn bottom_row() -> Vec<KeyCell> {
    vec![
        modifier("ctrl", ModifierKind::Control, Side::Left, 1.3),
        modifier("super", ModifierKind::Platform, Side::Left, 1.3),
        modifier("alt", ModifierKind::Alt, Side::Left, 1.3),
        wkey("space", "space", 5.6),
        modifier("alt", ModifierKind::Alt, Side::Right, 1.3),
        modifier("super", ModifierKind::Platform, Side::Right, 1.3),
        modifier("ctrl", ModifierKind::Control, Side::Right, 1.3),
    ]
}

/// The Print Screen / Scroll Lock / Pause row, level with the function row,
/// directly above the nav cluster — same as a full-size keyboard.
fn system_row() -> Vec<KeyCell> {
    vec![
        ukey_static("prtsc", 0x46),
        ukey_static("scrlk", 0x47),
        ukey_static("pause", 0x48),
    ]
}

/// The 6-key insert/delete/home/end/page-up/page-down block: 2 rows of 3
/// columns, with each bottom-row key directly under its top-row pair
/// (delete under insert, end under home, page down under page up).
fn nav_cluster_rows() -> Vec<Vec<KeyCell>> {
    vec![
        vec![small(key("ins", "insert")), small(key("home", "home")), small(key("pgup", "pageup"))],
        vec![small(key("del", "delete")), small(key("end", "end")), small(key("pgdn", "pagedown"))],
    ]
}

/// The inverted-T arrow cluster: an empty spacer keeps Up centered above
/// Left/Down/Right, matching a real keyboard's physical layout.
fn arrow_cluster_rows() -> Vec<Vec<Option<KeyCell>>> {
    vec![
        vec![None, Some(key("↑", "up")), None],
        vec![Some(key("←", "left")), Some(key("↓", "down")), Some(key("→", "right"))],
    ]
}

/// The standard 4-column numeric keypad, top-lock key first. macOS keypads
/// have a non-toggling "Clear" key where PC keyboards have NumLock.
/// Rendered as a flat row grid (no cell spans a real "row" of an adjacent
/// column) with `0` doubled in width and `+`/`enter` doubled in height,
/// which reads the same as a real keypad without needing grid row-spans.
#[cfg(target_os = "macos")]
const NUMPAD_LOCK_LABEL: &str = "clear";
#[cfg(not(target_os = "macos"))]
const NUMPAD_LOCK_LABEL: &str = "numlk";

/// Total rows kept across all `LOG_COLUMNS` log columns (see `render_log`)
/// — 10 rows per column, so this must stay a multiple of `LOG_COLUMNS`.
const MAX_LOG_LINES: usize = 30;

/// Every `KeyId::Char` variant that appears anywhere in the drawn layout —
/// i.e. keys with no layout-dependent printed form, so matching by the raw
/// `Keystroke.key` string is reliable everywhere. Letters/numbers/symbols
/// are *not* here; those are `KeyId::Physical` and matched by
/// `Keystroke.physical_key` instead (see `canonical_key_id`).
const ALL_CHAR_KEYS: &[&str] = &[
    "escape", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13",
    "backspace", "tab", "capslock", "enter", "space", "insert", "delete", "home", "end", "pageup",
    "pagedown", "up", "down", "left", "right", "printscreen", "scrolllock", "pause",
];

const ALL_NUMPAD_KEYS: &[&str] =
    &["numlock", "/", "*", "-", "7", "8", "9", "4", "5", "6", "1", "2", "3", "0", ".", "+", "enter"];

/// Every USB HID Usage ID a `KeyId::Physical` cell in the drawn layout can
/// have — used to filter `Keystroke.physical_key` so keys the layout
/// doesn't draw (should any exist) never leak into `dynamic_labels` or
/// pressed-state tracking.
const KNOWN_PHYSICAL_KEYS: &[u32] = &[
    0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x12,
    0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, // A-Z
    0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, // 1-0
    0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, // - = [ ] \ ; ' ` , . /
];

/// USB HID Usage IDs for `KeyId::PhysicalStatic` cells — keys with a fixed
/// label that platforms don't agree on a `key` string for (see
/// `system_row`'s Print Screen / Scroll Lock / Pause).
const KNOWN_STATIC_PHYSICAL_KEYS: &[u32] = &[0x46, 0x47, 0x48];

/// Best-guess US-QWERTY shifted symbol for the number/symbol row, used only
/// as a fallback before the user has actually pressed a physical key with
/// Shift held — at which point the *real*, live-observed character (from
/// whatever layout is actually active) always takes over instead (see
/// `display_label`). Plain `.to_uppercase()` works fine for letters but
/// gives the wrong answer for symbols (`"1".to_uppercase()` is still
/// `"1"`, not `"!"`), so those need an explicit table.
fn qwerty_shift_guess(usb_hid: u32, unshifted: &str) -> Option<&'static str> {
    Some(match usb_hid {
        0x1E => "!", // 1
        0x1F => "@", // 2
        0x20 => "#", // 3
        0x21 => "$", // 4
        0x22 => "%", // 5
        0x23 => "^", // 6
        0x24 => "&", // 7
        0x25 => "*", // 8
        0x26 => "(", // 9
        0x27 => ")", // 0
        0x2D => "_", // -
        0x2E => "+", // =
        0x2F => "{", // [
        0x30 => "}", // ]
        0x31 => "|", // backslash
        0x33 => ":", // ;
        0x34 => "\"", // '
        0x35 => "~", // `
        0x36 => "<", // ,
        0x37 => ">", // .
        0x38 => "?", // /
        _ => return None,
    })
    .filter(|_| unshifted.chars().count() == 1 && !unshifted.chars().next().unwrap().is_alphabetic())
}

/// The two cased forms observed for one physical key. Either slot may be
/// unpopulated if the user hasn't pressed that key in that shift state yet.
#[derive(Default)]
struct DynamicLabel {
    unshifted: Option<gpui::SharedString>,
    shifted: Option<gpui::SharedString>,
}

/// One row of the on-screen event log — a single `key_down`. Kept as
/// structured fields (not a pre-formatted string) so the renderer can color
/// each modifier name independently (bright when held, dim when not),
/// which plain string formatting can't express.
struct LogRow {
    shift: bool,
    ctrl: bool,
    alt: bool,
    cmd: bool,
    key_name: gpui::SharedString,
    scan_code: Option<u32>,
}

struct SomKey {
    focus_handle: FocusHandle,
    log: Vec<LogRow>,
    active_modifiers: gpui::Modifiers,
    /// Regular (non-modifier, non-numpad) keys currently held down.
    pressed_keys: HashSet<KeyId>,
    /// Which specific left/right modifier instances are currently held,
    /// tracked from `ModifiersChangedEvent::modifiers.side` since a bare
    /// modifier press never reaches `key_down`/`key_up`.
    pressed_modifiers: HashSet<(ModifierKind, Side)>,
    numlock_on: bool,
    capslock_on: bool,
    scrolllock_on: bool,
    /// Set once at startup from the command line — see
    /// `keyboard_profile_from_args`.
    profile: KeyboardProfile,
    /// The unshifted and shifted character each `KeyId::Physical` position
    /// was observed to type, keyed by USB HID Usage ID — see the `KeyCell`
    /// docs. Populated live from real `KeyDownEvent`s (one slot updates per
    /// keypress, depending on whether Shift/CapsLock was held at the time),
    /// so it reflects whatever the active OS keyboard layout produces, for
    /// any language, without this program hardcoding any layout table.
    /// Which slot is *displayed* is decided at render time from the
    /// current live Shift/CapsLock state, not from whenever the key was
    /// last pressed — so holding Shift alone re-cases every key on screen
    /// immediately, exactly like a physical keyboard's dual-legend caps.
    dynamic_labels: std::collections::HashMap<u32, DynamicLabel>,
}

impl SomKey {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            log: Vec::new(),
            active_modifiers: gpui::Modifiers::none(),
            pressed_keys: HashSet::new(),
            profile: keyboard_profile_from_args(),
            pressed_modifiers: HashSet::new(),
            numlock_on: false,
            capslock_on: false,
            scrolllock_on: false,
            dynamic_labels: std::collections::HashMap::new(),
        }
    }

    fn push_key_row(&mut self, row: LogRow) {
        self.log.push(row);
        if self.log.len() > MAX_LOG_LINES {
            self.log.remove(0);
        }
    }

    /// Matches an event's `Keystroke` against the static key ids used by
    /// the drawn layout. Letter/number/symbol keys (`KeyId::Physical`) are
    /// matched by `Keystroke.physical_key`, the only layout-independent
    /// signal — matching by `key` would miss e.g. a Russian layout's "й"
    /// entirely, since it never appears in `ALL_CHAR_KEYS`'s QWERTY
    /// strings. Everything else (arrows, space, function keys, ...) still
    /// matches by string, and numpad/top-row keys with the same printed
    /// character are told apart by `Keystroke.is_numpad`.
    fn canonical_key_id(ks: &gpui::Keystroke) -> Option<KeyId> {
        if let Some(usb_hid) = ks.physical_key
            && !ks.is_numpad
            && KNOWN_STATIC_PHYSICAL_KEYS.contains(&usb_hid)
        {
            return Some(KeyId::PhysicalStatic(usb_hid));
        }
        if let Some(usb_hid) = ks.physical_key
            && !ks.is_numpad
            && KNOWN_PHYSICAL_KEYS.contains(&usb_hid)
        {
            return Some(KeyId::Physical(usb_hid));
        }
        let table: &[&str] = if ks.is_numpad { ALL_NUMPAD_KEYS } else { ALL_CHAR_KEYS };
        table.iter().find(|&&k| k == ks.key).map(|&k| {
            if ks.is_numpad { KeyId::Numpad(k) } else { KeyId::Char(k) }
        })
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // Without this, Alt-held combinations are reported as "not
        // consumed" all the way back to the platform layer, and on
        // Windows an unhandled WM_SYSKEYDOWN falls through to
        // DefWindowProc, which plays the system error beep — since
        // som-key is a standalone diagnostic tool with no other key
        // handler competing for these events, it's always correct to
        // claim every keypress.
        cx.stop_propagation();
        let ks = &event.keystroke;
        self.active_modifiers = ks.modifiers;
        if let Some(id) = Self::canonical_key_id(ks) {
            self.pressed_keys.insert(id);
        }
        // Labels come exclusively from `on_layout_changed`'s
        // `key_for_physical` query, not from `ks.key` here: for non-ASCII
        // layouts (Russian, Armenian, ...) GPUI intentionally reports `key`
        // through the Cmd-layout translation instead of the raw character
        // (see `always_use_command_layout` in gpui_macos), so Zed's actual
        // keybindings stay reachable without Cmd held. That's correct for
        // matching keybindings, but it means `ks.key` for a live keypress
        // is the wrong thing to draw on the key — it would silently
        // overwrite the correct Cyrillic label with a Latin one.
        self.push_key_row(LogRow {
            shift: ks.modifiers.shift,
            ctrl: ks.modifiers.control,
            alt: ks.modifiers.alt,
            cmd: ks.modifiers.platform,
            key_name: ks.key.clone().into(),
            scan_code: ks.physical_key,
        });
        cx.notify();
    }

    fn key_up(&mut self, event: &KeyUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        if let Some(id) = Self::canonical_key_id(ks) {
            self.pressed_keys.remove(&id);
        }
        cx.notify();
    }

    /// Bare modifier presses (Shift/Control/Option/Command alone, with no
    /// other key) never reach `key_down`/`key_up` — the OS reports them as
    /// a distinct "modifiers changed" event, not a key event. We diff
    /// against the previous `Modifiers` bools to know which one flipped,
    /// and use `side` (when the platform reports it) to know which
    /// physical instance — otherwise both sides of that modifier light up.
    fn modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prev = self.active_modifiers;
        let m = event.modifiers;

        for (kind, was, now) in [
            (ModifierKind::Control, prev.control, m.control),
            (ModifierKind::Alt, prev.alt, m.alt),
            (ModifierKind::Shift, prev.shift, m.shift),
            (ModifierKind::Platform, prev.platform, m.platform),
            (ModifierKind::Function, prev.function, m.function),
        ] {
            if was == now {
                continue;
            }
            let sides: &[Side] = match m.side {
                Some(side) if now => &[side],
                _ => &[Side::Left, Side::Right],
            };
            for &side in sides {
                if now {
                    self.pressed_modifiers.insert((kind, side));
                } else {
                    self.pressed_modifiers.remove(&(kind, side));
                }
            }
        }

        self.active_modifiers = m;
        // Windows reports a ModifiersChangedEvent for these keys on every
        // physical press that flips the toggle — whether that flips it to
        // on OR off — so the flash has to fire on any change, not just
        // the on-transition, or every other press (the ones that toggle
        // the LED off) would silently produce no visual feedback at all.
        for (id, was_on, now_on) in [
            (KeyId::Char("capslock"), self.capslock_on, event.capslock.on),
            (KeyId::Numpad("numlock"), self.numlock_on, event.numlock.on),
            (KeyId::PhysicalStatic(0x47), self.scrolllock_on, event.scrolllock.on),
        ] {
            if was_on != now_on {
                self.flash_toggle_key(id, cx);
            }
        }
        self.numlock_on = event.numlock.on;
        self.capslock_on = event.capslock.on;
        self.scrolllock_on = event.scrolllock.on;
        cx.notify();
    }

    /// Briefly lights up a toggle key (see `TOGGLE_KEY_FLASH_DURATION`'s
    /// docs for why this exists instead of tracking true press/release).
    fn flash_toggle_key(&mut self, id: KeyId, cx: &mut Context<Self>) {
        self.pressed_keys.insert(id);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOGGLE_KEY_FLASH_DURATION).await;
            this.update(cx, |this, cx| {
                this.pressed_keys.remove(&id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The active keyboard layout changed — every remembered character is
    /// for the *old* layout now. Re-fetch every drawn physical key's
    /// current-layout character up front (via `key_for_physical`) rather
    /// than waiting for the user to press each one again — that's what
    /// makes a Russian layout switch immediately paint "й" on the key drawn
    /// "q", instead of only doing so after that key is actually pressed.
    /// Platforms/keys `key_for_physical` can't answer for fall back to the
    /// static QWERTY placeholder label, same as before this query existed.
    fn on_layout_changed(&mut self, cx: &App) {
        self.dynamic_labels.clear();
        let mapper = cx.keyboard_mapper();
        // `id()` is a stable-but-opaque identifier (e.g. a Windows KLID
        // hex string, or a macOS input-source id that isn't always
        // English) — the human-readable `name()` is what actually
        // contains "Russian" across platforms.
        let is_russian = cx.keyboard_layout().name().contains("Russian");
        for &usb_hid in KNOWN_PHYSICAL_KEYS {
            // The backslash-position key (USB HID 0x31) has no consistent
            // cross-platform answer under a Russian layout: Windows ЙЦУКЕН
            // moved "/" here (unshifted) with "\" on Shift, dropping "|"
            // entirely, but querying macOS's actual RussianWin layout data
            // returns "\" unshifted instead. Per explicit product decision,
            // draw the Windows ЙЦУКЕН behavior everywhere a Russian layout
            // is active, rather than whatever this platform's layout data
            // literally reports.
            if is_russian && usb_hid == 0x31 {
                let mut entry = DynamicLabel::default();
                entry.unshifted = Some("\\".into());
                entry.shifted = Some("/".into());
                self.dynamic_labels.insert(usb_hid, entry);
                continue;
            }
            let mut entry = DynamicLabel::default();
            entry.unshifted = mapper.key_for_physical(usb_hid, false).map(Into::into);
            entry.shifted = mapper.key_for_physical(usb_hid, true).map(Into::into);
            if entry.unshifted.is_some() || entry.shifted.is_some() {
                self.dynamic_labels.insert(usb_hid, entry);
            }
        }
    }

    fn is_pressed(&self, id: KeyId) -> bool {
        match id {
            KeyId::Char(_) | KeyId::Numpad(_) | KeyId::Physical(_) | KeyId::PhysicalStatic(_) => {
                self.pressed_keys.contains(&id)
            }
            KeyId::Modifier(kind, side) => self.pressed_modifiers.contains(&(kind, side)),
        }
    }

    /// The label a cell should actually show, decided fresh on every
    /// render from the *current* live Shift/CapsLock state — not from
    /// whatever state was active the last time this physical key was
    /// pressed. That's what makes holding Shift alone re-case every key on
    /// screen immediately, matching a real keyboard's dual-legend caps.
    ///
    /// Falls back through: the live-observed character for this shift
    /// state (once the user has pressed this physical key while in it) ->
    /// the *other* shift state's observed character, capitalized/
    /// lowercased as a best guess -> the static QWERTY placeholder label,
    /// same best-guess-cased.
    fn display_label(&self, cell: &KeyCell) -> String {
        let KeyId::Physical(usb_hid) = cell.id else {
            return cell.label.to_string();
        };
        let want_shifted = self.active_modifiers.shift || self.capslock_on;
        let entry = self.dynamic_labels.get(&usb_hid);

        let observed = entry.and_then(|e| if want_shifted { e.shifted.as_ref() } else { e.unshifted.as_ref() });
        if let Some(s) = observed {
            return s.to_string();
        }

        // No observation yet for this exact shift state: guess by
        // re-casing whatever we *do* have, falling back to the static
        // placeholder if we have nothing at all.
        let other = entry.and_then(|e| if want_shifted { e.unshifted.as_ref() } else { e.shifted.as_ref() });
        let base = other.map(|s| s.to_string()).unwrap_or_else(|| cell.label.to_string());

        if want_shifted {
            if let Some(symbol) = qwerty_shift_guess(usb_hid, &base) {
                return symbol.to_string();
            }
            base.to_uppercase()
        } else {
            base.to_lowercase()
        }
    }
}

impl Focusable for SomKey {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

const KEY_UNIT: f32 = 44.0;
const KEY_GAP: f32 = 4.0;

fn render_key(cell_width: f32, small_text: bool, label: String, pressed: bool) -> impl IntoElement {
    let width = px(cell_width * KEY_UNIT + (cell_width - 1.0).max(0.0) * KEY_GAP);
    div()
        .w(width)
        .h(px(KEY_UNIT))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .when(pressed, |el| el.bg(rgb(0x88c0d0)).border_color(rgb(0x88c0d0)))
        .when(!pressed, |el| el.bg(rgb(0x2e3440)).border_color(rgb(0x4c566a)))
        .text_color(if pressed { rgb(0x2e3440) } else { rgb(0xd8dee9) })
        .text_size(px(if small_text { 10.0 } else { 13.0 }))
        .child(label)
}

fn render_row(cells: Vec<KeyCell>, state: &SomKey) -> gpui::Div {
    div()
        .flex()
        .gap(px(KEY_GAP))
        .children(cells.into_iter().map(|c| {
            let pressed = state.is_pressed(c.id);
            let label = state.display_label(&c);
            render_key(c.width, c.small_text, label, pressed)
        }))
}

/// Renders `function_row_groups` with a wider gap between groups than
/// within them, matching how a real function row is visually clustered.
fn render_function_row(state: &SomKey) -> gpui::Div {
    // 16px between groups puts F13's right edge exactly under backspace's
    // right edge in number_row (13x1u + 1x2u keys, 13 gaps of KEY_GAP).
    div()
        .flex()
        .gap(px(16.0))
        .children(function_row_groups().into_iter().map(|group| render_row(group, state)))
}

fn render_nav_cluster(state: &SomKey) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(KEY_GAP))
        .children(nav_cluster_rows().into_iter().map(|row| render_row(row, state)))
}

fn render_arrow_cluster(state: &SomKey) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(KEY_GAP))
        .children(arrow_cluster_rows().into_iter().map(|row| {
            div()
                .flex()
                .gap(px(KEY_GAP))
                .children(row.into_iter().map(|maybe_cell| match maybe_cell {
                    Some(c) => {
                        let pressed = state.is_pressed(c.id);
                        let label = state.display_label(&c);
                        render_key(c.width, c.small_text, label, pressed).into_any_element()
                    }
                    None => div()
                        .w(px(KEY_UNIT))
                        .h(px(KEY_UNIT))
                        .into_any_element(),
                }))
        }))
}

/// Draws one LED indicator dot + label, matching the "Indicator Lights"
/// row above a full-size keyboard's numpad (num lock / caps lock / scroll
/// lock) — lit green when on, dim gray otherwise.
fn render_lock_indicator(label: &'static str, on: bool) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .size(px(8.0))
                .rounded_full()
                .when(on, |el| el.bg(rgb(0xa3be8c)))
                .when(!on, |el| el.bg(rgb(0x4c566a))),
        )
        .child(
            div()
                .text_size(px(10.0))
                .text_color(if on { rgb(0xeceff4) } else { rgb(0x4c566a) })
                .child(label),
        )
}

/// Total width of `render_numpad`'s output: a true 4-column CSS grid
/// (`+`/`enter` are placed *inside* this same 4-column grid via
/// `col_span`/`row_span`, not in a 5th column next to it — this constant
/// was previously computed as if there were a separate operator column,
/// which overcounted the width by one extra `KEY_UNIT + KEY_GAP` and was
/// what threw off `KEYBOARD_TOTAL_WIDTH` below).
const NUMPAD_WIDTH: f32 = 4.0 * KEY_UNIT + 3.0 * KEY_GAP;

/// Width of `render_function_row`'s output: esc (1) + 3 groups of 4 F-keys
/// + F13 (1), 16px between groups instead of `KEY_GAP` within them — this
/// is the widest row in `main_block` (wider than `number_row`'s 13 unit
/// keys + 1 double-width backspace), so it sets `main_block`'s width.
const FUNCTION_ROW_WIDTH: f32 = KEY_UNIT // esc
    + 16.0
    + 4.0 * KEY_UNIT + 3.0 * KEY_GAP // F1-F4
    + 16.0
    + 4.0 * KEY_UNIT + 3.0 * KEY_GAP // F5-F8
    + 16.0
    + 4.0 * KEY_UNIT + 3.0 * KEY_GAP // F9-F12
    + 16.0
    + KEY_UNIT; // F13

/// Width of `system_row` + nav cluster + arrow cluster: 3 unit-width keys
/// in a row.
const NAV_ARROW_WIDTH: f32 = 3.0 * KEY_UNIT + 2.0 * KEY_GAP;

/// The gap `Render::render` puts between `main_block`, the nav/arrow
/// block, and the numpad block.
const BLOCK_GAP: f32 = KEY_UNIT * 0.5;

/// Total width of the full keyboard row (`main_block` + nav/arrow +
/// numpad, with the gaps between them) — every one of these three blocks
/// is present at the `Full100` profile, which is what `som-key` now
/// always starts at (see `keyboard_profile_from_args`), so this is the
/// steady-state width the log row (see `render_log`) is constrained to.
const KEYBOARD_TOTAL_WIDTH: f32 =
    FUNCTION_ROW_WIDTH + BLOCK_GAP + NAV_ARROW_WIDTH + BLOCK_GAP + NUMPAD_WIDTH;

/// The row of 3 LED indicators (num lock / caps lock / scroll lock) drawn
/// above the numpad, centered over its full width — mirrors the
/// "Indicator Lights" cluster on a real full-size keyboard, positioned
/// the same way relative to the numpad.
///
/// Carries the same `mb(KEY_UNIT * 0.1)` margin as `main_block`'s
/// function-row and `nav_arrow_block`'s system-row, so the numpad's
/// second row (`7 8 9`) lines up with `number_row` like the rest of the
/// keyboard, instead of starting 0.1 unit too high.
fn render_lock_indicators(state: &SomKey) -> impl IntoElement {
    div()
        .w(px(NUMPAD_WIDTH))
        .h(px(KEY_UNIT))
        .flex()
        .items_center()
        .justify_center()
        .gap_3()
        .mb(px(KEY_UNIT * 0.1))
        .child(render_lock_indicator("num", state.numlock_on))
        .child(render_lock_indicator("caps", state.capslock_on))
        .child(render_lock_indicator("scrl", state.scrolllock_on))
}

/// A single numpad grid cell — same visual style as `render_key`, but
/// placed by `render_numpad` via explicit `col_span`/`row_span` instead of
/// being sized by `KeyCell.width` like the rest of the keyboard, since
/// numpad keys only ever span whole grid cells (never fractional widths).
fn render_numpad_key(
    label: &'static str,
    id: KeyId,
    col_span: u16,
    row_span: u16,
    state: &SomKey,
) -> impl IntoElement {
    let pressed = state.is_pressed(id);
    // Long labels (`numlk`) and the NumLock-off nav labels (`home`, `pgup`,
    // the arrow glyphs, etc.) all share the smaller size — the nav labels
    // wouldn't otherwise qualify by length alone (some are a single arrow
    // glyph), so a length check can't drive this by itself.
    let small = label.len() > 4 || matches!(label, "home" | "end" | "pgup" | "pgdn" | "ins" | "del" | "↑" | "↓" | "←" | "→");
    div()
        .col_span(col_span)
        .row_span(row_span)
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .when(pressed, |el| el.bg(rgb(0x88c0d0)).border_color(rgb(0x88c0d0)))
        .when(!pressed, |el| el.bg(rgb(0x2e3440)).border_color(rgb(0x4c566a)))
        .text_color(if pressed { rgb(0x2e3440) } else { rgb(0xd8dee9) })
        .text_size(px(if small { 10.0 } else { 13.0 }))
        .child(label)
}

/// The numpad as a true 4-column x 5-row CSS grid, matching a real numeric
/// keypad's physical layout exactly: `+` and `enter` each span 2 rows
/// (vertical merge of two cells), and `0` spans 2 columns (horizontal
/// merge). A `flex`-based layout can't express this — any attempt to mix
/// a 2-row-tall key into the same row as normal-height ones makes that
/// row's total height the tallest child's height, throwing off every row
/// below it — so this uses GPUI's grid support instead, which places each
/// key by grid line rather than by accumulated flex-row height.
fn render_numpad(state: &SomKey) -> impl IntoElement {
    const COLS: u16 = 4;
    const ROWS: u16 = 5;
    let width = px(COLS as f32 * KEY_UNIT + (COLS - 1) as f32 * KEY_GAP);
    let height = px(ROWS as f32 * KEY_UNIT + (ROWS - 1) as f32 * KEY_GAP);

    // A real numpad's digit keys double as cursor-navigation keys when
    // NumLock is off — the physical key (and its `KeyId`, matched by the
    // same underlying position) never changes, only the printed/displayed
    // function does. `0` becomes Insert and `.` becomes Delete too.
    let (l7, l8, l9, l4, l6, l1, l2, l3, l0, ldot) = if state.numlock_on {
        ("7", "8", "9", "4", "6", "1", "2", "3", "0", ".")
    } else {
        ("home", "↑", "pgup", "←", "→", "end", "↓", "pgdn", "ins", "del")
    };

    div()
        .grid()
        .grid_cols(COLS)
        .grid_rows(ROWS)
        .gap(px(KEY_GAP))
        .w(width)
        .h(height)
        .child(render_numpad_key(NUMPAD_LOCK_LABEL, KeyId::Numpad("numlock"), 1, 1, state))
        .child(render_numpad_key("/", KeyId::Numpad("/"), 1, 1, state))
        .child(render_numpad_key("*", KeyId::Numpad("*"), 1, 1, state))
        .child(render_numpad_key("-", KeyId::Numpad("-"), 1, 1, state))
        .child(render_numpad_key(l7, KeyId::Numpad("7"), 1, 1, state))
        .child(render_numpad_key(l8, KeyId::Numpad("8"), 1, 1, state))
        .child(render_numpad_key(l9, KeyId::Numpad("9"), 1, 1, state))
        .child(render_numpad_key("+", KeyId::Numpad("+"), 1, 2, state))
        .child(render_numpad_key(l4, KeyId::Numpad("4"), 1, 1, state))
        .child(render_numpad_key("5", KeyId::Numpad("5"), 1, 1, state))
        .child(render_numpad_key(l6, KeyId::Numpad("6"), 1, 1, state))
        .child(render_numpad_key(l1, KeyId::Numpad("1"), 1, 1, state))
        .child(render_numpad_key(l2, KeyId::Numpad("2"), 1, 1, state))
        .child(render_numpad_key(l3, KeyId::Numpad("3"), 1, 1, state))
        .child(render_numpad_key("enter", KeyId::Numpad("enter"), 1, 2, state))
        .child(render_numpad_key(l0, KeyId::Numpad("0"), 2, 1, state))
        .child(render_numpad_key(ldot, KeyId::Numpad("."), 1, 1, state))
}

impl Render for SomKey {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_function_row = self.profile >= KeyboardProfile::Tkl80;
        let has_numpad = self.profile >= KeyboardProfile::Full100;

        let mut main_block = div().flex().flex_col().gap(px(KEY_GAP));
        if has_function_row {
            main_block = main_block.child(render_function_row(self).mb(px(KEY_UNIT * 0.1)));
        }
        let main_block = main_block
            .child(render_row(number_row(), self))
            .child(render_row(qwerty_row(), self))
            .child(render_row(asdf_row(), self))
            .child(render_row(zxcv_row(), self))
            .child(render_row(bottom_row(), self));

        // Row heights line up with the main block: prt sc/scr lk/pause sits
        // level with the function row, then nav cluster + arrows sit level
        // with the number/qwerty rows — same as a real keyboard. Both the
        // system row and the nav cluster/arrows exist on TKL and 100%
        // boards alike; only the numpad itself is 100%-only. A board with
        // no F-row also has none of this (that's the 60% definition).
        let nav_arrow_block = if has_function_row {
            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(KEY_GAP))
                    .child(render_row(system_row(), self).mb(px(KEY_UNIT * 0.1)))
                    .child(render_nav_cluster(self))
                    .child(div().h(px(KEY_UNIT)))
                    .child(render_arrow_cluster(self)),
            )
        } else {
            None
        };

        let numpad_block = has_numpad.then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(KEY_GAP))
                .child(render_lock_indicators(self))
                .child(render_numpad(self))
        });

        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("SomKey")
            .capture_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .on_modifiers_changed(cx.listener(Self::modifiers_changed))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e222a))
            .p_4()
            .gap_3()
            .text_color(rgb(0xeceff4))
            .font_family("Menlo")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(KEY_GAP))
                    .child(
                        div()
                            .flex()
                            .gap(px(KEY_UNIT * 0.5))
                            .child(main_block)
                            .children(nav_arrow_block)
                            .children(numpad_block),
                    )
                    .child(render_log(&self.log)),
            )
    }
}

/// How many side-by-side columns the event log fills (see `render_log`).
const LOG_COLUMNS: usize = 3;

/// Gap between the log's columns.
const LOG_COLUMN_GAP: f32 = 16.0;

/// Fixed width of each log column: `KEYBOARD_TOTAL_WIDTH` minus the gaps
/// between columns, divided evenly.
const LOG_COLUMN_WIDTH: f32 =
    (KEYBOARD_TOTAL_WIDTH - (LOG_COLUMNS as f32 - 1.0) * LOG_COLUMN_GAP) / LOG_COLUMNS as f32;

/// Renders the event log as `LOG_COLUMNS` side-by-side columns that fill
/// top to bottom: newest entries start at the top of the first column, and
/// once that column is full, older entries continue at the top of the
/// next one — so all columns read newest-first, left-to-right, matching
/// how someone visually scans a multi-column list rather than a single
/// tall one that needs scrolling.
///
/// Both the log and each column have an EXPLICIT fixed width
/// (`KEYBOARD_TOTAL_WIDTH` / `LOG_COLUMN_WIDTH`) rather than `w_full()` +
/// `flex_1()` — letting the log inherit its parent's width dynamically
/// repeatedly produced a mismatch with the keyboard's actual on-screen
/// width (the parent's own width was itself ambiguous under
/// `align-items: flex-start` with a `w_full()` child, and resolved
/// against the window instead of the sibling keyboard row). A fixed
/// width computed from the same per-key constants the keyboard itself is
/// built from is unambiguous by construction — `overflow_hidden()` (see
/// `render_log_row`) still clips any single row whose content is wider
/// than a column, so long unbroken content (e.g. `0xNN` scan codes)
/// cannot silently re-widen the column past its fixed size.
fn render_log(log: &[LogRow]) -> impl IntoElement {
    let newest_first: Vec<&LogRow> = log.iter().rev().collect();
    let column_len = newest_first.len().div_ceil(LOG_COLUMNS).max(1);

    let column = |rows: &[(usize, &&LogRow)]| {
        div()
            .flex()
            .flex_col()
            .w(px(LOG_COLUMN_WIDTH))
            .gap_1()
            .children(rows.iter().map(|(index, row)| render_log_row(*index + 1, row)))
    };

    div()
        .flex()
        .w(px(KEYBOARD_TOTAL_WIDTH))
        .gap(px(LOG_COLUMN_GAP))
        .mt_2()
        .pt_2()
        .border_t_1()
        .border_color(rgb(0x4c566a))
        .text_size(px(11.0))
        .overflow_hidden()
        .children(
            newest_first
                .iter()
                .enumerate()
                .collect::<Vec<_>>()
                .chunks(column_len)
                .map(|chunk| column(chunk)),
        )
}

const BRIGHT: u32 = 0xeceff4;
const DIM: u32 = 0x4c566a;
/// Same cyan used to highlight a pressed key on the drawn keyboard (see
/// `render_key`'s `pressed` branch) — held modifiers reuse it here so the
/// log's "what's held" reads consistently with the keyboard itself.
const HELD: u32 = 0x88c0d0;

/// The platform modifier's name in the log columns — matches the label
/// `bottom_row` draws on the key itself for the same modifier (command on
/// macOS, win on Windows, super elsewhere).
#[cfg(target_os = "macos")]
const PLATFORM_MODIFIER_LABEL: &str = "CMD";
#[cfg(target_os = "windows")]
const PLATFORM_MODIFIER_LABEL: &str = "WIN";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const PLATFORM_MODIFIER_LABEL: &str = "SUPER";

/// One log row, prefixed with its 1-based position among the newest
/// `MAX_LOG_LINES` keypresses (01 = newest), then: SHIFT CTRL ALT
/// <platform modifier> (cyan when held for this keypress, dim otherwise),
/// then the key name and its scan code — see `LogRow`.
fn render_log_row(number: usize, row: &LogRow) -> impl IntoElement {
    div()
        .flex()
        .min_w(px(0.))
        .overflow_hidden()
        .gap_2()
        .child(div().w(px(22.0)).text_color(rgb(DIM)).child(format!("{number:02}")))
        .child(div().w(px(48.0)).text_color(if row.shift { rgb(HELD) } else { rgb(DIM) }).child("SHIFT"))
        .child(div().w(px(40.0)).text_color(if row.ctrl { rgb(HELD) } else { rgb(DIM) }).child("CTRL"))
        .child(div().w(px(36.0)).text_color(if row.alt { rgb(HELD) } else { rgb(DIM) }).child("ALT"))
        .child(div().w(px(48.0)).text_color(if row.cmd { rgb(HELD) } else { rgb(DIM) }).child(PLATFORM_MODIFIER_LABEL))
        // Key name and scan code are grouped with their own small gap so
        // they read as a pair, instead of picking up the same wide gap
        // used to separate the modifier indicators from each other.
        .child(
            div()
                .flex()
                .gap_2()
                .child(
                    div()
                        .w(px(36.0))
                        .text_color(rgb(BRIGHT))
                        .child(row.scan_code.map_or_else(|| "-".to_string(), |c| format!("0x{c:02X}"))),
                )
                .child(
                    div().text_color(rgb(BRIGHT)).child(if row.key_name.is_empty() {
                        "<empty>".to_string()
                    } else {
                        row.key_name.to_string()
                    }),
                ),
        )
}

fn main() {
    application().run(|cx: &mut App| {
        // Sized to fit the content with only a small margin, not a
        // generic default — a window much larger than the keyboard row +
        // log leaves visible empty space on the right and at the bottom.
        // Width: KEYBOARD_TOTAL_WIDTH plus the root's p_4() padding
        // (16px each side = 32px) plus a small margin for rounding/border
        // slop. Height: keyboard row (6 main-block rows, or equivalently
        // 5 numpad rows + the lock-indicator row, whichever is taller)
        // plus the log (10 rows of 11px text plus its own borders/margins)
        // plus the root's own padding and inter-block gap.
        let bounds = Bounds::centered(
            None,
            size(px(KEYBOARD_TOTAL_WIDTH + 40.0), px(560.0)),
            cx,
        );
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(SomKey::new),
            )
            .unwrap();

        // Grab keyboard focus immediately — without this, GPUI leaves the
        // window with no focused element until the user clicks into it,
        // so no key_down/key_up/modifiers_changed events reach `SomKey`
        // at all until then (the window looks alive — it paints, it
        // responds to the OS chrome — but every key press is silently
        // dropped, which reads exactly like "toggle keys stopped
        // working" if you happen to test one of those first).
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.focus_handle(cx), cx);
            })
            .ok();

        // Populate labels for whatever layout is active at startup — not
        // just future switches — so e.g. launching straight into a Russian
        // layout draws Cyrillic immediately instead of QWERTY placeholders.
        window
            .update(cx, |view, _, cx| view.on_layout_changed(cx))
            .ok();

        // Read the real lock-key state at launch instead of assuming
        // everything starts off — the user may already have CapsLock,
        // NumLock, or ScrollLock toggled on before som-key even opens.
        window
            .update(cx, |view, window, cx| {
                view.numlock_on = window.numlock().on;
                view.capslock_on = window.capslock().on;
                view.scrolllock_on = window.scrolllock().on;
                cx.notify();
            })
            .ok();

        // Catches layout switches made through the system input-source menu
        // (not a keypress at all) as well as ones that happen between
        // keypresses — GPUI's platform layer already watches for this on
        // every backend, so no polling is needed on top of it.
        //
        // On macOS this fires on `NSTextInputContextKeyboardSelectionDidChangeNotification`,
        // which in practice can also fire for reasons that aren't actually
        // a layout change (e.g. focus/input-context churn) — so we compare
        // the reported layout id against the last one seen and only reset
        // `dynamic_labels` when it actually differs, rather than trusting
        // every callback invocation.
        let mut last_layout_id = cx.keyboard_layout().id().to_string();
        cx.on_keyboard_layout_change({
            move |cx| {
                let new_id = cx.keyboard_layout().id().to_string();
                if new_id == last_layout_id {
                    return;
                }
                let _old_id = std::mem::replace(&mut last_layout_id, new_id.clone());
                window
                    .update(cx, |view, _, cx| {
                        view.on_layout_changed(cx);
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();

        cx.activate(true);
    });
}
