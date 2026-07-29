use collections::HashMap;

use crate::{KeybindingKeystroke, Keystroke};

/// A trait for platform-specific keyboard layouts
pub trait PlatformKeyboardLayout {
    /// Get the keyboard layout ID, which should be unique to the layout
    fn id(&self) -> &str;
    /// Get the keyboard layout display name
    fn name(&self) -> &str;
}

/// A trait for platform-specific keyboard mappings
pub trait PlatformKeyboardMapper {
    /// Map a key equivalent to its platform-specific representation
    fn map_key_equivalent(
        &self,
        keystroke: Keystroke,
        use_key_equivalents: bool,
    ) -> KeybindingKeystroke;
    /// Get the key equivalents for the current keyboard layout,
    /// only used on macOS
    fn get_key_equivalents(&self) -> Option<&HashMap<char, char>>;
    /// Translate a physical key (identified by its layout-independent USB
    /// HID Usage ID, as reported in `Keystroke.physical_key`) to the
    /// character it currently produces under the active keyboard layout,
    /// without requiring an actual keypress — e.g. for drawing an on-screen
    /// keyboard whose labels must update the instant the layout changes.
    /// Returns `None` for keys with no printed character (arrows, function
    /// keys, modifiers, ...) or if this platform doesn't support the query.
    fn key_for_physical(&self, _usb_hid_usage: u32, _shift: bool) -> Option<String> {
        None
    }
}

/// A dummy implementation of the platform keyboard mapper
pub struct DummyKeyboardMapper;

impl PlatformKeyboardMapper for DummyKeyboardMapper {
    fn map_key_equivalent(
        &self,
        keystroke: Keystroke,
        _use_key_equivalents: bool,
    ) -> KeybindingKeystroke {
        KeybindingKeystroke::from_keystroke(keystroke)
    }

    fn get_key_equivalents(&self) -> Option<&HashMap<char, char>> {
        None
    }
}
