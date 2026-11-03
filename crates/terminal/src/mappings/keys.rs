use std::borrow::Cow;

/// The mappings defined in this file where created from reading the alacritty source
use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// The key that was pressed, independent of the keyboard layout: the USB HID
/// usage reported by the platform (`Keystroke.physical_key`) when there is
/// one, otherwise `Keystroke.key`. A Russian "с" on the C key is still `c`.
fn base_key(keystroke: &Keystroke) -> Cow<'_, str> {
    if let Some(name) = keystroke.physical_key.and_then(hid_key_name) {
        return Cow::Borrowed(name);
    }
    if keystroke.key.chars().count() > 1 {
        Cow::Owned(keystroke.key.to_ascii_lowercase())
    } else {
        Cow::Borrowed(keystroke.key.as_str())
    }
}

/// Whether shift was held. The platforms drop `modifiers.shift` for digits and
/// punctuation and report the shifted character in `key` instead (`:` for
/// shift-;), so for those it is recovered from `key` against the physical key.
fn shift_held(keystroke: &Keystroke, base: &str) -> bool {
    if keystroke.modifiers.shift {
        return true;
    }
    let mut key = keystroke.key.chars();
    let mut base_chars = base.chars();
    let (Some(k), None, Some(b), None) = (key.next(), key.next(), base_chars.next(), base_chars.next())
    else {
        return false;
    };
    if k == b || b.is_ascii_alphabetic() {
        return false;
    }
    // the layout's own upper-case letter on this key, or the US shifted symbol
    let us_shifted = match b {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        '`' => '~',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        _ => return false,
    };
    k == us_shifted || (k.is_alphabetic() && k.is_uppercase())
}

/// Base (unshifted, US-QWERTY) name of a USB HID keyboard usage. Keypad keys
/// and anything not listed return `None` and fall back to `Keystroke.key`.
fn hid_key_name(usage: u32) -> Option<&'static str> {
    const LETTERS: &str = "abcdefghijklmnopqrstuvwxyz";
    const DIGITS: &str = "123456789";
    const FUNCTION: [&str; 12] = [
        "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
    ];
    Some(match usage {
        0x04..=0x1d => &LETTERS[(usage - 0x04) as usize..(usage - 0x03) as usize],
        0x1e..=0x26 => &DIGITS[(usage - 0x1e) as usize..(usage - 0x1d) as usize],
        0x27 => "0",
        0x28 => "enter",
        0x29 => "escape",
        0x2a => "backspace",
        0x2b => "tab",
        0x2c => "space",
        0x2d => "-",
        0x2e => "=",
        0x2f => "[",
        0x30 => "]",
        0x31 => "\\",
        0x33 => ";",
        0x34 => "'",
        0x35 => "`",
        0x36 => ",",
        0x37 => ".",
        0x38 => "/",
        0x3a..=0x45 => FUNCTION[(usage - 0x3a) as usize],
        0x49 => "insert",
        0x4a => "home",
        0x4b => "pageup",
        0x4c => "delete",
        0x4d => "end",
        0x4e => "pagedown",
        0x4f => "right",
        0x50 => "left",
        0x51 => "down",
        0x52 => "up",
        _ => return None,
    })
}

/// Keys with a fixed sequence when pressed without modifiers.
fn plain_special(key: &str, mode: &TermMode) -> Option<&'static str> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    Some(match key {
        "home" if app_cursor => "\x1bOH",
        "home" => "\x1b[H",
        "end" if app_cursor => "\x1bOF",
        "end" => "\x1b[F",
        "up" if app_cursor => "\x1bOA",
        "up" => "\x1b[A",
        "down" if app_cursor => "\x1bOB",
        "down" => "\x1b[B",
        "right" if app_cursor => "\x1bOC",
        "right" => "\x1b[C",
        "left" if app_cursor => "\x1bOD",
        "left" => "\x1b[D",
        "back" => "\x7f",
        "insert" => "\x1b[2~",
        "delete" => "\x1b[3~",
        "pageup" => "\x1b[5~",
        "pagedown" => "\x1b[6~",
        "f1" => "\x1bOP",
        "f2" => "\x1bOQ",
        "f3" => "\x1bOR",
        "f4" => "\x1bOS",
        "f5" => "\x1b[15~",
        "f6" => "\x1b[17~",
        "f7" => "\x1b[18~",
        "f8" => "\x1b[19~",
        "f9" => "\x1b[20~",
        "f10" => "\x1b[21~",
        "f11" => "\x1b[23~",
        "f12" => "\x1b[24~",
        "f13" => "\x1b[25~",
        "f14" => "\x1b[26~",
        "f15" => "\x1b[28~",
        "f16" => "\x1b[29~",
        "f17" => "\x1b[31~",
        "f18" => "\x1b[32~",
        "f19" => "\x1b[33~",
        "f20" => "\x1b[34~",
        _ => return None,
    })
}

/// xterm sequences for cursor, editing and function keys with modifiers.
fn modified_special(key: &str, keystroke: &Keystroke) -> Option<String> {
    let code = modifier_code(keystroke);
    Some(match key {
        "up" => format!("\x1b[1;{code}A"),
        "down" => format!("\x1b[1;{code}B"),
        "right" => format!("\x1b[1;{code}C"),
        "left" => format!("\x1b[1;{code}D"),
        "f1" => format!("\x1b[1;{code}P"),
        "f2" => format!("\x1b[1;{code}Q"),
        "f3" => format!("\x1b[1;{code}R"),
        "f4" => format!("\x1b[1;{code}S"),
        "f5" => format!("\x1b[15;{code}~"),
        "f6" => format!("\x1b[17;{code}~"),
        "f7" => format!("\x1b[18;{code}~"),
        "f8" => format!("\x1b[19;{code}~"),
        "f9" => format!("\x1b[20;{code}~"),
        "f10" => format!("\x1b[21;{code}~"),
        "f11" => format!("\x1b[23;{code}~"),
        "f12" => format!("\x1b[24;{code}~"),
        "f13" => format!("\x1b[25;{code}~"),
        "f14" => format!("\x1b[26;{code}~"),
        "f15" => format!("\x1b[28;{code}~"),
        "f16" => format!("\x1b[29;{code}~"),
        "f17" => format!("\x1b[31;{code}~"),
        "f18" => format!("\x1b[32;{code}~"),
        "f19" => format!("\x1b[33;{code}~"),
        "f20" => format!("\x1b[34;{code}~"),
        "insert" => format!("\x1b[2;{code}~"),
        "delete" => format!("\x1b[3;{code}~"),
        "pageup" => format!("\x1b[5;{code}~"),
        "pagedown" => format!("\x1b[6;{code}~"),
        "end" => format!("\x1b[1;{code}F"),
        "home" => format!("\x1b[1;{code}H"),
        _ => return None,
    })
}

/// `CSI <codepoint> ; <modifiers> u` (fixterms/kitty): the encoding for key
/// combinations that have no legacy terminal sequence, so that a program that
/// asks for them can tell them apart.
fn csi_u(codepoint: u32, keystroke: &Keystroke) -> Cow<'static, str> {
    Cow::Owned(format!("\x1b[{};{}u", codepoint, modifier_code(keystroke)))
}

/// Legacy control byte for ctrl + `c` (`c` is the unshifted key), if there is one.
fn ctrl_byte(c: char, shift: bool) -> Option<u8> {
    match c {
        'a'..='z' | 'A'..='Z' => Some(c.to_ascii_lowercase() as u8 - b'a' + 1),
        '2' | '@' => Some(0x00),
        '3' => (!shift).then_some(0x1b),
        '4' => (!shift).then_some(0x1c),
        '5' => (!shift).then_some(0x1d),
        '6' | '^' => Some(0x1e),
        '7' => (!shift).then_some(0x1f),
        '8' => (!shift).then_some(0x7f),
        '`' => (!shift).then_some(0x00),
        '[' => (!shift).then_some(0x1b),
        '\\' => (!shift).then_some(0x1c),
        ']' => (!shift).then_some(0x1d),
        '-' | '/' | '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

pub fn to_esc_str(
    keystroke: &Keystroke,
    mode: &TermMode,
    option_as_meta: bool,
) -> Option<Cow<'static, str>> {
    let name = base_key(keystroke);
    let key = name.as_ref();
    let shift = if keystroke.physical_key.is_some() {
        shift_held(keystroke, key)
    } else {
        keystroke.modifiers.shift
    };
    let (alt, ctrl) = (keystroke.modifiers.alt, keystroke.modifiers.control);
    let mut with_shift = keystroke.clone();
    with_shift.modifiers.shift = shift;
    let keystroke = &with_shift;
    let any = shift || alt || ctrl;

    // The platform key (Win / Cmd) has no terminal encoding of its own; the
    // modifier-aware sequences below simply ignore it.
    if keystroke.modifiers.platform {
        return modified_special(key, keystroke).map(Cow::Owned);
    }

    // AltGr arrives as ctrl+alt and produces text: let the text input have it.
    if ctrl && alt && keystroke.key_char.is_some() {
        return None;
    }

    // On macOS, option composes characters unless it is told to act as meta.
    let alt_is_meta = !cfg!(target_os = "macos") || option_as_meta;

    let fixed: Option<&'static str> = match (key, shift, alt, ctrl) {
        ("enter", false, false, false) => Some("\x0d"),
        // A terminal cannot tell ctrl-enter from ctrl-j; programs read LF as ctrl-enter.
        ("enter", true, false, false) | ("enter", false, false, true) => Some("\x0a"),
        ("enter", false, true, false) => Some("\x1b\x0d"),
        ("tab", false, false, false) => Some("\x09"),
        ("tab", true, false, false) => Some("\x1b[Z"),
        ("escape", false, false, false) => Some("\x1b"),
        ("escape", false, true, false) => Some("\x1b\x1b"),
        ("backspace", false, false, false) | ("backspace", true, false, false) => Some("\x7f"),
        ("backspace", false, false, true) => Some("\x08"),
        ("backspace", false, true, false) => Some("\x1b\x7f"),
        ("backspace", false, true, true) => Some("\x1b\x08"),
        ("space", _, false, true) => Some("\x00"),
        ("space", _, true, true) => Some("\x1b\x00"),
        ("space", _, true, false) if alt_is_meta => Some("\x1b "),
        _ => None,
    };
    if let Some(esc_str) = fixed {
        return Some(Cow::Borrowed(esc_str));
    }

    if !any {
        return plain_special(key, mode).map(Cow::Borrowed);
    }

    if let Some(esc_str) = modified_special(key, keystroke) {
        return Some(Cow::Owned(esc_str));
    }

    // Named keys that have no legacy sequence for this modifier combination.
    match key {
        "enter" => return Some(csi_u(13, keystroke)),
        "tab" => return Some(csi_u(9, keystroke)),
        "escape" => return Some(csi_u(27, keystroke)),
        "backspace" => return Some(csi_u(127, keystroke)),
        "space" => return Some(csi_u(32, keystroke)),
        _ => {}
    }

    // Printable keys: letters, digits, punctuation.
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if !ctrl && !alt {
        // shift alone is plain text
        return None;
    }
    if alt && !alt_is_meta {
        return None;
    }
    let base = c.to_ascii_lowercase();

    if ctrl {
        let Some(byte) = ctrl_byte(c, shift) else {
            return Some(csi_u(base as u32, keystroke));
        };
        let control = char::from(byte);
        return Some(Cow::Owned(if alt {
            format!("\x1b{control}")
        } else {
            control.to_string()
        }));
    }

    // alt without ctrl: meta prefix
    if shift && !c.is_ascii_alphabetic() {
        return Some(csi_u(base as u32, keystroke));
    }
    let shown = if shift { c.to_ascii_uppercase() } else { c };
    Some(Cow::Owned(format!("\x1b{shown}")))
}

///   Code     Modifiers
/// ---------+---------------------------
///    2     | Shift
///    3     | Alt
///    4     | Shift + Alt
///    5     | Control
///    6     | Shift + Control
///    7     | Alt + Control
///    8     | Shift + Alt + Control
/// ---------+---------------------------
/// from: https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h2-PC-Style-Function-Keys
fn modifier_code(keystroke: &Keystroke) -> u32 {
    let mut modifier_code = 0;
    if keystroke.modifiers.shift {
        modifier_code |= 1;
    }
    if keystroke.modifiers.alt {
        modifier_code |= 1 << 1;
    }
    if keystroke.modifiers.control {
        modifier_code |= 1 << 2;
    }
    modifier_code + 1
}

#[cfg(test)]
mod test {
    use gpui::Modifiers;

    use super::*;

    #[test]
    fn test_plain_inputs() {
        let ks = Keystroke {
            modifiers: Modifiers {
                control: false,
                alt: false,
                shift: false,
                platform: false,
                function: false,
                ..Default::default()
            },
            key: "🖖🏻".to_string(), //2 char string
            key_char: None,
            ..Default::default()
        };
        assert_eq!(to_esc_str(&ks, &TermMode::NONE, false), None);
    }

    #[test]
    fn test_application_mode() {
        let app_cursor = TermMode::APP_CURSOR;
        let none = TermMode::NONE;

        let up = Keystroke::parse("up").unwrap();
        let down = Keystroke::parse("down").unwrap();
        let left = Keystroke::parse("left").unwrap();
        let right = Keystroke::parse("right").unwrap();

        assert_eq!(to_esc_str(&up, &none, false), Some("\x1b[A".into()));
        assert_eq!(to_esc_str(&down, &none, false), Some("\x1b[B".into()));
        assert_eq!(to_esc_str(&right, &none, false), Some("\x1b[C".into()));
        assert_eq!(to_esc_str(&left, &none, false), Some("\x1b[D".into()));

        assert_eq!(to_esc_str(&up, &app_cursor, false), Some("\x1bOA".into()));
        assert_eq!(to_esc_str(&down, &app_cursor, false), Some("\x1bOB".into()));
        assert_eq!(
            to_esc_str(&right, &app_cursor, false),
            Some("\x1bOC".into())
        );
        assert_eq!(to_esc_str(&left, &app_cursor, false), Some("\x1bOD".into()));

        let home = Keystroke::parse("home").unwrap();
        let end = Keystroke::parse("end").unwrap();
        assert_eq!(to_esc_str(&home, &none, false), Some("\x1b[H".into()));
        assert_eq!(to_esc_str(&end, &none, false), Some("\x1b[F".into()));
        assert_eq!(to_esc_str(&home, &app_cursor, false), Some("\x1bOH".into()));
        assert_eq!(to_esc_str(&end, &app_cursor, false), Some("\x1bOF".into()));

        let shift_up = Keystroke::parse("shift-up").unwrap();
        let shift_down = Keystroke::parse("shift-down").unwrap();
        let shift_home = Keystroke::parse("shift-home").unwrap();
        let shift_end = Keystroke::parse("shift-end").unwrap();
        assert_eq!(
            to_esc_str(&shift_up, &none, false),
            Some("\x1b[1;2A".into())
        );
        assert_eq!(
            to_esc_str(&shift_down, &none, false),
            Some("\x1b[1;2B".into())
        );
        assert_eq!(
            to_esc_str(&shift_home, &none, false),
            Some("\x1b[1;2H".into())
        );
        assert_eq!(
            to_esc_str(&shift_end, &none, false),
            Some("\x1b[1;2F".into())
        );
    }

    #[test]
    fn test_ctrl_codes() {
        let letters_lower = 'a'..='z';
        let letters_upper = 'A'..='Z';
        let mode = TermMode::ANY;

        for (lower, upper) in letters_lower.zip(letters_upper) {
            assert_eq!(
                to_esc_str(
                    &Keystroke::parse(&format!("ctrl-shift-{}", lower)).unwrap(),
                    &mode,
                    false
                ),
                to_esc_str(
                    &Keystroke::parse(&format!("ctrl-{}", upper)).unwrap(),
                    &mode,
                    false
                ),
                "On letter: {}/{}",
                lower,
                upper
            )
        }
    }

    #[test]
    fn alt_is_meta() {
        let ascii_printable = ' '..='~';
        for character in ascii_printable {
            assert_eq!(
                to_esc_str(
                    &Keystroke::parse(&format!("alt-{}", character)).unwrap(),
                    &TermMode::NONE,
                    true
                )
                .unwrap(),
                format!("\x1b{}", character)
            );
        }

        let gpui_keys = [
            "up", "down", "right", "left", "f1", "f2", "f3", "f4", "F5", "f6", "f7", "f8", "f9",
            "f10", "f11", "f12", "f13", "f14", "f15", "f16", "f17", "f18", "f19", "f20", "insert",
            "pageup", "pagedown", "end", "home",
        ];

        for key in gpui_keys {
            assert_ne!(
                to_esc_str(
                    &Keystroke::parse(&format!("alt-{}", key)).unwrap(),
                    &TermMode::NONE,
                    true
                )
                .unwrap(),
                format!("\x1b{}", key)
            );
        }
    }

    fn esc(source: &str) -> Option<String> {
        to_esc_str(&Keystroke::parse(source).unwrap(), &TermMode::NONE, true).map(|s| s.into_owned())
    }

    /// A keystroke as the platform reports it under a non-Latin layout: the
    /// layout's character in `key`, the physical key in `physical_key`.
    fn physical(key: &str, usage: u32, ctrl: bool, alt: bool, shift: bool) -> Option<String> {
        let ks = Keystroke {
            modifiers: Modifiers {
                control: ctrl,
                alt,
                shift,
                ..Default::default()
            },
            key: key.to_string(),
            key_char: None,
            physical_key: Some(usage),
            ..Default::default()
        };
        to_esc_str(&ks, &TermMode::NONE, true).map(|s| s.into_owned())
    }

    #[test]
    fn keys_not_characters() {
        // Russian layout: "с" on the C key, "х" on the [ key, "ж" on the ; key.
        assert_eq!(physical("с", 0x06, true, false, false).as_deref(), Some("\x03"));
        assert_eq!(physical("с", 0x06, true, false, true).as_deref(), Some("\x03"));
        assert_eq!(physical("с", 0x06, false, true, false).as_deref(), Some("\x1bc"));
        assert_eq!(physical("с", 0x06, true, true, false).as_deref(), Some("\x1b\x03"));
        assert_eq!(physical("х", 0x2f, true, false, false).as_deref(), Some("\x1b"));
        assert_eq!(physical("ж", 0x33, true, false, false).as_deref(), Some("\x1b[59;5u"));
        // shift dropped by the platform and folded into `key`
        assert_eq!(physical(":", 0x33, true, false, false).as_deref(), Some("\x1b[59;6u"));
        assert_eq!(physical("Ж", 0x33, true, false, false).as_deref(), Some("\x1b[59;6u"));
        assert_eq!(physical("@", 0x1f, true, false, false).as_deref(), Some("\x00"));
        assert_eq!(physical("!", 0x1e, false, true, false).as_deref(), Some("\x1b[49;4u"));
        assert_eq!(physical("1", 0x1e, false, true, false).as_deref(), Some("\x1b1"));
        // plain and shifted typing stays text
        assert_eq!(physical("с", 0x06, false, false, false), None);
        assert_eq!(physical("С", 0x06, false, false, true), None);
    }

    #[test]
    fn every_modifier_combination_is_sent() {
        let mods = ["shift", "ctrl", "alt", "ctrl-shift", "alt-shift", "ctrl-alt", "ctrl-alt-shift"];
        let mut keys: Vec<String> = ('a'..='z').map(|c| c.to_string()).collect();
        keys.extend(('0'..='9').map(|c| c.to_string()));
        keys.extend(["-", "=", "[", "]", "\\", ";", "'", ",", ".", "/", "`"].map(String::from));
        let named = [
            "space", "enter", "tab", "escape", "backspace", "delete", "insert", "home", "end",
            "pageup", "pagedown", "up", "down", "left", "right",
        ];
        keys.extend(named.map(String::from));
        keys.extend((1..=12).map(|i| format!("f{i}")));
        for key in &keys {
            for m in mods {
                let printable = key.chars().count() == 1 || key == "space";
                if m == "shift" && printable {
                    // shifted characters are typed text
                    continue;
                }
                assert!(esc(&format!("{m}-{key}")).is_some(), "{m}-{key} sends nothing");
            }
        }
    }

    #[test]
    fn legacy_sequences() {
        assert_eq!(esc("ctrl-enter").as_deref(), Some("\x0a"));
        assert_eq!(esc("ctrl-shift-enter").as_deref(), Some("\x1b[13;6u"));
        assert_eq!(esc("ctrl-shift-a").as_deref(), Some("\x01"));
        assert_eq!(esc("ctrl-alt-a").as_deref(), Some("\x1b\x01"));
        assert_eq!(esc("ctrl-2").as_deref(), Some("\x00"));
        assert_eq!(esc("ctrl-6").as_deref(), Some("\x1e"));
        assert_eq!(esc("ctrl-1").as_deref(), Some("\x1b[49;5u"));
        assert_eq!(esc("ctrl-/").as_deref(), Some("\x1f"));
        assert_eq!(esc("ctrl-delete").as_deref(), Some("\x1b[3;5~"));
        assert_eq!(esc("shift-delete").as_deref(), Some("\x1b[3;2~"));
        assert_eq!(esc("shift-f5").as_deref(), Some("\x1b[15;2~"));
        assert_eq!(esc("ctrl-f5").as_deref(), Some("\x1b[15;5~"));
        assert_eq!(esc("alt-escape").as_deref(), Some("\x1b\x1b"));
        assert_eq!(esc("ctrl-tab").as_deref(), Some("\x1b[9;5u"));
        assert_eq!(esc("ctrl-alt-shift-a").as_deref(), Some("\x1b\x01"));
    }

    #[test]
    fn altgr_text_is_left_to_text_input() {
        let ks = Keystroke {
            modifiers: Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
            key: "q".to_string(),
            key_char: Some("@".to_string()),
            physical_key: Some(0x14),
            ..Default::default()
        };
        assert_eq!(to_esc_str(&ks, &TermMode::NONE, false), None);
    }

    #[test]
    fn test_shift_enter_newline() {
        let shift_enter = Keystroke::parse("shift-enter").unwrap();
        let regular_enter = Keystroke::parse("enter").unwrap();
        let mode = TermMode::NONE;

        // Shift-enter should send line feed (newline)
        assert_eq!(to_esc_str(&shift_enter, &mode, false), Some("\x0a".into()));

        // Regular enter should still send carriage return
        assert_eq!(
            to_esc_str(&regular_enter, &mode, false),
            Some("\x0d".into())
        );
    }

    #[test]
    fn test_modifier_code_calc() {
        //   Code     Modifiers
        // ---------+---------------------------
        //    2     | Shift
        //    3     | Alt
        //    4     | Shift + Alt
        //    5     | Control
        //    6     | Shift + Control
        //    7     | Alt + Control
        //    8     | Shift + Alt + Control
        // ---------+---------------------------
        // from: https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h2-PC-Style-Function-Keys
        assert_eq!(2, modifier_code(&Keystroke::parse("shift-a").unwrap()));
        assert_eq!(3, modifier_code(&Keystroke::parse("alt-a").unwrap()));
        assert_eq!(4, modifier_code(&Keystroke::parse("shift-alt-a").unwrap()));
        assert_eq!(5, modifier_code(&Keystroke::parse("ctrl-a").unwrap()));
        assert_eq!(6, modifier_code(&Keystroke::parse("shift-ctrl-a").unwrap()));
        assert_eq!(7, modifier_code(&Keystroke::parse("alt-ctrl-a").unwrap()));
        assert_eq!(
            8,
            modifier_code(&Keystroke::parse("shift-ctrl-alt-a").unwrap())
        );
    }
}
