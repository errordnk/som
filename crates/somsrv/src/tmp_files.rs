//! Files Som carries inside its binaries and puts in tmp for Windows to load
//! (the ConPTY, FFmpeg's DLLs): never kept in Som's own folders, never looked
//! for anywhere else. Each set gets a folder in tmp named after its content,
//! and is checked byte for byte every time it is about to be used: tmp may
//! have been cleaned in the meantime, or something not ours put there.

use std::io::Write as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// `%TEMP%\<prefix>-<hash of the embedded bytes>`, unique to this content: a
/// Som with other files gets another folder, and a running one keeps its own.
pub fn folder(tmp: &Path, prefix: &str, embedded: &[&[u8]]) -> PathBuf {
    let mut hash = Sha256::new();
    for bytes in embedded {
        hash.update(bytes);
    }
    tmp.join(format!("{prefix}-{}", hex::encode(&hash.finalize()[..6])))
}

/// Writes `bytes` to `path` unless it holds exactly them already.
pub fn write_unless_same(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // FILE_ATTRIBUTE_TEMPORARY: Windows keeps the file in its memory cache
    // and avoids writing it to the disk. (It has to be a file: a DLL is
    // loaded and an exe started by path, and delete-on-close would stop the
    // loader from opening it.)
    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
    // a file of ours that a running process holds cannot be replaced in
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
