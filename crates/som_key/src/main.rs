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

/// Which physical keyboard form factor to draw. Selected via a command-line
/// argument (`--60`, `--80`/`--tkl`, `--100`/`--full`) since macOS doesn't
/// hand out a way to ask "what keyboard is this" without an Input
/// Monitoring permission prompt tied to IOHIDManager — not worth the
/// friction for a diagnostic tool. Defaults to TKL/80%, the most common
/// desk keyboard size.
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
    KeyboardProfile::Tkl80
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

fn npkey(label: &'static str, key: &'static str) -> KeyCell {
    KeyCell { label, id: KeyId::Numpad(key), width: 1.0, small_text: false }
}

fn npkey_w(label: &'static str, key: &'static str, width: f32) -> KeyCell {
    KeyCell { label, id: KeyId::Numpad(key), width, small_text: false }
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
        wkey("space", "space", 5.6),
        modifier("alt", ModifierKind::Alt, Side::Right, 1.3),
        modifier("win", ModifierKind::Platform, Side::Right, 1.3),
        modifier("ctrl", ModifierKind::Control, Side::Right, 1.3),
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
const NUMPAD_LOCK_LABEL: &str = "num lock";

const MAX_LOG_LINES: usize = 12;

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
        self.numlock_on = event.numlock.on;
        self.capslock_on = event.capslock.on;
        cx.notify();
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
        let is_russian = cx.keyboard_layout().id().contains("Russian");
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
            // CapsLock is a toggle, not a held key — macOS (and other
            // platforms) report it exclusively through `ModifiersChangedEvent`,
            // never `key_down`/`key_up`, so `pressed_keys` never gets an
            // entry for it. Light it up whenever the toggle is on instead.
            KeyId::Char("capslock") => self.capslock_on,
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

fn render_tall_key(label: &'static str, pressed: bool) -> impl IntoElement {
    div()
        .w(px(KEY_UNIT))
        .h(px(2.0 * KEY_UNIT + KEY_GAP))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .when(pressed, |el| el.bg(rgb(0x88c0d0)).border_color(rgb(0x88c0d0)))
        .when(!pressed, |el| el.bg(rgb(0x2e3440)).border_color(rgb(0x4c566a)))
        .text_color(if pressed { rgb(0x2e3440) } else { rgb(0xd8dee9) })
        .text_size(px(10.0))
        .child(label)
}

/// The 4x5 digit/operator grid plus the right-hand `+`/`enter` column that
/// spans 2 rows each, matching a real numeric keypad's physical layout.
fn render_numpad(state: &SomKey) -> impl IntoElement {
    let plus_pressed = state.is_pressed(KeyId::Numpad("+"));
    let enter_pressed = state.is_pressed(KeyId::Numpad("enter"));
    div()
        .flex()
        .flex_col()
        .gap(px(KEY_GAP))
        // Row 1: numlock / * -  (no 4th-column key here — `-` sits alone,
        // `+` starts one row down, directly under it).
        .child(render_row(
            vec![
                npkey(NUMPAD_LOCK_LABEL, "numlock"),
                npkey("/", "/"),
                npkey("*", "*"),
                npkey("-", "-"),
            ],
            state,
        ))
        // Rows 2-3: 7 8 9 / 4 5 6, with `+` spanning both, directly under `-`.
        .child(
            div()
                .flex()
                .gap(px(KEY_GAP))
                .child(render_row(vec![npkey("7", "7"), npkey("8", "8"), npkey("9", "9")], state))
                .child(render_tall_key("+", plus_pressed)),
        )
        .child(render_row(vec![npkey("4", "4"), npkey("5", "5"), npkey("6", "6")], state))
        // Rows 4-5: 1 2 3 / 0 0 ., with `enter` spanning both, directly
        // under `+`.
        .child(
            div()
                .flex()
                .gap(px(KEY_GAP))
                .child(render_row(vec![npkey("1", "1"), npkey("2", "2"), npkey("3", "3")], state))
                .child(render_tall_key("enter", enter_pressed)),
        )
        .child(render_row(vec![npkey_w("0", "0", 2.0), npkey(".", ".")], state))
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
                .child(div().h(px(KEY_UNIT)))
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
                    .gap(px(KEY_UNIT * 0.5))
                    .child(main_block)
                    .children(nav_arrow_block)
                    .children(numpad_block),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap_1()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(rgb(0x4c566a))
                    .text_size(px(11.0))
                    .overflow_hidden()
                    .children(self.log.iter().rev().map(render_log_row)),
            )
    }
}

const BRIGHT: u32 = 0xeceff4;
const DIM: u32 = 0x4c566a;
/// Same cyan used to highlight a pressed key on the drawn keyboard (see
/// `render_key`'s `pressed` branch) — held modifiers reuse it here so the
/// log's "what's held" reads consistently with the keyboard itself.
const HELD: u32 = 0x88c0d0;

/// One log row: SHIFT CTRL ALT CMD (cyan when held for this keypress, dim
/// otherwise), then the key name and its scan code — see `LogRow`.
fn render_log_row(row: &LogRow) -> impl IntoElement {
    div()
        .flex()
        .gap_2()
        .child(div().w(px(48.0)).text_color(if row.shift { rgb(HELD) } else { rgb(DIM) }).child("SHIFT"))
        .child(div().w(px(40.0)).text_color(if row.ctrl { rgb(HELD) } else { rgb(DIM) }).child("CTRL"))
        .child(div().w(px(36.0)).text_color(if row.alt { rgb(HELD) } else { rgb(DIM) }).child("ALT"))
        .child(div().w(px(40.0)).text_color(if row.cmd { rgb(HELD) } else { rgb(DIM) }).child("CMD"))
        .child(
            div().w(px(120.0)).text_color(rgb(BRIGHT)).child(if row.key_name.is_empty() {
                "<empty>".to_string()
            } else {
                row.key_name.to_string()
            }),
        )
        .child(
            div()
                .text_color(rgb(BRIGHT))
                .child(row.scan_code.map_or_else(|| "-".to_string(), |c| format!("0x{c:02X}"))),
        )
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1180.), px(640.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(SomKey::new),
            )
            .unwrap();

        // Populate labels for whatever layout is active at startup — not
        // just future switches — so e.g. launching straight into a Russian
        // layout draws Cyrillic immediately instead of QWERTY placeholders.
        window
            .update(cx, |view, _, cx| view.on_layout_changed(cx))
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
