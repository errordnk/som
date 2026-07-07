use gpui::{Hsla, Rgba, WindowControlArea, prelude::*};
#[cfg(windows)]
use std::sync::OnceLock;
use ui::prelude::*;

// This whole module is compiled on every platform (see
// `platform_title_bar.rs`'s `render_right_window_controls` — the Windows
// branch is picked at RUNTIME via `PlatformStyle::platform()`, not gated by
// `#[cfg(windows)]`, so `platform_windows` has to exist and compile
// everywhere even though it only ever RUNS on Windows). Only this one
// function actually touches the `windows` crate, so only it needs the
// `cfg` split — everything else here is plain GPUI rendering code that's
// already platform-agnostic.
#[cfg(windows)]
fn is_windows_11() -> bool {
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(|| {
        use windows::Win32::System::SystemInformation::GetVersionExW;
        use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        unsafe { let _ = GetVersionExW(&mut info); }
        // Windows 11 is build 22000+
        info.dwBuildNumber >= 22000
    })
}

// Never actually called on a non-Windows build (this whole module only
// RENDERS when `PlatformStyle::platform() == Windows`, which is a runtime
// fact, not something `cfg(not(windows))` can know) — this is purely to
// satisfy the compiler on the other 3 platforms.
#[cfg(not(windows))]
fn is_windows_11() -> bool {
    false
}

#[derive(IntoElement)]
pub struct WindowsWindowControls {
    button_height: Pixels,
}

impl WindowsWindowControls {
    pub fn new(button_height: Pixels) -> Self {
        Self { button_height }
    }
}

impl RenderOnce for WindowsWindowControls {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .id("windows-window-controls")
            .flex()
            .flex_row()
            .justify_center()
            .content_stretch()
            .max_h(self.button_height)
            .min_h(self.button_height)
            .child(WindowsCaptionButton::Minimize)
            .map(|this| {
                this.child(if window.is_maximized() {
                    WindowsCaptionButton::Restore
                } else {
                    WindowsCaptionButton::Maximize
                })
            })
            .child(WindowsCaptionButton::Close)
    }
}

#[derive(IntoElement)]
enum WindowsCaptionButton {
    Minimize,
    Restore,
    Maximize,
    Close,
}

impl WindowsCaptionButton {
    #[inline]
    fn id(&self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Restore => "restore",
            Self::Maximize => "maximize",
            Self::Close => "close",
        }
    }

    #[inline]
    fn icon(&self) -> &'static str {
        if is_windows_11() {
            match self {
                Self::Minimize => "\u{e921}",
                Self::Restore => "\u{e923}",
                Self::Maximize => "\u{e922}",
                Self::Close => "\u{e8bb}",
            }
        } else {
            match self {
                Self::Minimize => "─",
                Self::Restore => "◱",
                Self::Maximize => "□",
                Self::Close => "✕",
            }
        }
    }

    #[inline]
    fn control_area(&self) -> WindowControlArea {
        match self {
            Self::Close => WindowControlArea::Close,
            Self::Maximize | Self::Restore => WindowControlArea::Max,
            Self::Minimize => WindowControlArea::Min,
        }
    }
}

impl RenderOnce for WindowsCaptionButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let (hover_bg, hover_fg, active_bg, active_fg) = match self {
            Self::Close => {
                let color: Hsla = Rgba {
                    r: 232.0 / 255.0,
                    g: 17.0 / 255.0,
                    b: 32.0 / 255.0,
                    a: 1.0,
                }
                .into();

                (
                    color,
                    gpui::white(),
                    color.opacity(0.8),
                    gpui::white().opacity(0.8),
                )
            }
            _ => (
                cx.theme().colors().ghost_element_hover,
                cx.theme().colors().text,
                cx.theme().colors().ghost_element_active,
                cx.theme().colors().text,
            ),
        };

        h_flex()
            .id(self.id())
            .justify_center()
            .content_center()
            .occlude()
            .w(px(36.))
            .h_full()
            .text_size(px(10.0))
            .when(is_windows_11(), |this| {
                this.font(gpui::Font {
                    family: "Segoe Fluent Icons".into(),
                    ..Default::default()
                })
            })
            .hover(|style| style.bg(hover_bg).text_color(hover_fg))
            .active(|style| style.bg(active_bg).text_color(active_fg))
            .window_control_area(self.control_area())
            .child(self.icon())
    }
}
