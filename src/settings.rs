//! Application settings stored as JSON in the user's config directory.

use crate::git::ops::PullMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Repositories open as tabs (restored on start).
    pub open_repos: Vec<PathBuf>,
    pub active_repo: usize,
    pub recent_repos: Vec<PathBuf>,
    /// How often to check the server for new changes (0 = never).
    pub fetch_interval_secs: u64,
    /// Release LFS locks on files after they are submitted.
    pub unlock_after_submit: bool,
    pub pull_mode: PullMode,
    /// Warn when adding files larger than this that are not stored in LFS.
    pub large_file_mb: u64,
    pub dark_mode: bool,
    /// Remove automatic backup shelves after this many days (0 = keep forever).
    pub backup_retention_days: i64,
    /// Push right after committing (Perforce-style "Submit").
    pub push_on_submit: bool,
    pub ui_scale: f32,
    /// Use the Git installed on this computer instead of the one that came with g4v (takes effect on restart).
    pub use_system_git: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            open_repos: Vec::new(),
            active_repo: 0,
            recent_repos: Vec::new(),
            fetch_interval_secs: 180,
            unlock_after_submit: true,
            pull_mode: PullMode::Rebase,
            large_file_mb: 25,
            dark_mode: true,
            backup_retention_days: 30,
            push_on_submit: true,
            ui_scale: 1.0,
            use_system_git: false,
        }
    }
}

impl Settings {
    /// `G4V_CONFIG_DIR` overrides the location (useful for testing and portable installs).
    pub fn path() -> Option<PathBuf> {
        if let Some(d) = std::env::var_os("G4V_CONFIG_DIR") {
            return Some(PathBuf::from(d).join("settings.json"));
        }
        dirs::config_dir().map(|d| d.join("g4v").join("settings.json"))
    }

    pub fn load() -> Settings {
        let Some(p) = Self::path() else { return Settings::default() };
        let Ok(text) = std::fs::read_to_string(&p) else { return Settings::default() };
        serde_json::from_str(&text).unwrap_or_else(|_| {
            // Keep the unreadable file so the list of open projects can be recovered by hand.
            let _ = std::fs::copy(&p, p.with_extension("json.bak"));
            Settings::default()
        })
    }

    pub fn save(&self) {
        let Some(p) = Self::path() else { return };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            use std::io::Write;
            if let Some(parent) = p.parent() {
                if let Ok(mut tmp) = tempfile::NamedTempFile::new_in(parent) {
                    if tmp.write_all(text.as_bytes()).is_ok() && tmp.as_file().sync_all().is_ok() {
                        let _ = tmp.persist(p);
                    }
                }
            }
        }
    }

    pub fn add_recent(&mut self, p: PathBuf) {
        self.recent_repos.retain(|r| r != &p);
        self.recent_repos.insert(0, p);
        self.recent_repos.truncate(12);
    }
}
