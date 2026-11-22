//! Get Latest with binaries, put together.
//!
//! 1. Finish (roll back) any transaction a crash left behind.
//! 2. Fetch, including the notes ref. Decide with [`plan::decide`].
//! 3. Download and verify the build first: a failure here leaves the workspace untouched.
//! 4. Log the rollback point (`.git/g4v/binaries-txn.json`), move the checkout, update submodules.
//! 5. Run `post_sync` if what it depends on changed. 6. Swap the files in. 7. Mark the log complete.
//!
//! Any failure in 4–6 puts the branch, the submodules, the files and the marker back together. The one state that
//! can't always be undone offline is the post-sync command (engine dependencies, typically): if it fails both
//! forwards and back, the log stays in `DepsIncomplete` and Launch is refused until a retry succeeds.
//!
//! The post-sync command's success is recorded against what it depends on (`.git/g4v/binaries-setup.json`: the
//! object IDs of `when_changed` at the commit it ran for). It runs whenever that record doesn't match `HEAD` — on a
//! first install too, where nothing "changed" — and Launch is refused until it has.
//!
//! Everything that changes the workspace holds an exclusive lock (`.git/g4v/binaries.lock`) from recovery to
//! cleanup, so a second g4v window can't roll back an install the first one is still making.

use super::config::BinariesConfig;
use super::install::{self, Manifest, PrepareError, Report, Staged};
use super::notes::{self, NoteState};
use super::paths::resolve_inside;
use super::plan::{self, Candidate, Facts, Movement, Plan};
use super::source::AssetSource;
use crate::git::ops::{PullMode, SyncOutcome};
use crate::git::{CmdLogEntry, Repo};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// How far back the first-parent history is searched for a ready build.
const CHAIN_LIMIT: usize = 500;
/// Ready builds tried, newest first, if releases have been pruned.
const MAX_CANDIDATES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BinSyncOutcome {
    /// Binaries weren't involved; the normal Get Latest ran.
    Normal {
        reason: String,
        sync: SyncOutcome,
    },
    UpToDate {
        waiting: usize,
    },
    /// Moved by `moved` commits (negative: back to the newest build) and/or installed build `key`.
    Synced {
        moved: i64,
        installed: Option<String>,
        waiting: usize,
    },
    /// No commit on the branch has a downloadable build.
    NoBuild {
        building: usize,
    },
    /// Local edits to files that differ at the target; nothing was changed.
    Blocked {
        overlap: Vec<String>,
    },
}

/// What the toolbar and banners show, worked out locally after a fetch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BinStatus {
    /// CI has published build information for this repository.
    pub index: bool,
    /// The newest commit with a ready build.
    pub target: Option<String>,
    /// Newer commits than the target that don't have a build yet.
    pub waiting: usize,
    /// What those newer commits are doing (the newest one's state decides).
    pub newer: Option<plan::Newer>,
    pub building: usize,
    /// Key of the complete installed build.
    pub installed: Option<String>,
    /// HEAD is the target and its build is installed.
    pub up_to_date: bool,
    /// An unfinished or failed transaction, described for the user.
    pub incomplete: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Phase {
    Moving,
    Deps,
    Swapping,
    /// Rolled back, except the post-sync command could not be re-run. Launch is refused until it is.
    DepsIncomplete,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SubState {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Txn {
    pub phase: Phase,
    pub target: String,
    pub key: Option<String>,
    pub old_head: String,
    pub submodules: Vec<SubState>,
    pub deps_ran: bool,
}

fn work_dir(repo: &Repo) -> PathBuf {
    repo.meta_dir().join("binaries")
}
fn txn_path(repo: &Repo) -> PathBuf {
    repo.meta_dir().join("binaries-txn.json")
}
fn staging_dir(repo: &Repo) -> PathBuf {
    work_dir(repo).join("staging")
}
fn backup_dir(repo: &Repo) -> PathBuf {
    work_dir(repo).join("backup")
}
fn new_manifest_path(repo: &Repo) -> PathBuf {
    work_dir(repo).join("new-manifest.json")
}

fn check_launcher_transaction(repo: &Repo) -> Result<(), String> {
    let message = "Launch.bat has an interrupted editor installation. Run Launch.bat to finish recovery, then retry.";
    if repo.meta_dir().join("launcher-install").try_exists().map_err(|e| e.to_string())? {
        return Err(message.into());
    }
    // Older launchers stored their backups in the workspace. Never modify those from G4V.
    for entry in std::fs::read_dir(repo.root()).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if name
            .to_str()
            .and_then(|n| n.strip_prefix(".binwork-"))
            .is_some_and(|key| key.len() == 16 && key.bytes().all(|b| b.is_ascii_hexdigit()))
            && entry.path().join("backup").is_dir()
        {
            return Err(message.into());
        }
    }
    Ok(())
}

pub fn pending_txn(repo: &Repo) -> Option<Txn> {
    std::fs::read(txn_path(repo)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn save_txn(repo: &Repo, t: &Txn) -> Result<(), String> {
    let p = txn_path(repo);
    std::fs::create_dir_all(repo.meta_dir()).map_err(|e| e.to_string())?;
    let mut tmp = tempfile::NamedTempFile::new_in(repo.meta_dir()).map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut tmp, &serde_json::to_vec_pretty(t).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    tmp.persist(&p).map_err(|e| e.to_string())?;
    Ok(())
}

fn clear_txn(repo: &Repo) {
    let _ = std::fs::remove_file(txn_path(repo));
    let _ = std::fs::remove_file(new_manifest_path(repo));
    let _ = std::fs::remove_dir_all(staging_dir(repo));
    let _ = std::fs::remove_dir_all(backup_dir(repo));
}

// ------------------------------------------------------------------ workspace lock

/// Held while a Get Latest, rollback or setup changes this workspace. Released when dropped (and by the OS if g4v
/// dies), so it can never be left behind.
pub struct WorkspaceLock {
    _file: std::fs::File,
}

fn lock_path(repo: &Repo) -> PathBuf {
    repo.meta_dir().join("binaries.lock")
}

const LOCKED: &str = "g4v is already updating this project (in another window or tab). Wait for that to finish, then try again.";

/// Takes the workspace lock, or says another g4v is holding it.
pub fn lock_workspace(repo: &Repo) -> Result<WorkspaceLock, String> {
    std::fs::create_dir_all(repo.meta_dir()).map_err(|e| e.to_string())?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(repo))
        .map_err(|e| format!("{}: {e}", lock_path(repo).display()))?;
    match file.try_lock() {
        Ok(()) => Ok(WorkspaceLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(LOCKED.into()),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("Could not lock the workspace: {e}")),
    }
}

/// Is another process (or another tab) updating this workspace right now?
pub fn locked_by_other(repo: &Repo) -> bool {
    matches!(lock_workspace(repo), Err(e) if e == LOCKED)
}

// ------------------------------------------------------------------ setup record

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct SetupRecord {
    inputs: String,
}

fn setup_path(repo: &Repo) -> PathBuf {
    repo.meta_dir().join("binaries-setup.json")
}

/// What the post-sync command depends on at `rev`: the command itself (program and arguments) and the paths it
/// watches, then the object IDs of those paths (for a submodule, its pinned commit), or the commit itself when it
/// watches none. A changed command, or a changed list of paths, is a different setup even at the same engine pin.
fn setup_inputs(repo: &Repo, cfg: &BinariesConfig, rev: &str) -> Option<String> {
    let ps = cfg.post_sync.as_ref()?;
    let commit = repo.run(&["rev-parse", &format!("{rev}^{{commit}}")]).ok()?;
    let ids = if ps.when_changed.is_empty() {
        commit
    } else {
        ps.when_changed
            .iter()
            .map(|p| repo.run(&["rev-parse", &format!("{commit}:{p}")]).unwrap_or_else(|_| "-".into()))
            .collect::<Vec<_>>()
            .join(",")
    };
    let command = serde_json::json!({ "run": ps.run, "when_changed": ps.when_changed });
    Some(format!("{command}|{ids}"))
}

fn recorded_setup(repo: &Repo) -> Option<String> {
    std::fs::read(setup_path(repo)).ok().and_then(|b| serde_json::from_slice::<SetupRecord>(&b).ok()).map(|r| r.inputs)
}

fn record_setup(repo: &Repo, cfg: &BinariesConfig, rev: &str) -> Result<(), String> {
    match setup_inputs(repo, cfg, rev) {
        Some(inputs) => install::write_atomic(&setup_path(repo), &serde_json::to_vec(&SetupRecord { inputs }).map_err(|e| e.to_string())?),
        None => Ok(()),
    }
}

fn forget_setup(repo: &Repo) {
    let _ = std::fs::remove_file(setup_path(repo));
}

/// Does `rev` need the post-sync command run (it never succeeded for what `rev` depends on)?
fn setup_needed(repo: &Repo, cfg: &BinariesConfig, rev: &str) -> bool {
    cfg.post_sync.is_some() && setup_inputs(repo, cfg, rev) != recorded_setup(repo)
}

/// Is the post-sync command done for the checked-out commit? (Always, when the project has none.)
pub fn setup_current(repo: &Repo, cfg: &BinariesConfig) -> bool {
    !setup_needed(repo, cfg, "HEAD")
}

/// Runs the post-sync command for `rev` and records its success. The record is dropped first: a run that fails
/// halfway may have left the old setup half-replaced, so nothing may trust it until a run succeeds.
fn run_setup_for(repo: &Repo, cfg: &BinariesConfig, rev: &str, report: Report) -> Result<(), String> {
    forget_setup(repo);
    run_post_sync(repo, cfg, report)?;
    record_setup(repo, cfg, rev)
}

/// "Run setup": the post-sync command for the checked-out commit, on its own.
pub fn run_setup(repo: &Repo, cfg: &BinariesConfig, report: Report) -> Result<(), String> {
    let _lock = lock_workspace(repo)?;
    check_launcher_transaction(repo)?;
    if pending_txn(repo).is_some() {
        return Err("The last Get Latest didn't finish; Get Latest first.".into());
    }
    run_setup_for(repo, cfg, "HEAD", report)
}

pub fn describe_incomplete(t: &Txn) -> String {
    match t.phase {
        Phase::DepsIncomplete => {
            "The last Get Latest was rolled back, but the project's post-sync step (engine dependencies) didn't finish. \
             Get Latest again to retry; the editor can't be launched until it succeeds."
                .into()
        }
        _ => "The last Get Latest didn't finish. Get Latest again: g4v puts the previous version back first.".into(),
    }
}

// ------------------------------------------------------------------ status

/// Local facts for the toolbar, after a fetch (no network).
pub fn status(repo: &Repo, cfg: &BinariesConfig) -> BinStatus {
    let mut s = BinStatus {
        index: notes::exists(repo, &cfg.notes_ref),
        installed: install::installed(repo.root(), cfg).map(|m| m.key),
        incomplete: pending_txn(repo).map(|t| describe_incomplete(&t)),
        ..Default::default()
    };
    if s.incomplete.is_some() {
        s.installed = None;
    }
    if !s.index {
        return s;
    }
    let Ok(chain) = notes::first_parent(repo, "@{upstream}", CHAIN_LIMIT, &cfg.notes_ref) else { return s };
    s.building = plan::building(&chain);
    if let Some((_, c)) = plan::ready(&chain).first() {
        s.waiting = plan::waiting_above(&chain, &c.commit);
        s.newer = plan::newer(&chain, &c.commit);
        let head = repo.run(&["rev-parse", "HEAD"]).unwrap_or_default();
        s.up_to_date = head == c.commit && s.installed.as_deref() == c.note.as_ref().map(|n| n.key.as_str());
        s.target = Some(c.commit.clone());
    }
    s
}

// ------------------------------------------------------------------ helpers

fn is_ancestor(repo: &Repo, a: &str, b: &str) -> bool {
    repo.git().args(["merge-base", "--is-ancestor", a, b]).ok_codes(&[0]).quiet().run().is_ok()
}

fn has_submodules(repo: &Repo) -> bool {
    repo.abs(".gitmodules").exists()
}

/// `git submodule status --recursive`: (flag, sha, path) per submodule.
fn submodule_status(repo: &Repo) -> Result<Vec<(char, String, String)>, String> {
    if !has_submodules(repo) {
        return Ok(Vec::new());
    }
    let out = repo.git().args(["submodule", "status", "--recursive"]).quiet().run().map_err(|e| e.to_string())?;
    Ok(out
        .text()
        .lines()
        .filter_map(|l| {
            let flag = l.chars().next()?;
            let rest = &l[flag.len_utf8()..];
            let mut it = rest.split_whitespace();
            let sha = it.next()?.to_string();
            let path = it.next()?.to_string();
            Some((flag, sha, path))
        })
        .collect())
}

fn git_in(repo: &Repo, dir: &Path, args: &[&str]) -> Result<String, String> {
    repo.git().arg("-C").arg(dir).args(args).quiet().run().map(|o| o.text().trim().to_string()).map_err(|e| e.to_string())
}

fn record_submodules(repo: &Repo) -> Result<Vec<SubState>, String> {
    let mut v = Vec::new();
    for (flag, sha, path) in submodule_status(repo)? {
        if flag == '-' {
            continue;
        }
        let dir = repo.abs(&path);
        let head = git_in(repo, &dir, &["rev-parse", "HEAD"]).unwrap_or(sha);
        let branch = git_in(repo, &dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok().filter(|b| !b.is_empty());
        v.push(SubState { path, head, branch });
    }
    Ok(v)
}

/// The shared routine, so optional (private) submodules are never initialized for someone without access.
fn update_submodules(repo: &Repo) -> Result<(), String> {
    repo.update_submodules_if_any().map_err(|e| e.to_string())
}

/// Puts submodules back on the branches they were on, where that branch's tip is what's now checked out.
fn reattach(repo: &Repo, subs: &[SubState]) {
    for s in subs {
        let Some(b) = &s.branch else { continue };
        let dir = repo.abs(&s.path);
        let (Ok(head), Ok(tip)) =
            (git_in(repo, &dir, &["rev-parse", "HEAD"]), git_in(repo, &dir, &["rev-parse", &format!("refs/heads/{b}")]))
        else {
            continue;
        };
        if head == tip {
            let _ = git_in(repo, &dir, &["switch", "--quiet", b]);
        }
    }
}

/// Why this is a programmer's workspace: edits under `source_paths` (in whichever repository holds them) or a
/// submodule whose checked-out commit isn't its pin. The superproject's own status can't see either: a clean
/// submodule that's three local commits ahead, or an edit inside one, look the same as no change at all.
fn programmer_reason(repo: &Repo, cfg: &BinariesConfig) -> Result<Option<String>, String> {
    for (flag, _, path) in submodule_status(repo)? {
        match flag {
            '+' => return Ok(Some(format!("{path} is not at the commit the project pins (you have local work there)"))),
            'U' => return Ok(Some(format!("{path} has a merge conflict"))),
            _ => {}
        }
    }
    for p in &cfg.source_paths {
        let abs = resolve_inside(repo.root(), p)?;
        if !abs.exists() {
            continue;
        }
        // Run from inside the path, so git answers for whichever repository (superproject or submodule) holds it.
        let dir = if abs.is_dir() { abs.clone() } else { abs.parent().map(Path::to_path_buf).unwrap_or_default() };
        let spec = abs.to_string_lossy().into_owned();
        let changed = git_in(repo, &dir, &["status", "--porcelain", "--untracked-files=normal", "--", &spec]).is_ok_and(|o| !o.is_empty());
        if changed {
            return Ok(Some(format!("you have local changes under {p}")));
        }
    }
    Ok(None)
}

/// Names from `wanted` that are running now (case-insensitive).
pub fn running(wanted: &[String]) -> Vec<String> {
    if wanted.is_empty() {
        return Vec::new();
    }
    let list = if cfg!(windows) {
        let mut c = Command::new("tasklist");
        c.args(["/FO", "CSV", "/NH"]);
        hide_window(&mut c);
        c.output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    } else {
        Command::new("ps").args(["-A", "-o", "comm="]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    };
    let names: Vec<String> = list
        .lines()
        .filter_map(|l| {
            let first = if cfg!(windows) { l.split("\",").next()?.trim_matches('"') } else { l.rsplit('/').next()? };
            Some(first.trim().to_lowercase())
        })
        .collect();
    wanted.iter().filter(|w| names.contains(&w.to_lowercase())).cloned().collect()
}

#[cfg(windows)]
fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}
#[cfg(not(windows))]
fn hide_window(_cmd: &mut Command) {}

/// Builds the command for a repository-relative program: `.ps1` through PowerShell on Windows, `.sh` through `sh`.
fn program(repo: &Repo, argv: &[String]) -> Result<Command, String> {
    let (exe, args) = argv.split_first().ok_or("no program given")?;
    let abs = resolve_inside(repo.root(), exe)?;
    if !abs.is_file() {
        return Err(format!("{} does not exist", abs.display()));
    }
    let ext = abs.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut c = if cfg!(windows) && ext == "ps1" {
        let mut c = Command::new("powershell.exe");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&abs);
        c
    } else if !cfg!(windows) && ext == "sh" {
        let mut c = Command::new("sh");
        c.arg(&abs);
        c
    } else {
        Command::new(&abs)
    };
    for a in args {
        // An argument naming an existing workspace path becomes absolute; anything else is passed as written.
        match resolve_inside(repo.root(), a) {
            Ok(p) if super::paths::is_safe_rel(a) && p.exists() => c.arg(p),
            _ => c.arg(a),
        };
    }
    c.current_dir(repo.root());
    Ok(c)
}

fn run_post_sync(repo: &Repo, cfg: &BinariesConfig, report: Report) -> Result<(), String> {
    let Some(ps) = &cfg.post_sync else { return Ok(()) };
    report(format!("Running {}…", ps.run.join(" ")));
    let started = std::time::Instant::now();
    let mut c = program(repo, &ps.run)?;
    c.stdin(std::process::Stdio::null());
    hide_window(&mut c);
    let out = c.output().map_err(|e| format!("Could not start {}: {e}", ps.run[0]))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    repo.cmd_log().push(CmdLogEntry {
        time: chrono::Local::now(),
        repo: repo.name(),
        command: ps.run.join(" "),
        ok: out.status.success(),
        millis: started.elapsed().as_millis(),
        output: text.chars().rev().take(8000).collect::<Vec<_>>().into_iter().rev().collect(),
    });
    if !out.status.success() {
        let tail: Vec<&str> = text.lines().rev().take(12).collect();
        return Err(format!(
            "{} failed (exit code {:?}):\n{}",
            ps.run.join(" "),
            out.status.code(),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ rollback

/// Undoes an unfinished transaction: files, then branch and submodules, then (if it ran) the post-sync step.
pub fn rollback(repo: &Repo, cfg: &BinariesConfig, txn: &Txn, report: Report) -> Result<(), String> {
    report("Putting the previous version back…".into());
    if txn.phase != Phase::DepsIncomplete {
        let new_manifest = std::fs::read(new_manifest_path(repo)).ok().and_then(|b| Manifest::parse(&b).ok());
        install::restore(repo.root(), cfg, new_manifest.as_ref(), &staging_dir(repo), &backup_dir(repo))?;
        let head = repo.run(&["rev-parse", "HEAD"]).map_err(|e| e.to_string())?;
        if head != txn.old_head {
            repo.git()
                .args(["reset", "--keep", &txn.old_head])
                .run()
                .map_err(|e| format!("Could not move the branch back to {}: {e}", &txn.old_head[..txn.old_head.len().min(10)]))?;
        }
        update_submodules(repo)?;
        reattach(repo, &txn.submodules);
    }
    if txn.deps_ran || txn.phase == Phase::DepsIncomplete || setup_needed(repo, cfg, "HEAD") {
        if let Err(e) = run_setup_for(repo, cfg, "HEAD", report) {
            let mut t = txn.clone();
            t.phase = Phase::DepsIncomplete;
            save_txn(repo, &t)?;
            return Err(format!("The previous version is back, but its post-sync step failed:\n{e}"));
        }
    }
    clear_txn(repo);
    Ok(())
}

/// Rolls back anything a crash or failure left behind. `Ok(true)` if there was something to undo.
pub fn recover(repo: &Repo, cfg: &BinariesConfig, report: Report) -> Result<bool, String> {
    let _lock = lock_workspace(repo)?;
    recover_locked(repo, cfg, report)
}

fn recover_locked(repo: &Repo, cfg: &BinariesConfig, report: Report) -> Result<bool, String> {
    check_launcher_transaction(repo)?;
    match pending_txn(repo) {
        Some(t) => rollback(repo, cfg, &t, report).map(|_| true),
        None => Ok(false),
    }
}

// ------------------------------------------------------------------ get latest

pub fn get_latest(
    repo: &Repo,
    cfg: &BinariesConfig,
    src: &dyn AssetSource,
    cache: &Path,
    mode: PullMode,
    dirty: &[String],
    report: Report,
) -> Result<BinSyncOutcome, String> {
    // Held to the end: no other g4v may recover, stage or swap in this workspace meanwhile.
    let _lock = lock_workspace(repo)?;
    recover_locked(repo, cfg, report)?;

    report("Checking the server…".into());
    let remote = repo.fetch().map_err(|e| e.to_string())?;
    if let Some(r) = &remote {
        notes::fetch(repo, r, &cfg.notes_ref).map_err(|e| format!("Could not read build information from the server: {e}"))?;
    }
    let st = repo.status().map_err(|e| e.to_string())?;
    if st.branch.head.is_none() {
        return Err("You are not on a branch (detached HEAD). Switch to a branch first.".into());
    }
    if st.has_conflicts() || st.op != crate::git::status::RepoOp::None {
        return Err("Finish resolving the current merge/rebase before getting latest.".into());
    }
    let normal = |reason: String| -> Result<BinSyncOutcome, String> {
        let sync = repo.sync(mode, dirty).map_err(|e| e.to_string())?;
        Ok(BinSyncOutcome::Normal { reason, sync })
    };
    if st.branch.upstream.is_none() {
        return normal("this branch isn't on the server".into());
    }
    let head = st.branch.oid.clone().unwrap_or_default();
    let have_index = notes::exists(repo, &cfg.notes_ref);
    let chain = if have_index {
        notes::first_parent(repo, "@{upstream}", CHAIN_LIMIT, &cfg.notes_ref).map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };
    let ready = plan::candidates(&chain, MAX_CANDIDATES);
    let facts = Facts {
        head: head.clone(),
        ahead: st.branch.ahead,
        programmer: programmer_reason(repo, cfg)?,
        have_index,
        installed_key: install::installed(repo.root(), cfg).map(|m| m.key),
        head_is_ancestor_of: ready.iter().map(|(_, c)| is_ancestor(repo, &head, &c.commit)).collect(),
        is_ancestor_of_head: ready.iter().map(|(_, c)| is_ancestor(repo, &c.commit, &head)).collect(),
        chain: chain.clone(),
    };
    let candidates = match plan::decide(&facts) {
        Plan::Normal(reason) => return normal(reason),
        Plan::UpToDate => {
            // Up to date can still mean setup never ran here (a fresh clone at the ready commit) or a run failed.
            if setup_needed(repo, cfg, "HEAD") {
                run_setup_for(repo, cfg, "HEAD", report)?;
            }
            return Ok(BinSyncOutcome::UpToDate { waiting: plan::waiting_above(&chain, &head) });
        }
        Plan::NoBuild => return Ok(BinSyncOutcome::NoBuild { building: plan::building(&chain) }),
        Plan::Go(c) => c,
    };

    // Download and verify before touching anything. A pruned or unfinished release falls back to an older build —
    // but only once it's clear the user can see the releases at all, or a missing sign-in looks like a missing build.
    if candidates.iter().any(|c| facts.installed_key.as_deref() != Some(c.note.key.as_str())) {
        src.check_access()?;
    }
    let mut chosen: Option<(Candidate, Option<Staged>)> = None;
    for cand in candidates {
        if facts.installed_key.as_deref() == Some(cand.note.key.as_str()) {
            chosen = Some((cand, None));
            break;
        }
        match install::prepare(cfg, &cand.note, src, cache, &staging_dir(repo), report) {
            Ok(staged) => {
                chosen = Some((cand, Some(staged)));
                break;
            }
            Err(PrepareError::Missing) => continue,
            Err(PrepareError::Failed(e)) => {
                let _ = std::fs::remove_dir_all(staging_dir(repo));
                return Err(e);
            }
        }
    }
    let Some((cand, staged)) = chosen else { return Ok(BinSyncOutcome::NoBuild { building: plan::building(&chain) }) };

    let busy = running(&cfg.block_if_running);
    if !busy.is_empty() && staged.is_some() {
        let _ = std::fs::remove_dir_all(staging_dir(repo));
        return Err(format!("Close {} first, then Get Latest again.", busy.join(" and ")));
    }
    if cand.movement != Movement::Stay {
        let diff =
            repo.git().args(["diff", "--name-only", "-z", "--no-renames", &head, &cand.commit]).quiet().run().map_err(|e| e.to_string())?;
        let changed: std::collections::HashSet<String> = crate::git::split_nul(&diff.stdout).into_iter().collect();
        let mut overlap: Vec<String> = dirty.iter().filter(|p| changed.contains(*p)).cloned().collect();
        if !overlap.is_empty() {
            overlap.sort();
            overlap.dedup();
            let _ = std::fs::remove_dir_all(staging_dir(repo));
            return Ok(BinSyncOutcome::Blocked { overlap });
        }
    }

    let mut txn = Txn {
        phase: Phase::Moving,
        target: cand.commit.clone(),
        key: staged.as_ref().map(|s| s.manifest.key.clone()),
        old_head: head.clone(),
        submodules: record_submodules(repo)?,
        deps_ran: false,
    };
    save_txn(repo, &txn)?;
    let result = (|| -> Result<(), String> {
        match cand.movement {
            Movement::Stay => {}
            Movement::Forward => {
                report("Updating files…".into());
                repo.git().args(["merge", "--ff-only", &cand.commit]).run().map_err(|e| e.to_string())?;
            }
            Movement::Back => {
                report("Going back to the newest change that has a build…".into());
                repo.git().args(["reset", "--keep", &cand.commit]).run().map_err(|e| e.to_string())?;
            }
        }
        if cand.movement != Movement::Stay {
            update_submodules(repo)?;
            reattach(repo, &txn.submodules);
        }
        if setup_needed(repo, cfg, &cand.commit) {
            txn.phase = Phase::Deps;
            txn.deps_ran = true;
            save_txn(repo, &txn)?;
            run_setup_for(repo, cfg, &cand.commit, report)?;
        }
        if let Some(staged) = &staged {
            std::fs::write(new_manifest_path(repo), &staged.manifest_bytes).map_err(|e| e.to_string())?;
            txn.phase = Phase::Swapping;
            save_txn(repo, &txn)?;
            install::swap(repo.root(), cfg, staged, &backup_dir(repo), report)?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        return match rollback(repo, cfg, &txn, report) {
            Ok(()) => Err(format!("{e}\n\nNothing changed: the previous version is back.")),
            Err(r) => Err(format!("{e}\n\nPutting the previous version back also failed:\n{r}")),
        };
    }
    clear_txn(repo);
    install::prune_cache(cache, 3);

    let count = |a: &str, b: &str| -> i64 {
        repo.run(&["rev-list", "--count", &format!("{a}..{b}")]).ok().and_then(|s| s.parse().ok()).unwrap_or(0)
    };
    let moved = match cand.movement {
        Movement::Stay => 0,
        Movement::Forward => count(&head, &cand.commit),
        Movement::Back => -count(&cand.commit, &head),
    };
    Ok(BinSyncOutcome::Synced { moved, installed: staged.map(|s| s.manifest.key), waiting: plan::waiting_above(&chain, &cand.commit) })
}

// ------------------------------------------------------------------ launch

/// What fixes a [`LaunchProblem`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchFix {
    /// Get Latest (re)installs the build for this version ("Repair editor" when the install is damaged).
    GetLatest,
    Repair,
    /// Run the project's setup step for this version.
    RunSetup,
    /// Wait, or nothing g4v can do.
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchProblem {
    pub message: String,
    pub fix: LaunchFix,
}

fn problem(message: impl Into<String>, fix: LaunchFix) -> Result<(), LaunchProblem> {
    Err(LaunchProblem { message: message.into(), fix })
}

/// Everything Launch needs to be true, checked off the UI thread (it reads every installed file's size):
/// no unfinished Get Latest, the program present, and — for an editor g4v installed — every file of the build
/// whole, the build the one this version of the project needs, and the setup step done for this version. A
/// programmer's own build (local source edits, or no g4v install) is theirs: only its presence is checked.
pub fn launch_check(repo: &Repo, cfg: &BinariesConfig) -> Result<(), LaunchProblem> {
    if locked_by_other(repo) {
        return problem(LOCKED, LaunchFix::None);
    }
    check_launch_files(repo, cfg)
}

fn check_launch_files(repo: &Repo, cfg: &BinariesConfig) -> Result<(), LaunchProblem> {
    if let Err(e) = check_launcher_transaction(repo) {
        return problem(e, LaunchFix::None);
    }
    if cfg.launch.is_empty() {
        return problem("this project doesn't say what to launch", LaunchFix::None);
    }
    // Read once: a Get Latest finishing between two reads would otherwise make the second one come back empty.
    if let Some(txn) = pending_txn(repo) {
        return problem(describe_incomplete(&txn), LaunchFix::GetLatest);
    }
    match resolve_inside(repo.root(), &cfg.launch[0]) {
        Ok(p) if p.is_file() => {}
        Ok(_) => return problem("the editor isn't installed yet — Get Latest installs it", LaunchFix::GetLatest),
        Err(e) => return problem(e, LaunchFix::None),
    }
    // Managed: g4v (or Launch.bat) installed this editor, which either record shows. Losing one of the two doesn't
    // turn a damaged install into "someone's own build" that skips every check below.
    let record = |rel: &str| resolve_inside(repo.root(), rel).is_ok_and(|p| p.is_file());
    let managed = record(&cfg.installed_manifest()) || record(&cfg.marker);
    if !managed || programmer_reason(repo, cfg).ok().flatten().is_some() {
        return Ok(());
    }
    let Some(installed) = install::installed(repo.root(), cfg) else {
        return problem("the installed editor is incomplete or damaged (files are missing or changed)", LaunchFix::Repair);
    };
    if let Some(note) = notes::note_for(repo, &cfg.notes_ref, "HEAD") {
        if note.key != installed.key {
            let why = match note.state {
                NoteState::Ready => "the installed editor was built for a different version of the project",
                _ => "this version of the project has no editor build yet",
            };
            return problem(format!("{why} — Get Latest fixes it"), LaunchFix::GetLatest);
        }
    }
    if !setup_current(repo, cfg) {
        return problem("this version's setup step (engine dependencies) hasn't finished", LaunchFix::RunSetup);
    }
    Ok(())
}

/// Why Launch is unavailable right now, if it is.
pub fn launch_blocker(repo: &Repo, cfg: &BinariesConfig) -> Option<String> {
    launch_check(repo, cfg).err().map(|p| p.message)
}

pub fn launch(repo: &Repo, cfg: &BinariesConfig) -> Result<(), String> {
    let _lock = lock_workspace(repo)?;
    check_launch_files(repo, cfg).map_err(|p| format!("Can't launch: {}", p.message))?;
    let mut c = program(repo, &cfg.launch)?;
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    c.spawn().map(|_| ()).map_err(|e| format!("Could not start {}: {e}", cfg.launch[0]))
}
