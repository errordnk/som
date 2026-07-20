// This crate was essentially pulled out verbatim from main `zed` crate to avoid having to run RustEmbed macro whenever zed has to be rebuilt. It saves a second or two on an incremental build.

use anyhow::Context as _;
use gpui::{App, AssetSource, Result, SharedString};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../assets"]
#[include = "windows.json"]
#[include = "macos.json"]
#[include = "linux.json"]
#[include = "nord.json"]
#[include = "fonts/FiraCodeNerdFont-Regular.ttf"]
#[include = "icons/*.svg"]
// Pre-built som-tmux binaries for every remote platform Som's `tmux: true`
// profiles support (Windows amd64, macOS arm64, Linux amd64 — NOT
// linux-arm, which stays permanently unsupported) — see `som_tmux::
// protocol::ensure_embedded_binary_extracted`, which writes these bytes out
// to `~/.config/som/tmux/{platform}/` on demand. Kept up to date by
// `scripts/update-tmux-binaries.sh` (dev-only, manually run before a
// release), not built here or in CI.
#[include = "tmux/windows-amd/som-tmux.exe"]
#[include = "tmux/macos-arm/som-tmux"]
#[include = "tmux/linux-amd/som-tmux"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Self::get(path)
            .map(|f| Some(f.data))
            .with_context(|| format!("loading asset at path {path:?}"))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter_map(|p| {
                if p.starts_with(path) {
                    Some(p.into())
                } else {
                    None
                }
            })
            .collect())
    }
}

impl Assets {
    pub fn load_fonts(&self, cx: &App) -> anyhow::Result<()> {
        let font_paths = self.list("")?;
        let mut embedded_fonts = Vec::new();
        for font_path in font_paths {
            if font_path.ends_with(".ttf") {
                let font_bytes = cx
                    .asset_source()
                    .load(&font_path)?
                    .expect("Assets should never return None");
                embedded_fonts.push(font_bytes);
            }
        }

        cx.text_system().add_fonts(embedded_fonts)
    }

    pub fn load_test_fonts(&self, cx: &App) {
        cx.text_system()
            .add_fonts(vec![
                self.load("fonts/FiraCodeNerdFont-Regular.ttf").unwrap().unwrap(),
            ])
            .unwrap()
    }
}
