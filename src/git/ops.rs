//! Higher level workflows: submit, revert, get latest, conflict resolution.
//!
//! These functions encode the "safe path" through git so the UI never has to
//! improvise sequences of commands.

use super::status::{FileStatus, RepoOp};
use super::{run_global, CmdLog, GitError, GitResult, Repo};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl Identity {
    pub fn complete(&self) -> bool {
        self.name.is_some() && self.email.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum PullMode {
    /// Replay my unpushed commits on top of the latest (linear, Perforce-like history).
    #[default]
    Rebase,
    /// Create a merge commit when both sides have new commits.
    Merge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    UpToDate,
    Updated {
        commits: u32,
    },
    /// The current branch has never been published.
    NoUpstream,
    /// Local edits touch files that were also changed on the server.
    Blocked {
        overlap: Vec<String>,
    },
    /// Merge/rebase stopped with conflicts that need resolving.
    Conflicts,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed,
    /// Server has commits we don't have yet: need to get latest first.
    Rejected(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Mine,
    Theirs,
}

/// Is `err` a "the server has newer commits" push rejection?
pub fn is_non_fast_forward(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("[rejected]") && (s.contains("fetch first") || s.contains("non-fast-forward"))
        || s.contains("updates were rejected because the remote contains work")
        || s.contains("updates were rejected because the tip of your current branch is behind")
}

impl Repo {
    pub fn identity(&self) -> Identity {
        Identity { name: self.config_get("user.name"), email: self.config_get("user.email") }
    }

    pub fn set_identity(&self, name: &str, email: &str, global: bool) -> GitResult<()> {
        let scope = if global { "--global" } else { "--local" };
        self.git().args(["config", scope, "user.name"]).arg(name.trim()).run()?;
        self.git().args(["config", scope, "user.email"]).arg(email.trim()).run()?;
        Ok(())
    }

    pub fn has_head(&self) -> bool {
        self.git().args(["rev-parse", "--verify", "--quiet", "HEAD"]).quiet().run().is_ok()
    }

    /// Fetches the default remote (branches, tags and shared shelves).
    pub fn fetch(&self) -> GitResult<Option<String>> {
        let Some(remote) = self.default_remote() else { return Ok(None) };
        self.git().args(["fetch", "--prune", "--quiet"]).arg(&remote).quiet().run()?;
        let _ = self.fetch_shelves(&remote);
        Ok(Some(remote))
    }

    /// Files changed on the upstream branch that are not in my HEAD yet
    /// ("not at latest revision" in Perforce terms).
    pub fn incoming_files(&self) -> GitResult<Vec<String>> {
        Ok(self.incoming_changes()?.into_iter().map(|(_, p)| p).collect())
    }

    /// Like [`Self::incoming_files`] with the change letter (A/M/D/T).
    pub fn incoming_changes(&self) -> GitResult<Vec<(char, String)>> {
        let r = self.git().args(["diff", "--no-ext-diff", "--name-status", "-z", "--no-renames", "HEAD...@{upstream}"]).quiet().run();
        match r {
            Ok(out) => Ok(super::history::parse_name_status(&out.stdout).into_iter().map(|f| (f.status, f.path)).collect()),
            Err(e)
                if e.stderr.contains("no upstream")
                    || e.stderr.contains("unknown revision")
                    || e.stderr.contains("ambiguous argument")
                    || e.stderr.contains("bad revision") =>
            {
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        }
    }

    /// Commits `paths` (and only those paths) with `message`.
    /// Anything else that happens to be staged is left untouched.
    pub fn commit_paths(&self, paths: &[String], message: &str) -> GitResult<String> {
        if message.trim().is_empty() {
            return Err(user_error("commit", "Please write a description before submitting."));
        }
        if paths.is_empty() {
            return Err(user_error("commit", "No files selected."));
        }
        let status = self.status()?;
        if status.has_conflicts() || status.op != RepoOp::None {
            return Err(user_error("commit", "Finish resolving the current operation and use Continue before submitting."));
        }
        self.backup_staged_paths(paths)?;
        self.stage_paths(None, paths)?;
        // A render may have disappeared since the dialog opened. Only pass actual selected changes to --only;
        // unknown paths otherwise reject the whole batch. This does not expand the user's selection.
        let staged = self.git().args(["diff", "--cached", "--name-only", "--no-renames", "-z"]).quiet().run()?;
        let changed: HashSet<String> = super::split_nul(&staged.stdout).into_iter().collect();
        let selected: Vec<&String> = paths.iter().filter(|p| changed.contains(*p)).collect();
        if selected.is_empty() {
            return Err(user_error("commit", "The selected files have no changes to submit. Refresh the project."));
        }
        std::fs::create_dir_all(self.meta_dir()).ok();
        let msg_file = self.meta_dir().join("COMMIT_MSG");
        std::fs::write(&msg_file, message.trim().as_bytes())
            .map_err(|e| user_error("commit", &format!("Could not write the commit message: {e}")))?;
        let commit = self.git().args(["commit", "--cleanup=whitespace", "-F"]).arg(&msg_file);
        // Enforce selection inside Git's index transaction. Checking the index and then running a plain commit
        // races another editor staging files between those commands.
        let res = commit.args(["--only", "--pathspec-from-file=-", "--pathspec-file-nul"]).stdin_paths(&selected).run();
        let _ = std::fs::remove_file(&msg_file);
        res?;
        self.run(&["rev-parse", "HEAD"])
    }

    /// Pushes the current branch to its upstream (or publishes it if it has none).
    pub fn push_current(&self) -> GitResult<PushOutcome> {
        let branch = self
            .run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .map_err(|_| user_error("push", "You are not on a branch (detached HEAD). Create a branch first."))?;
        let upstream_remote = self.config_get(&format!("branch.{branch}.remote")).filter(|r| r != ".");
        let upstream_merge = self.config_get(&format!("branch.{branch}.merge"));
        let res = match (upstream_remote, upstream_merge) {
            (Some(remote), Some(merge)) => self.git().arg("push").arg(&remote).arg(format!("HEAD:{merge}")).run(),
            _ => {
                let remote =
                    self.default_remote().ok_or_else(|| user_error("push", "This repository has no server (remote) configured."))?;
                self.git().args(["push", "-u"]).arg(&remote).arg("HEAD").run()
            }
        };
        match res {
            Ok(_) => Ok(PushOutcome::Pushed),
            Err(e) if is_non_fast_forward(&e.stderr) => Ok(PushOutcome::Rejected(e.to_string())),
            Err(e) => Err(e),
        }
    }

    /// Discards local changes to `files`. Callers should back them up first.
    pub fn revert_files(&self, files: &[FileStatus]) -> GitResult<()> {
        // A queued action can outlive the status shown on screen (or another Git client can have committed it).
        let wanted: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let current = self.status()?;
        let files: Vec<_> = current.files.iter().filter(|f| wanted.contains(f.path.as_str())).collect();
        let paths: Vec<String> = files.iter().flat_map(|f| std::iter::once(f.path.clone()).chain(f.orig_path.clone())).collect();
        self.backup_staged_paths(&paths)?;
        let mut restore: Vec<String> = Vec::new();
        let mut unstage_new: Vec<String> = Vec::new();
        let mut delete: Vec<String> = Vec::new();
        let mut submodules: Vec<String> = Vec::new();
        for f in files {
            if f.submodule {
                restore.push(f.path.clone());
                submodules.push(f.path.clone());
                continue;
            }
            if f.untracked {
                delete.push(f.path.clone());
            } else if f.x == 'A' || f.x == 'R' || f.x == 'C' || (f.x == '.' && f.y == 'A') {
                // Staged as new (or `git add -N`): not in HEAD, so reverting means removing it.
                unstage_new.push(f.path.clone());
                delete.push(f.path.clone());
                if let Some(o) = &f.orig_path {
                    restore.push(o.clone());
                }
            } else {
                restore.push(f.path.clone());
                if let Some(o) = &f.orig_path {
                    restore.push(o.clone());
                }
            }
        }
        if !unstage_new.is_empty() {
            self.git()
                .args(["rm", "--cached", "-q", "-f", "--ignore-unmatch", "--pathspec-from-file=-", "--pathspec-file-nul"])
                .stdin_paths(&unstage_new)
                .run()?;
        }
        if !restore.is_empty() {
            restore.sort();
            restore.dedup();
            self.git()
                .args(["restore", "--source=HEAD", "--staged", "--worktree", "--pathspec-from-file=-", "--pathspec-file-nul"])
                .stdin_paths(&restore)
                .run()?;
        }
        for p in &delete {
            let abs = self.deletion_path(p)?;
            if abs.is_file() || abs.is_symlink() {
                std::fs::remove_file(&abs)
                    .map_err(|e| user_error("revert", &format!("Could not delete {p}: {e} (is it open in another program?)")))?;
                remove_empty_parents(self.root(), &abs);
            }
        }
        if !submodules.is_empty() {
            self.git().args(["submodule", "update", "--init", "--"]).args(&submodules).run()?;
        }
        Ok(())
    }

    /// Get Latest: fetch, then fast-forward / rebase / merge onto the upstream.
    /// `dirty` are paths the caller knows to have uncommitted local changes; the working copy is checked again here,
    /// so a stale or empty list can never let local edits be overwritten.
    pub fn sync(&self, mode: PullMode, dirty: &[String]) -> GitResult<SyncOutcome> {
        self.fetch()?;
        let status = self.status()?;
        if status.branch.head.is_none() {
            return Err(user_error("sync", "You are not on a branch (detached HEAD). Switch to a branch first."));
        }
        if status.branch.upstream.is_none() {
            return Ok(SyncOutcome::NoUpstream);
        }
        if status.has_conflicts() || status.op != RepoOp::None {
            return Err(user_error("sync", "Finish resolving the current merge/rebase before getting latest."));
        }
        let behind = status.branch.behind;
        if behind == 0 {
            self.update_submodules_if_any()?;
            return Ok(SyncOutcome::UpToDate);
        }
        let incoming: HashSet<String> = self.incoming_files()?.into_iter().collect();
        let local = status.files.iter().flat_map(|f| std::iter::once(&f.path).chain(f.orig_path.as_ref()));
        let mut overlap: Vec<String> = dirty.iter().chain(local).filter(|p| incoming.contains(*p)).cloned().collect();
        overlap.sort();
        overlap.dedup();
        if !overlap.is_empty() {
            return Ok(SyncOutcome::Blocked { overlap });
        }
        let res = if status.branch.ahead == 0 {
            self.git().args(["merge", "--ff-only", "@{upstream}"]).run().map(|_| ())
        } else {
            match mode {
                PullMode::Rebase => self.git().args(["rebase", "--autostash", "@{upstream}"]).run().map(|_| ()),
                PullMode::Merge => self.git().args(["merge", "--no-edit", "--autostash", "@{upstream}"]).run().map(|_| ()),
            }
        };
        if let Err(e) = res {
            let after = self.status()?;
            if after.has_conflicts() || after.op != RepoOp::None {
                return Ok(SyncOutcome::Conflicts);
            }
            return Err(e);
        }
        self.update_submodules_if_any()?;
        Ok(SyncOutcome::Updated { commits: behind })
    }

    /// Resolves a conflicted file by taking one complete side.
    pub fn resolve_take(&self, f: &FileStatus, side: Side, op: RepoOp) -> GitResult<()> {
        let status = self.status()?;
        let f = status
            .find(&f.path)
            .filter(|f| f.kind == super::status::ChangeKind::Conflict)
            .ok_or_else(|| user_error("resolve", "This file is no longer conflicted. Refresh the project before resolving."))?;
        if status.op != op {
            return Err(user_error("resolve", "The operation changed since this conflict was displayed. Refresh the project."));
        }
        self.create_shelf(std::slice::from_ref(&f.path), &format!("{}before resolving {}", super::shelf::BACKUP_TAG, f.path))?;
        // During a rebase git's "ours" is the upstream and "theirs" is my commit.
        let use_ours = match (op, side) {
            (RepoOp::Rebase, Side::Mine) => false,
            (RepoOp::Rebase, Side::Theirs) => true,
            (_, Side::Mine) => true,
            (_, Side::Theirs) => false,
        };
        let exists = if use_ours { f.conflict_ours_exists() } else { f.conflict_theirs_exists() };
        if exists {
            self.git().args(["checkout", if use_ours { "--ours" } else { "--theirs" }, "--"]).arg(&f.path).run()?;
            self.git().args(["add", "--"]).arg(&f.path).run()?;
        } else {
            self.git().args(["rm", "-q", "-f", "--"]).arg(&f.path).run()?;
        }
        Ok(())
    }

    /// Marks a conflicted file as resolved with its current on-disk content.
    pub fn mark_resolved(&self, path: &str) -> GitResult<()> {
        if self.abs(path).exists() {
            self.git().args(["add", "--"]).arg(path).run()?;
        } else {
            self.git().args(["rm", "-q", "--cached", "--ignore-unmatch", "--"]).arg(path).run()?;
        }
        Ok(())
    }

    /// Launches the user's configured merge tool for one file (blocks until closed).
    pub fn run_mergetool(&self, path: &str) -> GitResult<()> {
        self.git().args(["mergetool", "--no-prompt", "--"]).arg(path).run()?;
        Ok(())
    }

    /// Continues the in-progress operation after all conflicts are resolved.
    pub fn continue_op(&self, op: RepoOp) -> GitResult<()> {
        let res = match op {
            RepoOp::Merge => self.git().args(["commit", "--no-edit"]).run(),
            RepoOp::Rebase => self.git().args(["rebase", "--continue"]).run(),
            RepoOp::CherryPick => self.git().args(["cherry-pick", "--continue"]).run(),
            RepoOp::Revert => self.git().args(["revert", "--continue"]).run(),
            RepoOp::None => return Ok(()),
        };
        match res {
            Ok(_) => Ok(()),
            Err(e) if op == RepoOp::Rebase && (e.to_string().contains("nothing to commit") || e.to_string().contains("--allow-empty")) => {
                // My commit became empty after resolution: drop it and carry on.
                self.git().args(["rebase", "--skip"]).run()?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn abort_op(&self, op: RepoOp) -> GitResult<()> {
        let status = self.status()?;
        if status.op != op {
            return Err(user_error("abort", "The operation changed. Refresh the project before aborting."));
        }
        let paths: Vec<String> =
            status.files.iter().filter(|f| !f.submodule).flat_map(|f| std::iter::once(f.path.clone()).chain(f.orig_path.clone())).collect();
        if op != RepoOp::None && !paths.is_empty() {
            self.create_shelf(&paths, &format!("{}before aborting {}", super::shelf::BACKUP_TAG, op.label()))?;
        }
        match op {
            RepoOp::Merge => self.git().args(["merge", "--abort"]).run()?,
            RepoOp::Rebase => self.git().args(["rebase", "--abort"]).run()?,
            RepoOp::CherryPick => self.git().args(["cherry-pick", "--abort"]).run()?,
            RepoOp::Revert => self.git().args(["revert", "--abort"]).run()?,
            RepoOp::None => return Ok(()),
        };
        Ok(())
    }

    /// Creates a new commit that undoes `id` (safe for already-pushed history).
    pub fn revert_commit(&self, id: &str, is_merge: bool) -> GitResult<()> {
        let mut c = self.git().args(["revert", "--no-edit"]);
        if is_merge {
            c = c.args(["-m", "1"]);
        }
        c.arg(id).run()?;
        Ok(())
    }

    /// Undoes the last local (unpushed) commit, keeping its changes in the working copy.
    pub fn undo_last_commit(&self) -> GitResult<()> {
        self.git().args(["reset", "--soft", "HEAD~1"]).run()?;
        Ok(())
    }

    /// Writes `path` as it was in `rev` into the working copy ("Get Revision").
    pub fn restore_file_from(&self, rev: &str, path: &str) -> GitResult<()> {
        self.git().args(["restore", "--worktree"]).arg(format!("--source={rev}")).arg("--").arg(path).run()?;
        Ok(())
    }

    pub fn cherry_pick(&self, id: &str) -> GitResult<()> {
        self.git().args(["cherry-pick"]).arg(id).run()?;
        Ok(())
    }

    /// Sizes (bytes) of working files.
    pub fn file_size(&self, path: &str) -> Option<u64> {
        std::fs::metadata(self.abs(path)).ok().map(|m| m.len())
    }

    /// Removes a leftover `.git/index.lock` from a crashed git process.
    pub fn remove_index_lock(&self) -> std::io::Result<()> {
        std::fs::remove_file(self.git_dir().join("index.lock"))
    }

    pub fn index_lock_age_secs(&self) -> Option<u64> {
        let m = std::fs::metadata(self.git_dir().join("index.lock")).ok()?;
        m.modified().ok()?.elapsed().ok().map(|d| d.as_secs())
    }
}

fn remove_empty_parents(root: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(d).is_err() {
            break; // not empty
        }
        dir = d.parent();
    }
}

pub fn user_error(cmd: &str, msg: &str) -> GitError {
    GitError { command: cmd.into(), code: None, stderr: msg.into(), stdout: String::new() }
}

/// Clones `url` into `dest` (including LFS content), then the submodules the project says everyone needs — not
/// every submodule: a private one in `.g4v/submodules.toml`'s `optional` list would stop the clone for anyone
/// without access to it.
pub fn clone(url: &str, dest: &Path, log: &CmdLog) -> GitResult<()> {
    run_global(&[OsStr::new("clone"), OsStr::new("--"), OsStr::new(url), dest.as_os_str()], None, log)?;
    let repo = super::Repo::open(dest, log.clone()).map_err(|e| user_error("clone", &e.to_string()))?;
    repo.update_submodules_if_any()?;
    Ok(())
}

/// Creates a new repository in `dir` with `main` as the default branch.
pub fn init(dir: &Path, log: &CmdLog) -> GitResult<()> {
    std::fs::create_dir_all(dir).map_err(|e| user_error("init", &e.to_string()))?;
    run_global(&[OsStr::new("init"), OsStr::new("-b"), OsStr::new("main")], Some(dir), log)?;
    Ok(())
}

pub fn global_identity(log: &CmdLog) -> Identity {
    let get = |k: &str| {
        run_global(&[OsStr::new("config"), OsStr::new("--global"), OsStr::new("--get"), OsStr::new(k)], None, log)
            .ok()
            .filter(|s| !s.is_empty())
    };
    Identity { name: get("user.name"), email: get("user.email") }
}

/// Opens a file or folder with the operating system's default handler.
pub fn open_path(path: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Shows a file in the system file browser.
pub fn reveal_path(path: &Path) {
    #[cfg(windows)]
    {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path.as_os_str());
        let _ = std::process::Command::new("explorer").arg(arg).spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(p) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(p).spawn();
        }
    }
}
