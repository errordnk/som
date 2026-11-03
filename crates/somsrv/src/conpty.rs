//! The ConPTY every Som terminal runs in on Windows: the Windows Terminal
//! project's `conpty.dll` + `OpenConsole.exe` (an unmodified Microsoft build,
//! Microsoft.Windows.Console.ConPTY 1.24.260303001), embedded here and written
//! to a folder in tmp named after their content.
//!
//! A deliberate departure from Windows' defaults: Windows' own conhost drops
//! APC strings (`ESC _ ... ESC \`), and SRP's link frames are APC strings, as
//! is anything else a program sends that conhost does not know. Checked
//! 2026-11-03: `before<APC>after` came out of conhost as `beforeafter`, and
//! intact out of OpenConsole. Both Som's window (`terminal`) and `somsrv`'s
//! sessions (the shells of every Windows tab, whose raw output goes on to
//! Som) take it from here, so the console never depends on what else is
//! installed: the DLL is loaded by full path only (see the `alacritty`
//! fork's `set_conpty_dir`), never found on PATH.
//!
//! [`wire`] runs before every pseudoconsole is created. The first one of a
//! process checks every byte (and writes again anything that is not ours);
//! later ones only check that the files are still there, as tmp may have
//! been cleaned in the meantime.
//!
//! To update: put the new pair from the nupkg's `runtimes/win-x64/native/
//! conpty.dll` and `build/native/runtimes/x64/OpenConsole.exe` through zstd
//! (level 19) into `assets/conpty/*.zst`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::tmp_files;

const FILES: [(&str, &[u8]); 2] = [
    ("conpty.dll", include_bytes!("../../../assets/conpty/conpty.dll.zst")),
    ("OpenConsole.exe", include_bytes!("../../../assets/conpty/OpenConsole.exe.zst")),
];

/// Whether this process has already checked the files byte for byte.
static VERIFIED: AtomicBool = AtomicBool::new(false);

/// Makes the next pseudoconsole use our ConPTY. If it cannot be written,
/// Windows' own is used and the reason is logged.
pub fn wire() {
    let dir = folder(&std::env::temp_dir());
    let present = || FILES.iter().all(|(name, _)| dir.join(name).is_file());
    if VERIFIED.load(Ordering::Relaxed) && present() {
        alacritty_terminal::tty::set_conpty_dir(dir);
        return;
    }
    match ensure(&dir) {
        Ok(()) => {
            VERIFIED.store(true, Ordering::Relaxed);
            alacritty_terminal::tty::set_conpty_dir(dir);
        },
        Err(err) => log::error!("ConPTY files not written, Windows' own console is used: {err:#}"),
    }
}

fn folder(tmp: &Path) -> PathBuf {
    tmp_files::folder(tmp, "som-conpty", &FILES.map(|(_, compressed)| compressed))
}

/// Writes the files into `dir` unless they are already there, byte for byte.
fn ensure(dir: &Path) -> anyhow::Result<()> {
    for (name, compressed) in FILES {
        let bytes = zstd::stream::decode_all(compressed)?;
        tmp_files::write_unless_same(&dir.join(name), &bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_are_written_once_and_rewritten_when_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = folder(tmp.path());
        ensure(&dir).unwrap();
        let dll = dir.join("conpty.dll");
        assert!(dll.is_file() && dir.join("OpenConsole.exe").is_file());
        // a stranger's copy in its place is replaced
        std::fs::write(&dll, b"not ours").unwrap();
        ensure(&dir).unwrap();
        let expected = zstd::stream::decode_all(FILES[0].1).unwrap();
        assert_eq!(std::fs::read(&dll).unwrap(), expected);
    }

    /// The ConPTY comes from this module only (AGENTS.md, "Windows defaults,
    /// and the one ConPTY exception"): no other code may name the DLL, which
    /// is how a by-name load, a copy beside the exe or a build-time download
    /// would come back.
    #[test]
    fn no_other_code_loads_conpty_by_name() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let this = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("conpty.rs");
        let mut offenders = Vec::new();
        let mut stack = vec![crates];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name != "target") {
                        stack.push(path);
                    }
                } else if path.extension().is_some_and(|ext| ext == "rs" || ext == "iss" || ext == "toml")
                    && !same_file(&path, &this)
                    && std::fs::read_to_string(&path).is_ok_and(|text| text.to_lowercase().contains("conpty.dll"))
                {
                    offenders.push(path);
                }
            }
        }
        assert!(offenders.is_empty(), "only crates/somsrv/src/conpty.rs may name conpty.dll: {offenders:?}");
    }

    /// Som keeps only settings and logs on disk (AGENTS.md, "On-disk layout"):
    /// nothing may use `data_dir()` (`~/.local/share/som`) or a cache folder
    /// of its own. `paths::temp_dir()` is allowed only as `fs`'s fallback for
    /// an atomic write next to a file that has no parent folder.
    #[test]
    fn nothing_is_kept_outside_settings_and_logs() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let this = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("conpty.rs");
        let mut offenders = Vec::new();
        let mut stack = vec![crates];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if same_file(&path, &this) {
                    continue;
                }
                if path.is_dir() {
                    // `paths` defines these folders (and keeps Zed's unused ones)
                    if path.file_name().is_some_and(|name| name != "target" && name != "paths") {
                        stack.push(path);
                    }
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let allowed_temp = path.ends_with(Path::new("fs").join("src").join("fs.rs"));
                if text.contains("paths::data_dir(")
                    || text.contains("paths::hang_traces_dir(")
                    || (text.contains("paths::temp_dir(") && !allowed_temp)
                {
                    offenders.push(path);
                }
            }
        }
        assert!(offenders.is_empty(), "Som keeps only settings and logs on disk; these use other folders: {offenders:?}");
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        a.canonicalize().ok() == b.canonicalize().ok()
    }
}
