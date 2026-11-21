//! The user's decision, per repository folder, to let its `.g4v/binaries.toml` install files and run commands.
//!
//! A decision is tied to the hash of the config it was made for: when a sync brings a different config, g4v asks
//! again. Until a repository is trusted, Get Latest behaves exactly as it does without binaries.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Trusted,
    Declined,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Entry {
    hash: String,
    decision: Decision,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct TrustStore {
    #[serde(default)]
    repos: BTreeMap<String, Entry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustState {
    /// Never asked, or the config changed since the user decided.
    Unknown,
    Trusted,
    Declined,
}

fn key(root: &Path) -> String {
    let s = root.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

impl TrustStore {
    pub fn path() -> Option<PathBuf> {
        crate::settings::Settings::path().and_then(|p| p.parent().map(|d| d.join("binaries-trust.json")))
    }

    pub fn load() -> TrustStore {
        Self::path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let p = Self::path().ok_or("no settings folder")?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let dir = p.parent().ok_or("no settings folder")?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut tmp, &text).map_err(|e| e.to_string())?;
        tmp.persist(&p).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn state(&self, root: &Path, config_hash: &str) -> TrustState {
        match self.repos.get(&key(root)) {
            Some(e) if e.hash == config_hash => match e.decision {
                Decision::Trusted => TrustState::Trusted,
                Decision::Declined => TrustState::Declined,
            },
            _ => TrustState::Unknown,
        }
    }

    pub fn set(&mut self, root: &Path, config_hash: &str, decision: Decision) {
        self.repos.insert(key(root), Entry { hash: config_hash.to_string(), decision });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_are_tied_to_the_config_hash() {
        let mut s = TrustStore::default();
        let root = Path::new("/work/project");
        assert_eq!(s.state(root, "h1"), TrustState::Unknown);
        s.set(root, "h1", Decision::Trusted);
        assert_eq!(s.state(root, "h1"), TrustState::Trusted);
        assert_eq!(s.state(root, "h2"), TrustState::Unknown, "a changed config must ask again");
        s.set(root, "h2", Decision::Declined);
        assert_eq!(s.state(root, "h2"), TrustState::Declined);
    }
}
