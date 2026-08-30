#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

fn main() {
    if cfg!(windows) && cfg!(target_env = "msvc") {
        // Mirrors `crates/zed/build.rs`'s own `/DELAYLOAD` setup for the
        // same reason: `somcat` also calls FFmpeg-backed code
        // (`video_metadata`) directly, so without delay-loading, the
        // Windows PE loader tries to resolve avcodec/avformat/avutil/
        // swresample/swscale at process-start time — before `main` gets a
        // chance to run `terminal::rich_content_video_player::
        // ensure_ffmpeg_extracted_and_wired`'s extraction/search-path
        // setup — confirmed live as `STATUS_DLL_NOT_FOUND` with zero
        // stderr output on a machine with no system FFmpeg install.
        for dll in
            ["avcodec-63.dll", "avformat-63.dll", "avutil-61.dll", "swresample-7.dll", "swscale-10.dll"]
        {
            println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg=delayimp.lib");
    }
}
