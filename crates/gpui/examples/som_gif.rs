#![cfg_attr(target_family = "wasm", no_main)]

//! Minimal standalone animated-GIF viewer — bypasses the entire Kitty
//! graphics protocol / PTY / ConPTY / som-srv stack this project's
//! terminal-based Kitty support goes through, to answer one narrow
//! question in isolation: does GPUI's own `img()` element actually
//! animate a multi-frame GIF on this machine at all. Usage:
//! `cargo run --example som_gif -- <path-to.gif>`.

use gpui::{App, Context, Render, Window, WindowOptions, div, img, prelude::*};
use gpui_platform::application;
use std::path::PathBuf;

struct GifViewer {
    gif_path: PathBuf,
}

impl GifViewer {
    fn new(gif_path: PathBuf) -> Self {
        Self { gif_path }
    }
}

impl Render for GifViewer {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            img(self.gif_path.clone())
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .id("gif"),
        )
    }
}

fn run_example() {
    let gif_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/image/black-cat-typing.gif")
        });

    application().run(|cx: &mut App| {
        cx.open_window(
            WindowOptions {
                focus: true,
                ..Default::default()
            },
            |_, cx| cx.new(|_| GifViewer::new(gif_path)),
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    env_logger::init();
    run_example();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run_example();
}
