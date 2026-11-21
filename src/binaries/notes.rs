//! Per-commit build notes written by CI under the configured notes ref.
//!
//! ```json
//! { "state": "ready", "key": "8cf6b2b5666ef728", "release": "bin-8cf6b2b5666ef728",
//!   "archive":  { "name": "Editor-Win64-Development.zip",           "sha256": "…", "bytes": 610271232 },
//!   "manifest": { "name": "Editor-Win64-Development.manifest.json", "sha256": "…" } }
//! ```

use crate::git::{GitResult, Repo};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NoteState {
    Building,
    Ready,
    Failed,
    Superseded,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AssetRef {
    pub name: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Note {
    pub state: NoteState,
    pub key: String,
    pub release: String,
    #[serde(default)]
    pub archive: Option<AssetRef>,
    #[serde(default)]
    pub manifest: Option<AssetRef>,
}

impl Note {
    /// A ready note that names everything an install needs, with names that are plain file names.
    pub fn installable(&self) -> bool {
        let plain = |s: &str| !s.is_empty() && super::paths::is_safe_rel(s) && !s.contains('/');
        self.state == NoteState::Ready
            && plain(&self.key)
            && plain(&self.release)
            && self.archive.as_ref().is_some_and(|a| plain(&a.name))
            && self.manifest.as_ref().is_some_and(|m| plain(&m.name))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitNote {
    pub commit: String,
    /// `None` when the commit has no note, or its note is unreadable.
    pub note: Option<Note>,
}

/// Fetches the notes ref from `remote`. `Ok(false)` when the server has no such ref yet (CI hasn't written any).
pub fn fetch(repo: &Repo, remote: &str, notes_ref: &str) -> GitResult<bool> {
    let spec = format!("+{notes_ref}:{notes_ref}");
    match repo.git().args(["fetch", "--quiet", "--no-tags"]).arg(remote).arg(&spec).quiet().run() {
        Ok(_) => Ok(true),
        Err(e) if e.stderr.contains("couldn't find remote ref") => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn exists(repo: &Repo, notes_ref: &str) -> bool {
    repo.git().args(["rev-parse", "--verify", "--quiet"]).arg(notes_ref).quiet().run().is_ok()
}

/// The first-parent history of `tip` (newest first, at most `limit` commits) with each commit's note.
pub fn first_parent(repo: &Repo, tip: &str, limit: usize, notes_ref: &str) -> GitResult<Vec<CommitNote>> {
    let out = repo
        .git()
        .args(["log", "--first-parent", "--format=%H%x1f%N%x1e"])
        .arg(format!("--notes={notes_ref}"))
        .arg(format!("-n{limit}"))
        .arg(tip)
        .arg("--")
        .quiet()
        .run()?;
    Ok(parse_log(&out.text()))
}

pub(crate) fn parse_log(text: &str) -> Vec<CommitNote> {
    text.split('\x1e')
        .filter_map(|rec| {
            let rec = rec.trim_start_matches(['\n', '\r']);
            let (commit, body) = rec.split_once('\x1f')?;
            let commit = commit.trim();
            if commit.len() < 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let body = body.trim();
            let note = if body.is_empty() { None } else { serde_json::from_str::<Note>(body).ok() };
            Some(CommitNote { commit: commit.to_string(), note })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_records() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        let note = r#"{"state":"ready","key":"k1","release":"bin-k1","archive":{"name":"x.zip"},"manifest":{"name":"x.json"}}"#;
        let text = format!("{a}\x1f{note}\n\x1e\n{b}\x1f\n\x1e\n{c}\x1fnot json\n\x1e\n");
        let v = parse_log(&text);
        assert_eq!(v.len(), 3);
        assert!(v[0].note.as_ref().unwrap().installable());
        assert!(v[1].note.is_none());
        assert!(v[2].note.is_none());
    }

    #[test]
    fn refuses_names_with_paths() {
        let mut n = Note {
            state: NoteState::Ready,
            key: "k".into(),
            release: "bin-k".into(),
            archive: Some(AssetRef { name: "../x.zip".into(), sha256: None, bytes: None }),
            manifest: Some(AssetRef { name: "m.json".into(), sha256: None, bytes: None }),
        };
        assert!(!n.installable());
        n.archive.as_mut().unwrap().name = "x.zip".into();
        assert!(n.installable());
        n.state = NoteState::Building;
        assert!(!n.installable());
    }
}
