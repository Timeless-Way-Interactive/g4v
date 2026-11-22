//! Which submodules g4v fetches for everyone, from the project's `.g4v/submodules.toml`:
//!
//! ```toml
//! # Private to a few people: updated for whoever already has it, never cloned on anyone else's behalf.
//! optional = ["BusinessOrganization"]
//! ```
//!
//! Without the file every submodule is required, as plain `git submodule update --init --recursive` treats them.
//! Reading it needs no trust: it can only make g4v fetch less.

use serde::Deserialize;
use std::path::Path;

pub const CONFIG_PATH: &str = ".g4v/submodules.toml";

#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    optional: Vec<String>,
}

/// Top-level submodule paths the project marks optional (normalized to forward slashes, no trailing slash).
pub fn optional(root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join(CONFIG_PATH)) else { return Vec::new() };
    toml::from_str::<File>(text.trim_start_matches('\u{feff}'))
        .map(|f| f.optional.into_iter().map(|p| p.replace('\\', "/").trim_end_matches('/').to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default()
}

/// Whether a submodule has been checked out here (its `.git` file or folder exists).
pub fn is_present(root: &Path, path: &str) -> bool {
    root.join(path).join(".git").exists()
}

/// Splits the top-level submodules into those to initialize and update (`required`) and the optional ones this
/// workspace already has, which are updated but never initialized.
pub fn split(root: &Path, all: &[String], optional: &[String]) -> (Vec<String>, Vec<String>) {
    let is_opt = |p: &str| optional.iter().any(|o| o.eq_ignore_ascii_case(p));
    let required = all.iter().filter(|p| !is_opt(p)).cloned().collect();
    let present = all.iter().filter(|p| is_opt(p) && is_present(root, p)).cloned().collect();
    (required, present)
}

/// Parses `git config -f .gitmodules --get-regexp '^submodule\..*\.path$'`.
pub fn parse_paths(text: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.split_once(' ').map(|(_, p)| p.trim().to_string())).filter(|p| !p.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_submodules_are_only_updated_where_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".g4v")).unwrap();
        std::fs::write(dir.path().join(CONFIG_PATH), "optional = [\"Business/\"]\n").unwrap();
        let all = parse_paths("submodule.Engine.path Engine\nsubmodule.Business.path Business\n");
        assert_eq!(all, ["Engine", "Business"]);
        let opt = optional(dir.path());
        assert_eq!(opt, ["Business"]);
        assert_eq!(split(dir.path(), &all, &opt), (vec!["Engine".to_string()], vec![]));
        std::fs::create_dir_all(dir.path().join("Business")).unwrap();
        std::fs::write(dir.path().join("Business/.git"), "gitdir: x").unwrap();
        assert_eq!(split(dir.path(), &all, &opt), (vec!["Engine".to_string()], vec!["Business".to_string()]));
    }

    #[test]
    fn no_file_means_everything_is_required() {
        let dir = tempfile::tempdir().unwrap();
        assert!(optional(dir.path()).is_empty());
    }
}
