//! Branches, tags, remotes and submodules.

use super::{GitError, GitResult, Repo};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefKind {
    Local,
    Remote,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchRef {
    pub full: String,
    /// Short name, e.g. `main`, `origin/main`, `v1.0`.
    pub name: String,
    pub kind: RefKind,
    pub target: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Upstream branch was deleted on the server.
    pub gone: bool,
    pub is_head: bool,
    pub time: i64,
    pub subject: String,
}

/// Parses `%(upstream:track)` e.g. `[ahead 1, behind 2]` or `[gone]`.
pub fn parse_track(s: &str) -> (u32, u32, bool) {
    let s = s.trim().trim_start_matches('[').trim_end_matches(']');
    if s == "gone" {
        return (0, 0, true);
    }
    let mut ahead = 0;
    let mut behind = 0;
    for part in s.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    (ahead, behind, false)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submodule {
    pub path: String,
    pub initialized: bool,
    /// Checked-out commit differs from what the parent repo records.
    pub out_of_sync: bool,
    pub conflicted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    pub index: usize,
    pub time: i64,
    pub message: String,
}

/// Fast, process-free check of a branch name (mirrors `git check-ref-format --branch`).
pub fn branch_name_problem_local(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("Enter a name.".into());
    }
    if name.contains(' ') {
        return Some("Branch names cannot contain spaces (try dashes: my-feature).".into());
    }
    let bad_chars = ['~', '^', ':', '?', '*', '[', '\\'];
    if name.chars().any(|c| bad_chars.contains(&c) || c.is_control()) {
        return Some("Branch names cannot contain ~ ^ : ? * [ or \\".into());
    }
    if name.starts_with('-') || name.starts_with('/') || name.ends_with('/') || name.ends_with('.') || name.ends_with(".lock") {
        return Some("That is not a valid branch name.".into());
    }
    if name.contains("..") || name.contains("//") || name.contains("@{") || name == "@" || name.split('/').any(|c| c.starts_with('.')) {
        return Some("That is not a valid branch name.".into());
    }
    None
}

impl Repo {
    pub fn branches(&self) -> GitResult<Vec<BranchRef>> {
        let out = self
            .git()
            .args([
                "for-each-ref",
                "--format=%(refname)%1f%(refname:short)%1f%(objectname)%1f%(upstream:short)%1f%(upstream:track)%1f%(HEAD)%1f%(committerdate:unix)%1f%(subject)",
                "refs/heads",
                "refs/remotes",
                "refs/tags",
            ])
            .quiet()
            .run()?;
        let mut v = Vec::new();
        for line in out.text().lines() {
            let f: Vec<&str> = line.splitn(8, '\u{1f}').collect();
            if f.len() < 8 {
                continue;
            }
            let kind = if f[0].starts_with("refs/heads/") {
                RefKind::Local
            } else if f[0].starts_with("refs/remotes/") {
                if f[0].ends_with("/HEAD") {
                    continue;
                }
                RefKind::Remote
            } else {
                RefKind::Tag
            };
            let (ahead, behind, gone) = parse_track(f[4]);
            v.push(BranchRef {
                full: f[0].to_string(),
                name: f[1].to_string(),
                kind,
                target: f[2].to_string(),
                upstream: (!f[3].is_empty()).then(|| f[3].to_string()),
                ahead,
                behind,
                gone,
                is_head: f[5].trim() == "*",
                time: f[6].parse().unwrap_or(0),
                subject: f[7].to_string(),
            });
        }
        Ok(v)
    }

    /// Returns a human readable problem with a proposed branch name, if any.
    pub fn branch_name_problem(&self, name: &str) -> Option<String> {
        let name = name.trim();
        if name.is_empty() {
            return Some("Enter a name.".into());
        }
        if name.contains(' ') {
            return Some("Branch names cannot contain spaces (try dashes: my-feature).".into());
        }
        match self.git().args(["check-ref-format", "--branch", name]).quiet().run() {
            Ok(_) => None,
            Err(_) => Some("That is not a valid branch name.".into()),
        }
    }

    pub fn switch_branch(&self, name: &str) -> GitResult<()> {
        self.git().args(["switch", "--no-guess", "--"]).arg(name).run()?;
        self.update_submodules_if_any()?;
        Ok(())
    }

    /// Creates a local branch tracking `remote_branch` (e.g. `origin/feature`) and switches to it.
    pub fn checkout_remote_branch(&self, remote_branch: &str) -> GitResult<()> {
        let local = remote_branch.split_once('/').map(|(_, b)| b).unwrap_or(remote_branch);
        let exists = self.git().args(["show-ref", "--verify", "--quiet"]).arg(format!("refs/heads/{local}")).quiet().run().is_ok();
        if exists {
            self.switch_branch(local)
        } else {
            self.git().args(["switch", "--track", "-c"]).arg(local).arg(remote_branch).run()?;
            self.update_submodules_if_any()?;
            Ok(())
        }
    }

    pub fn create_branch(&self, name: &str, start: Option<&str>, switch: bool) -> GitResult<()> {
        if switch {
            let mut c = self.git().args(["switch", "-c"]).arg(name);
            if let Some(s) = start {
                c = c.arg(s);
            }
            c.run()?;
        } else {
            let mut c = self.git().arg("branch").arg(name);
            if let Some(s) = start {
                c = c.arg(s);
            }
            c.run()?;
        }
        Ok(())
    }

    /// Safe delete; fails if the branch has commits that exist nowhere else.
    pub fn delete_branch(&self, name: &str, force: bool) -> GitResult<()> {
        self.git().args(["branch", if force { "-D" } else { "-d" }, "--"]).arg(name).run()?;
        Ok(())
    }

    pub fn delete_remote_branch(&self, remote_branch: &str) -> GitResult<()> {
        let (remote, branch) = remote_branch.split_once('/').ok_or_else(|| GitError {
            command: "push --delete".into(),
            code: None,
            stderr: format!("{remote_branch} is not a remote branch"),
            stdout: String::new(),
        })?;
        self.git().args(["push", remote, "--delete"]).arg(branch).run()?;
        Ok(())
    }

    pub fn merge_into_current(&self, rev: &str) -> GitResult<()> {
        self.git().args(["merge", "--no-edit", "--autostash"]).arg(rev).run()?;
        Ok(())
    }

    pub fn create_tag(&self, name: &str, rev: &str, message: &str) -> GitResult<()> {
        let mut c = self.git().arg("tag");
        if !message.trim().is_empty() {
            c = c.args(["-a", "-m"]).arg(message.trim());
        }
        c.arg(name).arg(rev).run()?;
        Ok(())
    }

    pub fn delete_tag(&self, name: &str) -> GitResult<()> {
        self.git().args(["tag", "-d"]).arg(name).run()?;
        Ok(())
    }

    pub fn push_tag(&self, remote: &str, name: &str) -> GitResult<()> {
        self.git().arg("push").arg(remote).arg(format!("refs/tags/{name}")).run()?;
        Ok(())
    }

    pub fn remotes(&self) -> Vec<(String, String)> {
        let out = match self.git().args(["remote", "-v"]).quiet().run() {
            Ok(o) => o.text(),
            Err(_) => return Vec::new(),
        };
        let mut v: Vec<(String, String)> = Vec::new();
        for line in out.lines() {
            let mut it = line.split_whitespace();
            if let (Some(n), Some(u)) = (it.next(), it.next()) {
                if !v.iter().any(|(x, _)| x == n) {
                    v.push((n.to_string(), u.to_string()));
                }
            }
        }
        v
    }

    pub fn add_remote(&self, name: &str, url: &str) -> GitResult<()> {
        self.git().args(["remote", "add", "--"]).arg(name).arg(url).run()?;
        Ok(())
    }

    /// The remote to talk to: the current branch's upstream remote, else `origin`, else the first one.
    pub fn default_remote(&self) -> Option<String> {
        if let Ok(b) = self.run(&["symbolic-ref", "--quiet", "--short", "HEAD"]) {
            if let Some(r) = self.config_get(&format!("branch.{b}.remote")) {
                if r != "." {
                    return Some(r);
                }
            }
        }
        let remotes = self.remotes();
        if remotes.iter().any(|(n, _)| n == "origin") {
            return Some("origin".into());
        }
        remotes.into_iter().next().map(|(n, _)| n)
    }

    pub fn submodules(&self) -> Vec<Submodule> {
        if !self.root().join(".gitmodules").exists() {
            return Vec::new();
        }
        let out = match self.git().args(["submodule", "status"]).quiet().run() {
            Ok(o) => o.text(),
            Err(_) => return Vec::new(),
        };
        out.lines()
            .filter_map(|l| {
                let flag = l.chars().next()?;
                let rest = l.get(1..)?.trim_start();
                let mut it = rest.splitn(2, ' ');
                let _sha = it.next()?;
                let tail = it.next()?.trim();
                // path may be followed by " (describe)"
                let path = match tail.rfind(" (") {
                    Some(i) if tail.ends_with(')') => &tail[..i],
                    _ => tail,
                };
                Some(Submodule { path: path.to_string(), initialized: flag != '-', out_of_sync: flag == '+', conflicted: flag == 'U' })
            })
            .collect()
    }

    /// Brings submodules to the commits the project pins. Every submodule is initialized and updated, except the
    /// ones the project marks optional (see [`super::submodules`]): those are updated only where already checked
    /// out, so a teammate without access to a private one is never blocked by it.
    pub fn update_submodules_if_any(&self) -> GitResult<()> {
        if !self.root().join(".gitmodules").exists() {
            return Ok(());
        }
        let optional = super::submodules::optional(self.root());
        if optional.is_empty() {
            self.git().args(["submodule", "update", "--init", "--recursive"]).run()?;
            return Ok(());
        }
        let listing = self
            .git()
            .args(["config", "-f", ".gitmodules", "--get-regexp", r"^submodule\..*\.path$"])
            .ok_codes(&[0, 1])
            .quiet()
            .run()?
            .text();
        let all = super::submodules::parse_paths(&listing);
        let (required, present) = super::submodules::split(self.root(), &all, &optional);
        if !required.is_empty() {
            self.git().args(["submodule", "update", "--init", "--recursive", "--"]).args(&required).run()?;
        }
        if !present.is_empty() {
            self.git().args(["submodule", "update", "--recursive", "--"]).args(&present).run()?;
        }
        Ok(())
    }

    pub fn stashes(&self) -> Vec<Stash> {
        let out = match self.git().args(["stash", "list", "--format=%ct%x1f%gs"]).quiet().run() {
            Ok(o) => o.text(),
            Err(_) => return Vec::new(),
        };
        out.lines()
            .enumerate()
            .filter_map(|(i, l)| {
                let (t, m) = l.split_once('\u{1f}')?;
                Some(Stash { index: i, time: t.parse().unwrap_or(0), message: m.to_string() })
            })
            .collect()
    }

    pub fn stash_apply(&self, index: usize, drop: bool) -> GitResult<()> {
        self.git().args(["stash", if drop { "pop" } else { "apply" }]).arg(format!("stash@{{{index}}}")).run()?;
        Ok(())
    }

    pub fn stash_drop(&self, index: usize) -> GitResult<()> {
        self.git().args(["stash", "drop"]).arg(format!("stash@{{{index}}}")).run()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_branch_validation() {
        assert!(branch_name_problem_local("feature/new-level").is_none());
        for bad in ["a b", "a..b", "-x", "x/", "x.lock", "a~1", "a:b", "x/.hidden", "a@{b"] {
            assert!(branch_name_problem_local(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn track_parsing() {
        assert_eq!(parse_track("[ahead 1, behind 2]"), (1, 2, false));
        assert_eq!(parse_track("[behind 5]"), (0, 5, false));
        assert_eq!(parse_track("[gone]"), (0, 0, true));
        assert_eq!(parse_track(""), (0, 0, false));
    }
}
