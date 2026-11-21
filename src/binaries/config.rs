//! `.g4v/binaries.toml`: a repository's declaration that it ships precompiled binaries.
//!
//! ```toml
//! notes_ref        = "refs/notes/binaries"
//! release_repo     = "Owner/Name"
//! install_roots    = ["Game/Binaries", "UnrealEngine/Engine/Binaries"]
//! marker           = "Game/Binaries/Win64/.binkey"
//! block_if_running = ["DungeonBreakEditor.exe", "ShaderCompileWorker.exe"]
//! source_paths     = ["Game/Source", "Game/Plugins", "Game/DungeonBreak.uproject"]
//! launch           = ["Game/Binaries/Win64/DungeonBreakEditor.exe", "Game/DungeonBreak.uproject"]
//! post_sync        = { run = ["Sync.ps1", "-DependenciesOnly"], when_changed = ["UnrealEngine"] }
//! ```
//!
//! Every path is checked here, when the file is loaded, so the rest of the module can rely on them.

use super::paths::{is_safe_rel, is_under};
use serde::Deserialize;
use std::path::Path;

pub const CONFIG_PATH: &str = ".g4v/binaries.toml";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BinariesConfig {
    /// Where CI records per-commit build state, e.g. `refs/notes/binaries`.
    pub notes_ref: String,
    /// `owner/name` of the GitHub repository whose releases hold the builds.
    pub release_repo: String,
    /// The only folders files may be written to. Every manifest entry must be inside one.
    pub install_roots: Vec<String>,
    /// File holding the key of the installed build. The installed manifest is kept next to it.
    pub marker: String,
    /// Executables that hold the installed files open; installing waits until they are closed.
    #[serde(default)]
    pub block_if_running: Vec<String>,
    /// Local edits under these paths mean the user builds their own binaries (a programmer).
    #[serde(default)]
    pub source_paths: Vec<String>,
    /// Program to start for "Launch", then its arguments. Arguments naming an existing workspace path are passed
    /// as absolute paths.
    #[serde(default)]
    pub launch: Vec<String>,
    /// A command to run after the workspace moved, when one of `when_changed` changed.
    #[serde(default)]
    pub post_sync: Option<PostSync>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PostSync {
    pub run: Vec<String>,
    /// Run only when one of these paths differs between the old and new commit. Empty = always.
    #[serde(default)]
    pub when_changed: Vec<String>,
}

/// A config as loaded from disk, with the hash that trust decisions are tied to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedConfig {
    pub config: BinariesConfig,
    pub hash: String,
}

impl BinariesConfig {
    pub fn parse(text: &str) -> Result<BinariesConfig, String> {
        let c: BinariesConfig = toml::from_str(text).map_err(|e| format!("{CONFIG_PATH} is not valid: {e}"))?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<(), String> {
        let bad = |what: &str, v: &str| Err(format!("{CONFIG_PATH}: {what} \"{v}\" is not allowed"));
        let r = &self.notes_ref;
        if !r.starts_with("refs/notes/") || !is_safe_rel(r) || r.contains(char::is_whitespace) {
            return bad("notes_ref", r);
        }
        let repo_ok = self.release_repo.split_once('/').is_some_and(|(o, n)| {
            let part = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
            part(o) && part(n) && n != "." && n != ".."
        });
        if !repo_ok {
            return bad("release_repo", &self.release_repo);
        }
        if self.install_roots.is_empty() {
            return Err(format!("{CONFIG_PATH}: install_roots must list at least one folder"));
        }
        for p in self.install_roots.iter().chain(&self.source_paths) {
            if !is_safe_rel(p) {
                return bad("path", p);
            }
        }
        // `.git` is never a place to install into, at any depth.
        if self.install_roots.iter().any(|p| p.split('/').any(|s| s.eq_ignore_ascii_case(".git"))) {
            return bad("install root", ".git");
        }
        if !is_safe_rel(&self.marker) || !self.in_install_root(&self.marker) {
            return bad("marker (must be inside an install root)", &self.marker);
        }
        if let Some(exe) = self.launch.first() {
            if !is_safe_rel(exe) {
                return bad("launch program", exe);
            }
        }
        if let Some(ps) = &self.post_sync {
            match ps.run.first() {
                Some(exe) if is_safe_rel(exe) => {}
                Some(exe) => return bad("post_sync program", exe),
                None => return Err(format!("{CONFIG_PATH}: post_sync.run is empty")),
            }
            for p in &ps.when_changed {
                if !is_safe_rel(p) {
                    return bad("post_sync.when_changed path", p);
                }
            }
        }
        for name in &self.block_if_running {
            if name.is_empty() || name.contains(['/', '\\', ':', '"', '*', '?']) {
                return bad("block_if_running entry", name);
            }
        }
        Ok(())
    }

    pub fn in_install_root(&self, rel: &str) -> bool {
        self.install_roots.iter().any(|root| is_under(rel, root))
    }

    /// The installed manifest lives next to the marker (`.binmanifest.json`), the same place the PowerShell
    /// fetch script keeps it, so both tools see the same install.
    pub fn installed_manifest(&self) -> String {
        match self.marker.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/.binmanifest.json"),
            None => ".binmanifest.json".into(),
        }
    }
}

impl LoadedConfig {
    /// Reads the config from the working tree. `Ok(None)` when the repository doesn't use binaries.
    pub fn load(root: &Path) -> Result<Option<LoadedConfig>, String> {
        let path = root.join(CONFIG_PATH);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Could not read {CONFIG_PATH}: {e}")),
        };
        let text = String::from_utf8(super::strip_bom(&bytes).to_vec()).map_err(|_| format!("{CONFIG_PATH} is not UTF-8"))?;
        let config = BinariesConfig::parse(&text)?;
        Ok(Some(LoadedConfig { config, hash: super::sha256_hex(&bytes) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
notes_ref        = "refs/notes/binaries"
release_repo     = "Owner/Repo-Name"
install_roots    = ["Game/Binaries", "UnrealEngine/Engine/Binaries"]
marker           = "Game/Binaries/Win64/.binkey"
block_if_running = ["Editor.exe"]
source_paths     = ["Game/Source"]
launch           = ["Game/Binaries/Win64/Editor.exe", "Game/Game.uproject"]
post_sync        = { run = ["Sync.ps1", "-DependenciesOnly"], when_changed = ["UnrealEngine"] }
"#;

    #[test]
    fn parses_the_documented_example() {
        let c = BinariesConfig::parse(GOOD).unwrap();
        assert_eq!(c.installed_manifest(), "Game/Binaries/Win64/.binmanifest.json");
        assert!(c.in_install_root("UnrealEngine/Engine/Binaries/Win64/x.dll"));
        assert!(!c.in_install_root("UnrealEngine/Engine/Config/x.ini"));
    }

    #[test]
    fn refuses_unsafe_values() {
        for (from, to) in [
            ("\"Game/Binaries\", ", "\"../outside\", "),
            ("Game/Binaries/Win64/.binkey", "Elsewhere/.binkey"),
            ("Owner/Repo-Name", "Owner/../x"),
            ("refs/notes/binaries", "refs/heads/main"),
            ("[\"Sync.ps1\"", "[\"C:/Windows/evil.exe\""),
            ("\"Game/Binaries/Win64/Editor.exe\"", "\"/usr/bin/evil\""),
            ("\"Game/Binaries\", ", "\"Game/.git/hooks\", "),
        ] {
            let text = GOOD.replace(from, to);
            assert_ne!(text, GOOD, "replacement {from} did not apply");
            assert!(BinariesConfig::parse(&text).is_err(), "accepted: {to}");
        }
        assert!(BinariesConfig::parse(&format!("{GOOD}\nunknown = 1\n")).is_err());
    }
}
