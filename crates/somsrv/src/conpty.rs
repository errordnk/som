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
//! [`wire`] runs before every pseudoconsole is created: tmp may have been
//! cleaned in the meantime, and a file that is not byte for byte ours is
//! written again.
//!
//! To update: put the new pair from the nupkg's `runtimes/win-x64/native/
//! conpty.dll` and `build/native/runtimes/x64/OpenConsole.exe` through zstd
//! (level 19) into `assets/conpty/*.zst`.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const FILES: [(&str, &[u8]); 2] = [
    ("conpty.dll", include_bytes!("../../../assets/conpty/conpty.dll.zst")),
    ("OpenConsole.exe", include_bytes!("../../../assets/conpty/OpenConsole.exe.zst")),
];

/// Makes the next pseudoconsole use our ConPTY. If it cannot be written,
/// Windows' own is used and the reason is logged.
pub fn wire() {
    match ensure(&std::env::temp_dir()) {
        Ok(dir) => alacritty_terminal::tty::set_conpty_dir(dir),
        Err(err) => log::error!("ConPTY files not written, Windows' own console is used: {err:#}"),
    }
}

/// Writes the files into their folder in `tmp` unless they are already there,
/// byte for byte; returns the folder.
fn ensure(tmp: &Path) -> anyhow::Result<PathBuf> {
    let mut hash = Sha256::new();
    for (_, compressed) in FILES {
        hash.update(compressed);
    }
    let digest = hash.finalize();
    let dir = tmp.join(format!("som-conpty-{}", hex::encode(&digest[..6])));
    std::fs::create_dir_all(&dir)?;
    for (name, compressed) in FILES {
        let bytes = zstd::stream::decode_all(compressed)?;
        write_unless_same(&dir.join(name), &bytes)?;
    }
    Ok(dir)
}

fn write_unless_same(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    use std::io::Write as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    // FILE_ATTRIBUTE_TEMPORARY: Windows keeps the file in its memory cache
    // and avoids writing it to the disk. (It has to be a file: conpty.dll is
    // loaded and OpenConsole.exe started by path, and delete-on-close would
    // stop the loader from opening it.)
    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
    // a file of ours that a running console holds cannot be replaced in
    // place, but then it is not different either: write beside, then swap
    let fresh = path.with_extension("new");
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .attributes(FILE_ATTRIBUTE_TEMPORARY)
        .open(&fresh)?
        .write_all(bytes)?;
    std::fs::rename(&fresh, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_are_written_once_and_rewritten_when_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ensure(tmp.path()).unwrap();
        let dll = dir.join("conpty.dll");
        assert!(dll.is_file() && dir.join("OpenConsole.exe").is_file());
        // a stranger's copy in its place is replaced
        std::fs::write(&dll, b"not ours").unwrap();
        assert_eq!(ensure(tmp.path()).unwrap(), dir);
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

    fn same_file(a: &Path, b: &Path) -> bool {
        a.canonicalize().ok() == b.canonicalize().ok()
    }
}
