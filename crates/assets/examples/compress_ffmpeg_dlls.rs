//! Dev-only one-shot: compresses `assets/ffmpeg/windows-amd/*.dll` into
//! `*.dll.zst` (level 19) using the SAME `zstd` crate/version this
//! workspace embeds and decompresses with (`assets::decompress_zst`) —
//! deliberately not the external `zstd`/`7z` CLI, to avoid any risk of a
//! format/version mismatch between what compresses these files here and
//! what decompresses them at Som's own startup. Run manually whenever
//! the DLLs under `assets/ffmpeg/` are refreshed (a new vcpkg build, a
//! new decoder added to `vcpkg-overlays/ffmpeg`'s trim list) — not run
//! in CI, same manual-refresh posture as `scripts/update-tmux-
//! binaries.sh`.
//!
//! Run from the repo root: `cargo run -p assets --example
//! compress_ffmpeg_dlls`

fn main() -> anyhow::Result<()> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/ffmpeg/windows-amd");
    let mut compressed_any = false;

    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dll") {
            continue;
        }

        let raw = std::fs::read(&path)?;
        let compressed = zstd::stream::encode_all(raw.as_slice(), 19)?;

        let target = path.with_extension("dll.zst");
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
        anyhow::bail!("no .dll files found under {dir:?}");
    }

    println!();
    println!("Done. Once assets.rs's #[include] list points at the .dll.zst");
    println!("versions and a build/live-test has been verified, remove the raw");
    println!("originals: rm {}/*.dll", dir.display());

    Ok(())
}
