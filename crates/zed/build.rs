#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
use std::process::Command;

fn main() {
    #[cfg(target_os = "linux")]
    {
        // Add rpaths for libraries that webrtc-sys dlopens at runtime.
        // This is mostly required for hosts with non-standard SO installation
        // locations such as NixOS.
        let dlopened_libs = ["libva", "libva-drm", "egl"];

        let mut rpath_dirs = std::collections::BTreeSet::new();
        for lib in &dlopened_libs {
            if let Some(libdir) = pkg_config::get_variable(lib, "libdir").ok() {
                rpath_dirs.insert(libdir);
            } else {
                eprintln!("zed build.rs: {lib} not found in pkg-config's path");
            }
        }

        for dir in &rpath_dirs {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        }
    }

    if cfg!(target_os = "macos") {
        println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET=10.15.7");

        // Weakly link ReplayKit to ensure Zed can be used on macOS 10.15+.
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,ReplayKit");

        // Seems to be required to enable Swift concurrency
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

        // Register exported Objective-C selectors, protocols, etc
        println!("cargo:rustc-link-arg=-Wl,-ObjC");

        // weak link to support Catalina
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
    }

    // Populate git sha environment variable if git is available
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    println!(
        "cargo:rustc-env=TARGET={}",
        std::env::var("TARGET").unwrap()
    );

    let git_sha = match std::env::var("ZED_COMMIT_SHA").ok() {
        Some(git_sha) => {
            // In deterministic build environments such as Nix, we inject the commit sha into the build script.
            Some(git_sha)
        }
        None => {
            if let Some(output) = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                && output.status.success()
            {
                let git_sha = String::from_utf8_lossy(&output.stdout);
                Some(git_sha.trim().to_string())
            } else {
                None
            }
        }
    };

    if let Some(git_sha) = git_sha {
        println!("cargo:rustc-env=ZED_COMMIT_SHA={git_sha}");

        if let Some(build_identifier) = option_env!("GITHUB_RUN_NUMBER") {
            println!("cargo:rustc-env=ZED_BUILD_ID={build_identifier}");
        }

    }

    if cfg!(windows) {
        if cfg!(target_env = "msvc") {
            // todo(windows): This is to avoid stack overflow. Remove it when solved.
            println!("cargo:rustc-link-arg=/stack:{}", 8 * 1024 * 1024);

            // FFmpeg's DLLs (avcodec/avformat/avutil/swresample/swscale)
            // are statically IMPORTED by som.exe — `ffmpeg-sys-next`
            // links against their `.lib` import libraries — which means
            // the Windows PE loader normally resolves them before `main`
            // ever runs, well before `ensure_ffmpeg_extracted_and_wired`
            // (`crates/zed/src/main.rs`) gets a chance to extract the
            // embedded copies and call `AddDllDirectory`. Confirmed live:
            // without `/DELAYLOAD`, som.exe fails to start at all
            // (STATUS_DLL_NOT_FOUND) on a machine with no system-wide
            // FFmpeg install, even though nothing in `main` had run yet.
            // `/DELAYLOAD` makes the linker generate a thunk that only
            // resolves each DLL on its FIRST actual call — by which point
            // `main`'s extraction/search-path setup has already run.
            // Requires linking `delayimp.lib` (MSVC's delay-load runtime
            // support) alongside the `/DELAYLOAD` flags themselves.
            for dll in
                ["avcodec-63.dll", "avformat-63.dll", "avutil-61.dll", "swresample-7.dll", "swscale-10.dll"]
            {
                println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
            }
            println!("cargo:rustc-link-arg=delayimp.lib");
        }

        println!("cargo:rerun-if-env-changed=RELEASE_CHANNEL");
        println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");

        #[cfg(windows)]
        {
            windows_resources::compile(false).expect("failed to compile Windows resources");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    prepare_app_icon_x11();
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn icon_path() -> std::path::PathBuf {
    use std::str::FromStr;

    let release_channel = option_env!("RELEASE_CHANNEL").unwrap_or("dev");
    let channel = match release_channel {
        "stable" => "",
        "preview" => "-preview",
        "nightly" => "-nightly",
        "dev" => "-dev",
        _ => "-dev",
    };

    #[cfg(windows)]
    let icon = format!("resources/windows/app-icon{}.ico", channel);
    #[cfg(not(windows))]
    let icon = format!("resources/app-icon{}.png", channel);

    std::path::PathBuf::from_str(&icon).unwrap()
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn prepare_app_icon_x11() {
    use image::{ImageReader, imageops};
    use std::env;
    use std::path::Path;

    let out_dir = env::var("OUT_DIR").unwrap();

    let resized_image = ImageReader::open(icon_path())
        .unwrap()
        .decode()
        .unwrap()
        .resize(256, 256, imageops::FilterType::Lanczos3);

    // name should match include_bytes! call in src/zed.rs
    let icon_out_path = Path::new(&out_dir).join("app_icon.png");
    resized_image.save(&icon_out_path).expect("saving app icon");

    println!("cargo:rerun-if-env-changed=RELEASE_CHANNEL");
    println!("cargo:rerun-if-changed={}", icon_path().to_string_lossy());
}
