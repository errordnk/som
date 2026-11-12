//! Reading a plugin from `~/.config/som/plugins/<name>/` (`paths::plugins_dir`):
//! its `plugin.json` and every `*.lua` under the folder. Read each time an
//! instance starts, so an edited plugin is picked up by the next start.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// The most Lua a plugin may have, all files together.
const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
/// How deep the plugin's folder is searched for `*.lua`.
const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub manifest: serde_json::Value,
    /// `(path relative to the plugin's folder with '/', text)`.
    pub sources: Vec<(String, String)>,
}

impl Plugin {
    pub fn title(&self) -> String {
        self.manifest.get("title").and_then(|title| title.as_str()).unwrap_or(&self.name).to_owned()
    }

    /// Whether the manifest grants `permission` (`"permissions"`).
    pub fn grants(&self, permission: &str) -> bool {
        self.manifest
            .get("permissions")
            .and_then(|permissions| permissions.as_array())
            .is_some_and(|permissions| permissions.iter().any(|granted| granted.as_str() == Some(permission)))
    }
}

pub fn load(name: &str) -> anyhow::Result<Plugin> {
    load_from(paths::plugins_dir(), name)
}

pub fn load_from(plugins_dir: &Path, name: &str) -> anyhow::Result<Plugin> {
    anyhow::ensure!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "a plugin name is letters, digits, '-' and '_': {name:?}"
    );
    let folder = plugins_dir.join(name);
    let manifest_path = folder.join("plugin.json");
    let manifest_text =
        std::fs::read_to_string(&manifest_path).with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&manifest_text).with_context(|| format!("parsing {}", manifest_path.display()))?;
    anyhow::ensure!(manifest.is_object(), "{} is not a JSON object", manifest_path.display());

    let mut files = Vec::new();
    collect_lua(&folder, &folder, 0, &mut files)?;
    files.sort();
    let mut total = 0u64;
    let mut sources = Vec::with_capacity(files.len());
    for (relative, path) in files {
        total += std::fs::metadata(&path)?.len();
        anyhow::ensure!(total <= MAX_SOURCE_BYTES, "plugin {name} has more than {MAX_SOURCE_BYTES} bytes of Lua");
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        sources.push((relative, text));
    }
    let entry = manifest.get("entry").and_then(|entry| entry.as_str()).unwrap_or("main.lua");
    anyhow::ensure!(
        sources.iter().any(|(path, _)| path == entry),
        "plugin {name} has no {entry} (the manifest's \"entry\")"
    );
    Ok(Plugin { name: name.to_owned(), manifest, sources })
}

fn collect_lua(root: &Path, folder: &Path, depth: usize, files: &mut Vec<(String, PathBuf)>) -> anyhow::Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    for entry in std::fs::read_dir(folder).with_context(|| format!("reading {}", folder.display()))? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_lua(root, &path, depth + 1, files)?;
        } else if kind.is_file() && path.extension().is_some_and(|extension| extension == "lua") {
            let relative = path.strip_prefix(root)?;
            let relative: Vec<String> =
                relative.components().map(|component| component.as_os_str().to_string_lossy().into_owned()).collect();
            files.push((relative.join("/"), path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_manifest_and_every_lua_file_with_slash_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin = dir.path().join("echo");
        std::fs::create_dir_all(plugin.join("lib")).expect("mkdir");
        std::fs::write(plugin.join("plugin.json"), r#"{ "name": "echo", "version": "1", "permissions": ["notify"] }"#)
            .expect("write");
        std::fs::write(plugin.join("main.lua"), "require('lib.util')").expect("write");
        std::fs::write(plugin.join("lib").join("util.lua"), "return {}").expect("write");
        std::fs::write(plugin.join("README.md"), "not lua").expect("write");

        let loaded = load_from(dir.path(), "echo").expect("loading");
        let paths: Vec<&str> = loaded.sources.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["lib/util.lua", "main.lua"]);
        assert!(loaded.grants("notify"));
        assert!(!loaded.grants("clipboard"));
        assert_eq!(loaded.title(), "echo");
    }

    #[test]
    fn refuses_odd_names_and_a_missing_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_from(dir.path(), "../x").is_err());
        let plugin = dir.path().join("empty");
        std::fs::create_dir_all(&plugin).expect("mkdir");
        std::fs::write(plugin.join("plugin.json"), r#"{ "name": "empty", "version": "1" }"#).expect("write");
        assert!(load_from(dir.path(), "empty").is_err());
    }
}
