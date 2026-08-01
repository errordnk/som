use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseExitEvent, MouseMoveEvent, MousePressureEvent, MouseUpEvent,
    NavigationDirection, Numlock, PinchEvent, Pixels, PlatformInput, PressureStage, ScrollDelta,
    ScrollWheelEvent, Scrolllock, Side, TouchPhase, point, px,
};

use crate::{
    LMGetKbdType, NSStringExt, TISCopyCurrentKeyboardLayoutInputSource, TISGetInputSourceProperty,
    UCKeyTranslate, kTISPropertyUnicodeKeyLayoutData,
};
use cocoa::{
    appkit::{NSEvent, NSEventModifierFlags, NSEventPhase, NSEventType},
    base::{YES, id},
};
use core_foundation::data::{CFDataGetBytePtr, CFDataRef};
use core_graphics::event::CGKeyCode;
use objc::{msg_send, sel, sel_impl};
use std::{borrow::Cow, ffi::c_void};

const BACKSPACE_KEY: u16 = 0x7f;
const SPACE_KEY: u16 = b' ' as u16;
const ENTER_KEY: u16 = 0x0d;
const NUMPAD_ENTER_KEY: u16 = 0x03;
pub(crate) const ESCAPE_KEY: u16 = 0x1b;
const TAB_KEY: u16 = 0x09;
const SHIFT_TAB_KEY: u16 = 0x19;

pub fn key_to_native(key: &str) -> Cow<'_, str> {
    use cocoa::appkit::*;
    let code = match key {
        "space" => SPACE_KEY,
        "backspace" => BACKSPACE_KEY,
        "escape" => ESCAPE_KEY,
        "up" => NSUpArrowFunctionKey,
        "down" => NSDownArrowFunctionKey,
        "left" => NSLeftArrowFunctionKey,
        "right" => NSRightArrowFunctionKey,
        "pageup" => NSPageUpFunctionKey,
        "pagedown" => NSPageDownFunctionKey,
        "home" => NSHomeFunctionKey,
        "end" => NSEndFunctionKey,
        "delete" => NSDeleteFunctionKey,
        "insert" => NSHelpFunctionKey,
        "f1" => NSF1FunctionKey,
        "f2" => NSF2FunctionKey,
        "f3" => NSF3FunctionKey,
        "f4" => NSF4FunctionKey,
        "f5" => NSF5FunctionKey,
        "f6" => NSF6FunctionKey,
        "f7" => NSF7FunctionKey,
        "f8" => NSF8FunctionKey,
        "f9" => NSF9FunctionKey,
        "f10" => NSF10FunctionKey,
        "f11" => NSF11FunctionKey,
        "f12" => NSF12FunctionKey,
        "f13" => NSF13FunctionKey,
        "f14" => NSF14FunctionKey,
        "f15" => NSF15FunctionKey,
        "f16" => NSF16FunctionKey,
        "f17" => NSF17FunctionKey,
        "f18" => NSF18FunctionKey,
        "f19" => NSF19FunctionKey,
        "f20" => NSF20FunctionKey,
        "f21" => NSF21FunctionKey,
        "f22" => NSF22FunctionKey,
        "f23" => NSF23FunctionKey,
        "f24" => NSF24FunctionKey,
        "f25" => NSF25FunctionKey,
        "f26" => NSF26FunctionKey,
        "f27" => NSF27FunctionKey,
        "f28" => NSF28FunctionKey,
        "f29" => NSF29FunctionKey,
        "f30" => NSF30FunctionKey,
        "f31" => NSF31FunctionKey,
        "f32" => NSF32FunctionKey,
        "f33" => NSF33FunctionKey,
        "f34" => NSF34FunctionKey,
        "f35" => NSF35FunctionKey,
        _ => return Cow::Borrowed(key),
    };
    Cow::Owned(String::from_utf16(&[code]).unwrap())
}

/// Maps the `NSEvent.keyCode` carried by an `NSFlagsChanged` event to which
/// physical instance (left/right) of that modifier key changed state.
/// These virtual key codes are fixed by the ANSI/ISO/JIS USB HID keyboard
/// usage tables and don't vary by keyboard layout.
fn side_for_flags_changed_key_code(key_code: CGKeyCode) -> Option<Side> {
    match key_code {
        56 | 59 | 58 | 55 => Some(Side::Left),   // Shift, Control, Option, Command
        60 | 62 | 61 | 54 => Some(Side::Right),  // RightShift, RightControl, RightOption, RightCommand
        _ => None,
    }
}

unsafe fn read_modifiers(native_event: id) -> Modifiers {
    unsafe {
        let modifiers = native_event.modifierFlags();
        let control = modifiers.contains(NSEventModifierFlags::NSControlKeyMask);
        let alt = modifiers.contains(NSEventModifierFlags::NSAlternateKeyMask);
        let shift = modifiers.contains(NSEventModifierFlags::NSShiftKeyMask);
        let command = modifiers.contains(NSEventModifierFlags::NSCommandKeyMask);
        let function = modifiers.contains(NSEventModifierFlags::NSFunctionKeyMask);

        Modifiers {
            control,
            alt,
            shift,
            platform: command,
            function,
            ..Default::default()
        }
    }
}

pub(crate) unsafe fn platform_input_from_native(
    native_event: id,
    window_height: Option<Pixels>,
) -> Option<PlatformInput> {
    unsafe {
        let event_type = native_event.eventType();

        // Filter out event types that aren't in the NSEventType enum.
        // See https://github.com/servo/cocoa-rs/issues/155#issuecomment-323482792 for details.
        match event_type as u64 {
            0 | 21 | 32 | 33 | 35 | 36 | 37 => {
                return None;
            }
            _ => {}
        }

        match event_type {
            NSEventType::NSFlagsChanged => {
                let mut modifiers = read_modifiers(native_event);
                modifiers.side = side_for_flags_changed_key_code(native_event.keyCode());
                Some(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers,
                    capslock: Capslock {
                        on: native_event
                            .modifierFlags()
                            .contains(NSEventModifierFlags::NSAlphaShiftKeyMask),
                    },
                    // macOS numeric keypads have a non-toggling "Clear" key,
                    // not a numlock toggle — always off.
                    numlock: Numlock::default(),
                    // macOS keyboards/OS don't expose a scroll lock state.
                    scrolllock: Scrolllock::default(),
                }))
            }
            NSEventType::NSKeyDown => Some(PlatformInput::KeyDown(KeyDownEvent {
                keystroke: parse_keystroke(native_event),
                is_held: native_event.isARepeat() == YES,
                prefer_character_input: false,
            })),
            NSEventType::NSKeyUp => Some(PlatformInput::KeyUp(KeyUpEvent {
                keystroke: parse_keystroke(native_event),
            })),
            NSEventType::NSLeftMouseDown
            | NSEventType::NSRightMouseDown
            | NSEventType::NSOtherMouseDown => {
                let button = match native_event.buttonNumber() {
                    0 => MouseButton::Left,
                    1 => MouseButton::Right,
                    2 => MouseButton::Middle,
                    3 => MouseButton::Navigate(NavigationDirection::Back),
                    4 => MouseButton::Navigate(NavigationDirection::Forward),
                    // Other mouse buttons aren't tracked currently
                    _ => return None,
                };
                window_height.map(|window_height| {
                    PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position: point(
                            px(native_event.locationInWindow().x as f32),
                            // MacOS screen coordinates are relative to bottom left
                            window_height - px(native_event.locationInWindow().y as f32),
                        ),
                        modifiers: read_modifiers(native_event),
                        click_count: native_event.clickCount() as usize,
                        first_mouse: false,
                    })
                })
            }
            NSEventType::NSLeftMouseUp
            | NSEventType::NSRightMouseUp
            | NSEventType::NSOtherMouseUp => {
                let button = match native_event.buttonNumber() {
                    0 => MouseButton::Left,
                    1 => MouseButton::Right,
                    2 => MouseButton::Middle,
                    3 => MouseButton::Navigate(NavigationDirection::Back),
                    4 => MouseButton::Navigate(NavigationDirection::Forward),
                    // Other mouse buttons aren't tracked currently
                    _ => return None,
                };

                window_height.map(|window_height| {
                    PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position: point(
                            px(native_event.locationInWindow().x as f32),
                            window_height - px(native_event.locationInWindow().y as f32),
                        ),
                        modifiers: read_modifiers(native_event),
                        click_count: native_event.clickCount() as usize,
                    })
                })
            }
            NSEventType::NSEventTypePressure => {
                let stage = native_event.stage();
                let pressure = native_event.pressure();

                window_height.map(|window_height| {
                    PlatformInput::MousePressure(MousePressureEvent {
                        stage: match stage {
                            1 => PressureStage::Normal,
                            2 => PressureStage::Force,
                            _ => PressureStage::Zero,
                        },
                        pressure,
                        modifiers: read_modifiers(native_event),
                        position: point(
                            px(native_event.locationInWindow().x as f32),
                            window_height - px(native_event.locationInWindow().y as f32),
                        ),
                    })
                })
            }
            // Some mice (like Logitech MX Master) send navigation buttons as swipe events
            NSEventType::NSEventTypeSwipe => {
                let navigation_direction = match native_event.phase() {
                    NSEventPhase::NSEventPhaseEnded => match native_event.deltaX() {
                        x if x > 0.0 => Some(NavigationDirection::Back),
                        x if x < 0.0 => Some(NavigationDirection::Forward),
                        _ => return None,
                    },
                    _ => return None,
                };

                match navigation_direction {
                    Some(direction) => window_height.map(|window_height| {
                        PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Navigate(direction),
                            position: point(
                                px(native_event.locationInWindow().x as f32),
                                window_height - px(native_event.locationInWindow().y as f32),
                            ),
                            modifiers: read_modifiers(native_event),
                            click_count: 1,
                            first_mouse: false,
                        })
                    }),
                    _ => None,
                }
            }
            NSEventType::NSEventTypeMagnify => window_height.map(|window_height| {
                let phase = match native_event.phase() {
                    NSEventPhase::NSEventPhaseMayBegin | NSEventPhase::NSEventPhaseBegan => {
                        TouchPhase::Started
                    }
                    NSEventPhase::NSEventPhaseEnded => TouchPhase::Ended,
                    _ => TouchPhase::Moved,
                };

                let magnification = native_event.magnification() as f32;

                PlatformInput::Pinch(PinchEvent {
                    position: point(
                        px(native_event.locationInWindow().x as f32),
                        window_height - px(native_event.locationInWindow().y as f32),
                    ),
                    delta: magnification,
                    modifiers: read_modifiers(native_event),
                    phase,
                })
            }),
            NSEventType::NSScrollWheel => window_height.map(|window_height| {
                let phase = match native_event.phase() {
                    NSEventPhase::NSEventPhaseMayBegin | NSEventPhase::NSEventPhaseBegan => {
                        TouchPhase::Started
                    }
                    NSEventPhase::NSEventPhaseEnded => TouchPhase::Ended,
                    _ => TouchPhase::Moved,
                };

                let raw_data = point(
                    native_event.scrollingDeltaX() as f32,
                    native_event.scrollingDeltaY() as f32,
                );

                let delta = if native_event.hasPreciseScrollingDeltas() == YES {
                    ScrollDelta::Pixels(raw_data.map(px))
                } else {
                    ScrollDelta::Lines(raw_data)
                };

                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: point(
                        px(native_event.locationInWindow().x as f32),
                        window_height - px(native_event.locationInWindow().y as f32),
                    ),
                    delta,
                    touch_phase: phase,
                    modifiers: read_modifiers(native_event),
                })
            }),
            NSEventType::NSLeftMouseDragged
            | NSEventType::NSRightMouseDragged
            | NSEventType::NSOtherMouseDragged => {
                let pressed_button = match native_event.buttonNumber() {
                    0 => MouseButton::Left,
                    1 => MouseButton::Right,
                    2 => MouseButton::Middle,
                    3 => MouseButton::Navigate(NavigationDirection::Back),
                    4 => MouseButton::Navigate(NavigationDirection::Forward),
                    // Other mouse buttons aren't tracked currently
                    _ => return None,
                };

                window_height.map(|window_height| {
                    PlatformInput::MouseMove(MouseMoveEvent {
                        pressed_button: Some(pressed_button),
                        position: point(
                            px(native_event.locationInWindow().x as f32),
                            window_height - px(native_event.locationInWindow().y as f32),
                        ),
                        modifiers: read_modifiers(native_event),
                    })
                })
            }
            NSEventType::NSMouseMoved => window_height.map(|window_height| {
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: point(
                        px(native_event.locationInWindow().x as f32),
                        window_height - px(native_event.locationInWindow().y as f32),
                    ),
                    pressed_button: None,
                    modifiers: read_modifiers(native_event),
                })
            }),
            NSEventType::NSMouseExited => window_height.map(|window_height| {
                PlatformInput::MouseExited(MouseExitEvent {
                    position: point(
                        px(native_event.locationInWindow().x as f32),
                        window_height - px(native_event.locationInWindow().y as f32),
                    ),

                    pressed_button: None,
                    modifiers: read_modifiers(native_event),
                })
            }),
            _ => None,
        }
    }
}

unsafe fn parse_keystroke(native_event: id) -> Keystroke {
    unsafe {
        use cocoa::appkit::*;

        let characters = native_event
            .charactersIgnoringModifiers()
            .to_str()
            .to_string();
        let mut key_char = None;
        let first_char = characters.chars().next().map(|ch| ch as u16);
        let modifiers = native_event.modifierFlags();

        let control = modifiers.contains(NSEventModifierFlags::NSControlKeyMask);
        let alt = modifiers.contains(NSEventModifierFlags::NSAlternateKeyMask);
        let mut shift = modifiers.contains(NSEventModifierFlags::NSShiftKeyMask);
        let command = modifiers.contains(NSEventModifierFlags::NSCommandKeyMask);
        let function = modifiers.contains(NSEventModifierFlags::NSFunctionKeyMask)
            && first_char
                .is_none_or(|ch| !(NSUpArrowFunctionKey..=NSModeSwitchFunctionKey).contains(&ch));

        #[allow(non_upper_case_globals)]
        let key = match first_char {
            Some(SPACE_KEY) => {
                key_char = Some(" ".to_string());
                "space".to_string()
            }
            Some(TAB_KEY) => {
                key_char = Some("\t".to_string());
                "tab".to_string()
            }
            Some(ENTER_KEY) | Some(NUMPAD_ENTER_KEY) => {
                key_char = Some("\n".to_string());
                "enter".to_string()
            }
            Some(BACKSPACE_KEY) => "backspace".to_string(),
            Some(ESCAPE_KEY) => "escape".to_string(),
            Some(SHIFT_TAB_KEY) => "tab".to_string(),
            Some(NSUpArrowFunctionKey) => "up".to_string(),
            Some(NSDownArrowFunctionKey) => "down".to_string(),
            Some(NSLeftArrowFunctionKey) => "left".to_string(),
            Some(NSRightArrowFunctionKey) => "right".to_string(),
            Some(NSPageUpFunctionKey) => "pageup".to_string(),
            Some(NSPageDownFunctionKey) => "pagedown".to_string(),
            Some(NSHomeFunctionKey) => "home".to_string(),
            Some(NSEndFunctionKey) => "end".to_string(),
            Some(NSDeleteFunctionKey) => "delete".to_string(),
            // Observed Insert==NSHelpFunctionKey not NSInsertFunctionKey.
            Some(NSHelpFunctionKey) => "insert".to_string(),
            Some(NSF1FunctionKey) => "f1".to_string(),
            Some(NSF2FunctionKey) => "f2".to_string(),
            Some(NSF3FunctionKey) => "f3".to_string(),
            Some(NSF4FunctionKey) => "f4".to_string(),
            Some(NSF5FunctionKey) => "f5".to_string(),
            Some(NSF6FunctionKey) => "f6".to_string(),
            Some(NSF7FunctionKey) => "f7".to_string(),
            Some(NSF8FunctionKey) => "f8".to_string(),
            Some(NSF9FunctionKey) => "f9".to_string(),
            Some(NSF10FunctionKey) => "f10".to_string(),
            Some(NSF11FunctionKey) => "f11".to_string(),
            Some(NSF12FunctionKey) => "f12".to_string(),
            Some(NSF13FunctionKey) => "f13".to_string(),
            Some(NSF14FunctionKey) => "f14".to_string(),
            Some(NSF15FunctionKey) => "f15".to_string(),
            Some(NSF16FunctionKey) => "f16".to_string(),
            Some(NSF17FunctionKey) => "f17".to_string(),
            Some(NSF18FunctionKey) => "f18".to_string(),
            Some(NSF19FunctionKey) => "f19".to_string(),
            Some(NSF20FunctionKey) => "f20".to_string(),
            Some(NSF21FunctionKey) => "f21".to_string(),
            Some(NSF22FunctionKey) => "f22".to_string(),
            Some(NSF23FunctionKey) => "f23".to_string(),
            Some(NSF24FunctionKey) => "f24".to_string(),
            Some(NSF25FunctionKey) => "f25".to_string(),
            Some(NSF26FunctionKey) => "f26".to_string(),
            Some(NSF27FunctionKey) => "f27".to_string(),
            Some(NSF28FunctionKey) => "f28".to_string(),
            Some(NSF29FunctionKey) => "f29".to_string(),
            Some(NSF30FunctionKey) => "f30".to_string(),
            Some(NSF31FunctionKey) => "f31".to_string(),
            Some(NSF32FunctionKey) => "f32".to_string(),
            Some(NSF33FunctionKey) => "f33".to_string(),
            Some(NSF34FunctionKey) => "f34".to_string(),
            Some(NSF35FunctionKey) => "f35".to_string(),
            _ => {
                // Cases to test when modifying this:
                //
                //           qwerty key | none | cmd   | cmd-shift
                // * Armenian         s | ս    | cmd-s | cmd-shift-s  (layout is non-ASCII, so we use cmd layout)
                // * Dvorak+QWERTY    s | o    | cmd-s | cmd-shift-s  (layout switches on cmd)
                // * Ukrainian+QWERTY s | с    | cmd-s | cmd-shift-s  (macOS reports cmd-s instead of cmd-S)
                // * Czech            7 | ý    | cmd-ý | cmd-7        (layout has shifted numbers)
                // * Norwegian        7 | 7    | cmd-7 | cmd-/        (macOS reports cmd-shift-7 instead of cmd-/)
                // * Russian          7 | 7    | cmd-7 | cmd-&        (shift-7 is . but when cmd is down, should use cmd layout)
                // * German QWERTZ    ; | ö    | cmd-ö | cmd-Ö        (Zed's shift special case only applies to a-z)
                //
                let mut chars_ignoring_modifiers =
                    chars_for_modified_key(native_event.keyCode(), NO_MOD);
                let mut chars_with_shift =
                    chars_for_modified_key(native_event.keyCode(), SHIFT_MOD);
                let always_use_cmd_layout = always_use_command_layout();

                // Handle Dvorak+QWERTY / Russian / Armenian
                if command || always_use_cmd_layout {
                    let chars_with_cmd = chars_for_modified_key(native_event.keyCode(), CMD_MOD);
                    let chars_with_both =
                        chars_for_modified_key(native_event.keyCode(), CMD_MOD | SHIFT_MOD);

                    // We don't do this in the case that the shifted command key generates
                    // the same character as the unshifted command key (Norwegian, e.g.)
                    if chars_with_both != chars_with_cmd {
                        chars_with_shift = chars_with_both;

                    // Handle edge-case where cmd-shift-s reports cmd-s instead of
                    // cmd-shift-s (Ukrainian, etc.)
                    } else if chars_with_cmd.to_ascii_uppercase() != chars_with_cmd {
                        chars_with_shift = chars_with_cmd.to_ascii_uppercase();
                    }
                    chars_ignoring_modifiers = chars_with_cmd;
                }

                if !control && !command && !function {
                    let mut mods = NO_MOD;
                    if shift {
                        mods |= SHIFT_MOD;
                    }
                    if alt {
                        mods |= OPTION_MOD;
                    }

                    key_char = Some(chars_for_modified_key(native_event.keyCode(), mods));
                }

                if shift
                    && chars_ignoring_modifiers
                        .chars()
                        .all(|c| c.is_ascii_lowercase())
                {
                    chars_ignoring_modifiers
                } else if shift {
                    shift = false;
                    chars_with_shift
                } else {
                    chars_ignoring_modifiers
                }
            }
        };

        Keystroke {
            modifiers: Modifiers {
                control,
                alt,
                shift,
                platform: command,
                function,
                ..Default::default()
            },
            key,
            key_char,
            is_numpad: is_numpad_key_code(native_event.keyCode()),
            physical_key: macos_key_code_to_usb_hid_usage(native_event.keyCode()),
        }
    }
}

/// Maps a macOS ADB/USB virtual key code (`NSEvent.keyCode`) to its USB HID
/// Usage ID (Usage Page 0x07, Keyboard/Keypad) — the layout-independent
/// physical key identity. This mapping is fixed by Apple's ADB->USB HID
/// translation table and is the same across every Mac keyboard layout.
/// Covers the keys used by `som-key`'s drawn layout; returns `None` for
/// anything not in that table rather than guessing.
fn macos_key_code_to_usb_hid_usage(key_code: CGKeyCode) -> Option<u32> {
    Some(match key_code {
        0 => 0x04,  // A
        11 => 0x05, // B
        8 => 0x06,  // C
        2 => 0x07,  // D
        14 => 0x08, // E
        3 => 0x09,  // F
        5 => 0x0A,  // G
        4 => 0x0B,  // H
        34 => 0x0C, // I
        38 => 0x0D, // J
        40 => 0x0E, // K
        37 => 0x0F, // L
        46 => 0x10, // M
        45 => 0x11, // N
        31 => 0x12, // O
        35 => 0x13, // P
        12 => 0x14, // Q
        15 => 0x15, // R
        1 => 0x16,  // S
        17 => 0x17, // T
        32 => 0x18, // U
        9 => 0x19,  // V
        13 => 0x1A, // W
        7 => 0x1B,  // X
        16 => 0x1C, // Y
        6 => 0x1D,  // Z
        18 => 0x1E, // 1
        19 => 0x1F, // 2
        20 => 0x20, // 3
        21 => 0x21, // 4
        23 => 0x22, // 5
        22 => 0x23, // 6
        26 => 0x24, // 7
        28 => 0x25, // 8
        25 => 0x26, // 9
        29 => 0x27, // 0
        36 => 0x28, // Return
        53 => 0x29, // Escape
        51 => 0x2A, // Backspace/Delete
        48 => 0x2B, // Tab
        49 => 0x2C, // Space
        27 => 0x2D, // -
        24 => 0x2E, // =
        33 => 0x2F, // [
        30 => 0x30, // ]
        42 => 0x31, // \
        41 => 0x33, // ;
        39 => 0x34, // '
        50 => 0x35, // ` (grave)
        43 => 0x36, // ,
        47 => 0x37, // .
        44 => 0x38, // /
        57 => 0x39, // Caps Lock
        122 => 0x3A, // F1
        120 => 0x3B, // F2
        99 => 0x3C,  // F3
        118 => 0x3D, // F4
        96 => 0x3E,  // F5
        97 => 0x3F,  // F6
        98 => 0x40,  // F7
        100 => 0x41, // F8
        101 => 0x42, // F9
        109 => 0x43, // F10
        103 => 0x44, // F11
        111 => 0x45, // F12
        105 => 0x46, // F13 / Print Screen
        107 => 0x47, // F14 / Scroll Lock
        113 => 0x48, // F15 / Pause
        114 => 0x49, // Insert / Help
        115 => 0x4A, // Home
        116 => 0x4B, // Page Up
        117 => 0x4C, // Forward Delete
        119 => 0x4D, // End
        121 => 0x4E, // Page Down
        124 => 0x4F, // Right Arrow
        123 => 0x50, // Left Arrow
        125 => 0x51, // Down Arrow
        126 => 0x52, // Up Arrow
        71 => 0x53,  // Num Lock / Clear
        75 => 0x54,  // Keypad /
        67 => 0x55,  // Keypad *
        78 => 0x56,  // Keypad -
        69 => 0x57,  // Keypad +
        76 => 0x58,  // Keypad Enter
        83 => 0x59,  // Keypad 1
        84 => 0x5A,  // Keypad 2
        85 => 0x5B,  // Keypad 3
        86 => 0x5C,  // Keypad 4
        87 => 0x5D,  // Keypad 5
        88 => 0x5E,  // Keypad 6
        89 => 0x5F,  // Keypad 7
        91 => 0x60,  // Keypad 8
        92 => 0x61,  // Keypad 9
        82 => 0x62,  // Keypad 0
        65 => 0x63,  // Keypad .
        59 => 0xE0,  // Left Control
        58 => 0xE2,  // Left Option/Alt
        55 => 0xE3,  // Left Command
        56 => 0xE1,  // Left Shift
        60 => 0xE5,  // Right Shift
        61 => 0xE6,  // Right Option/Alt
        62 => 0xE4,  // Right Control
        54 => 0xE7,  // Right Command
        63 => 0x03,  // fn (not a standard HID usage; Apple-specific, reusing 0x03 which is Reserved)
        _ => return None,
    })
}

/// The inverse of `macos_key_code_to_usb_hid_usage`, restricted to the keys
/// with a layout-dependent printed character (letters, numbers, symbols) —
/// used to translate a physical key back to an ADB key code so its current
/// character can be looked up via `UCKeyTranslate` without an actual
/// keypress. Modifier/nav/function keys aren't included since they have no
/// such character.
pub(crate) fn usb_hid_usage_to_macos_key_code(usb_hid: u32) -> Option<CGKeyCode> {
    Some(match usb_hid {
        0x04 => 0,  // A
        0x05 => 11, // B
        0x06 => 8,  // C
        0x07 => 2,  // D
        0x08 => 14, // E
        0x09 => 3,  // F
        0x0A => 5,  // G
        0x0B => 4,  // H
        0x0C => 34, // I
        0x0D => 38, // J
        0x0E => 40, // K
        0x0F => 37, // L
        0x10 => 46, // M
        0x11 => 45, // N
        0x12 => 31, // O
        0x13 => 35, // P
        0x14 => 12, // Q
        0x15 => 15, // R
        0x16 => 1,  // S
        0x17 => 17, // T
        0x18 => 32, // U
        0x19 => 9,  // V
        0x1A => 13, // W
        0x1B => 7,  // X
        0x1C => 16, // Y
        0x1D => 6,  // Z
        0x1E => 18, // 1
        0x1F => 19, // 2
        0x20 => 20, // 3
        0x21 => 21, // 4
        0x22 => 23, // 5
        0x23 => 22, // 6
        0x24 => 26, // 7
        0x25 => 28, // 8
        0x26 => 25, // 9
        0x27 => 29, // 0
        0x2D => 27, // -
        0x2E => 24, // =
        0x2F => 33, // [
        0x30 => 30, // ]
        0x31 => 42, // \
        0x33 => 41, // ;
        0x34 => 39, // '
        0x35 => 50, // ` (grave)
        0x36 => 43, // ,
        0x37 => 47, // .
        0x38 => 44, // /
        _ => return None,
    })
}

/// Whether a `NSEvent.keyCode` belongs to the numeric keypad. These virtual
/// key codes are fixed by the ANSI/ISO/JIS USB HID keyboard usage tables
/// and don't vary by keyboard layout. A numpad "1" and a top-row "1"
/// produce the same `characters`/`charactersIgnoringModifiers`, so `key`
/// alone can't tell them apart — this is the only signal that can.
fn is_numpad_key_code(key_code: CGKeyCode) -> bool {
    matches!(
        key_code,
        65 | 67 | 69 | 71 | 75 | 76 | 78 | 81 | 82 | 83 | 84 | 85 | 86 | 87 | 88 | 89 | 91 | 92
    )
}

fn always_use_command_layout() -> bool {
    if chars_for_modified_key(0, NO_MOD).is_ascii() {
        return false;
    }

    chars_for_modified_key(0, CMD_MOD).is_ascii()
}

pub(crate) const NO_MOD: u32 = 0;
const CMD_MOD: u32 = 1;
pub(crate) const SHIFT_MOD: u32 = 2;
const OPTION_MOD: u32 = 8;

pub(crate) fn chars_for_modified_key(code: CGKeyCode, modifiers: u32) -> String {
    // Values from: https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.6.sdk/System/Library/Frameworks/Carbon.framework/Versions/A/Frameworks/HIToolbox.framework/Versions/A/Headers/Events.h#L126
    // shifted >> 8 for UCKeyTranslate
    const CG_SPACE_KEY: u16 = 49;
    // https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.6.sdk/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/CarbonCore.framework/Versions/A/Headers/UnicodeUtilities.h#L278
    #[allow(non_upper_case_globals)]
    const kUCKeyActionDown: u16 = 0;
    #[allow(non_upper_case_globals)]
    const kUCKeyTranslateNoDeadKeysMask: u32 = 0;

    let keyboard_type = unsafe { LMGetKbdType() as u32 };
    const BUFFER_SIZE: usize = 4;
    let mut dead_key_state = 0;
    let mut buffer: [u16; BUFFER_SIZE] = [0; BUFFER_SIZE];
    let mut buffer_size: usize = 0;

    let keyboard = unsafe { TISCopyCurrentKeyboardLayoutInputSource() };
    if keyboard.is_null() {
        return "".to_string();
    }
    let layout_data = unsafe {
        TISGetInputSourceProperty(keyboard, kTISPropertyUnicodeKeyLayoutData as *const c_void)
            as CFDataRef
    };
    if layout_data.is_null() {
        unsafe {
            let _: () = msg_send![keyboard, release];
        }
        return "".to_string();
    }
    let keyboard_layout = unsafe { CFDataGetBytePtr(layout_data) };

    unsafe {
        UCKeyTranslate(
            keyboard_layout as *const c_void,
            code,
            kUCKeyActionDown,
            modifiers,
            keyboard_type,
            kUCKeyTranslateNoDeadKeysMask,
            &mut dead_key_state,
            BUFFER_SIZE,
            &mut buffer_size as *mut usize,
            &mut buffer as *mut u16,
        );
        if dead_key_state != 0 {
            UCKeyTranslate(
                keyboard_layout as *const c_void,
                CG_SPACE_KEY,
                kUCKeyActionDown,
                modifiers,
                keyboard_type,
                kUCKeyTranslateNoDeadKeysMask,
                &mut dead_key_state,
                BUFFER_SIZE,
                &mut buffer_size as *mut usize,
                &mut buffer as *mut u16,
            );
        }
        let _: () = msg_send![keyboard, release];
    }
    String::from_utf16(&buffer[..buffer_size]).unwrap_or_default()
}
