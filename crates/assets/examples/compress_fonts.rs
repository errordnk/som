//! Dev-only one-shot: compresses `assets/fonts/*.ttf` into `*.ttf.zst`
//! (level 19) using the SAME `zstd` crate/version this workspace embeds
//! and decompresses with (`assets::decompress_zst`) — same reasoning as
//! `compress_ffmpeg_dlls.rs`. Run manually whenever a font under
//! `assets/fonts/` is added or refreshed.
//!
//! Run from the repo root: `cargo run -p assets --example compress_fonts`

fn main() -> anyhow::Result<()> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
    let mut compressed_any = false;

    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ttf") {
            continue;
        }

        let raw = std::fs::read(&path)?;
        let compressed = zstd::stream::encode_all(raw.as_slice(), 19)?;

        let target = path.with_extension("ttf.zst");
        std::fs::write(&target, &compressed)?;

        let ratio = 100.0 - (compressed.len() as f64 / raw.len() as f64 * 100.0);
        println!(
            "{}: {} -> {} bytes ({ratio:.1}% smaller)",
            path.file_name().unwrap().to_string_lossy(),
            raw.len(),
            compressed.len()
        );
        compressed_any = true;
    }

    if !compressed_any {
        anyhow::bail!("no .ttf files found under {dir:?}");
    }

    Ok(())
}
