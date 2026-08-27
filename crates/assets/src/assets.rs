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
// Pre-built som-srv binaries for every remote platform Som's `tmux: true`
// profiles support (Windows amd64, macOS arm64, Linux amd64 — NOT
// linux-arm, which stays permanently unsupported) — see `som_srv::
// protocol::ensure_embedded_binary_extracted`, which writes these bytes out
// to `~/.config/som/srv/{platform}/` on demand. Kept up to date by
// `scripts/update-srv-binaries.sh` (dev-only, manually run before a
// release), not built here or in CI.
#[include = "srv/windows-amd/som-srv.exe"]
#[include = "srv/macos-arm/som-srv"]
#[include = "srv/linux-amd/som-srv"]
// The Windows Terminal project's improved ConPTY backend — see `crates/zed/
// build.rs`'s doc comment for where these are downloaded from (a pinned
// nupkg version) and `crates/zed/src/main.rs`'s startup extraction, which
// writes these bytes out to `~/.config/som/conpty/` (never next to som.exe
// itself, so Som stays runnable from an arbitrary/read-only directory) and
// points `SetDllDirectoryW` there before any terminal is created. Refreshed
// manually by copying `target/release/{conpty.dll,OpenConsole.exe}` here
// whenever build.rs's pinned `conpty_url` version bumps — not automated.
#[include = "conpty/conpty.dll"]
#[include = "conpty/OpenConsole.exe"]
// Decode-only FFmpeg shared libs, trimmed to just the demuxers/decoders
// Som's video player actually uses (mov/matroska/avi containers,
// h264/hevc/vp9/av1/mpeg4 codecs — see `vcpkg-overlays/ffmpeg/
// portfile.cmake`'s own `--disable-everything`/`--enable-decoder=...`
// selection for the exact list and why) rather than FFmpeg's full
// default catalog of several hundred codecs/formats/filters/encoders.
// Built natively per-platform (never cross-compiled — see
// `project_som_tmux`'s same rule for som-srv binaries). Windows built
// via vcpkg's MSVC port (`cl.exe`, not MinGW) so no GNU toolchain is
// required on the dev machine.
//
// Stored zstd-compressed (`.dll.zst`, level 19 — see
// `crates/assets/examples/compress_ffmpeg_dlls.rs`) rather than raw:
// even trimmed to five decoders, `avcodec` alone still dwarfs every
// other embedded asset in this crate, and DLL machine code compresses
// well under zstd — worth the one-time decompression cost at startup
// (see `decompress_zst` below) to keep that weight out of `som.exe`
// itself. Lazily extracted-and-decompressed to `~/.config/som/ffmpeg/`
// by `ensure_ffmpeg_extracted_and_wired` in `crates/zed/src/main.rs`,
// same lazy-extraction pattern `conpty/` above uses, just with a
// decompression step in between. macOS/Linux builds not yet added.
//
// Measured impact of BOTH the decoder trim (previous paragraph) and
// this compression, together, on a real build (2026-08-27): the five
// DLLs went from ~18.3MB raw (full-catalog FFmpeg) to ~2.0MB embedded
// (trimmed + zstd) — `som.exe` itself shrank from 53,480,448 to
// 37,466,624 bytes (~15.3MB smaller, ~29% of the whole binary).
#[include = "ffmpeg/windows-amd/avcodec-63.dll.zst"]
#[include = "ffmpeg/windows-amd/avformat-63.dll.zst"]
#[include = "ffmpeg/windows-amd/avutil-61.dll.zst"]
#[include = "ffmpeg/windows-amd/swresample-7.dll.zst"]
#[include = "ffmpeg/windows-amd/swscale-10.dll.zst"]
pub struct Assets;

/// Decompresses one zstd-compressed embedded asset (see the `ffmpeg/`
/// `#[include]` block above for why FFmpeg's shared libs specifically
/// are stored this way) — a thin wrapper so call sites don't need their
/// own `zstd` dependency just to unpack what `Assets` already embeds.
pub fn decompress_zst(compressed: &[u8]) -> anyhow::Result<Vec<u8>> {
    zstd::stream::decode_all(compressed).context("failed to zstd-decompress embedded asset")
}

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
