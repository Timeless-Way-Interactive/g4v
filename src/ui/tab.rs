//! State and behaviour of one open repository tab.

use super::tree::{ancestor_dirs, FileTree, TreeFilter};
use crate::changelists::{ChangelistStore, DEFAULT_ID};
use crate::git::branches::{BranchRef, Stash, Submodule};
use crate::git::diff::{is_image_path, FileDiff};
use crate::git::history::{build_graph, ChangedFile, Commit, CommitDetail, GraphRow, HistoryScope};
use crate::git::lfs::Lock;
use crate::git::ops::{PullMode, PushOutcome, Side, SyncOutcome};
use crate::git::shelf::{Shelf, BACKUP_TAG};
use crate::git::status::{ChangeKind, FileStatus, RepoOp, StatusSnapshot};
use crate::git::{GitError, Repo};
use crate::settings::Settings;
use crate::worker::Worker;
use eframe::egui;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

fn es(e: GitError) -> String {
    e.to_string()
}

// ---------------------------------------------------------------- messages

pub struct TreeData {
    pub tracked: Vec<String>,
    pub lockable: HashSet<String>,
    pub lfs: HashSet<String>,
    pub uses_lfs: bool,
    pub lfs_installed: bool,
}

pub struct FetchData {
    pub remote: Option<String>,
    pub incoming: Vec<(char, String)>,
    /// Build state for repositories that use precompiled binaries.
    pub binaries: Option<crate::binaries::BinStatus>,
}

pub struct RefsData {
    pub identity: crate::git::ops::Identity,
    pub default_remote: Option<String>,
    pub branches: Vec<BranchRef>,
    pub remotes: Vec<(String, String)>,
    pub submodules: Vec<Submodule>,
    pub stashes: Vec<Stash>,
}

pub struct DiffData {
    pub title: String,
    pub path: String,
    pub diff: FileDiff,
    pub old_image: Option<egui::ColorImage>,
    pub new_image: Option<egui::ColorImage>,
    pub old_size: Option<u64>,
    pub new_size: Option<u64>,
}

/// Sortable columns of the History table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryColumn {
    Cl,
    Description,
    Author,
    Date,
}

impl HistoryColumn {
    pub fn title(self) -> &'static str {
        match self {
            HistoryColumn::Cl => "CL #",
            HistoryColumn::Description => "Description",
            HistoryColumn::Author => "Author",
            HistoryColumn::Date => "Date",
        }
    }
}

pub enum OpOk {
    Message(String),
    Sync(SyncOutcome),
    Submitted {
        commit: String,
        /// Changelist number of what landed on the branch (HEAD after any rebase during upload); 0 if unknown.
        cl: u64,
        cl_id: u32,
        push: Option<Result<String, String>>,
        unlocked: Vec<String>,
        unlock_failed: Vec<(String, String)>,
    },
    Shelved {
        shelf: Shelf,
        reverted: bool,
        share: Option<Result<(), String>>,
        cl_id: Option<u32>,
    },
    Unshelved {
        paths: Vec<String>,
        description: String,
        backup: Option<String>,
        /// Put the files in a new changelist named after the shelf (false = default changelist).
        new_changelist: bool,
    },
    Locks {
        verb: &'static str,
        ok: Vec<String>,
        failed: Vec<(String, String)>,
    },
    BlockedResolved {
        shelf: Shelf,
        sync: SyncOutcome,
    },
    Binaries(crate::binaries::BinSyncOutcome),
}

pub enum Msg {
    /// The snapshot and how long git took to produce it (paces the background polling).
    Status(Result<StatusSnapshot, String>, Duration),
    Tree(Result<TreeData, String>),
    Fetched(Result<FetchData, String>),
    /// Local-only recomputation of incoming files (errors are not network errors).
    Incoming(Result<Vec<(char, String)>, String>),
    Locks(Result<Vec<Lock>, String>),
    History(u64, Result<Vec<Commit>, String>),
    CommitDetail(Result<CommitDetail, String>),
    Diff(u64, Result<DiffData, String>),
    Shelves(Result<Vec<Shelf>, String>),
    ShelfFiles(String, Result<Vec<ChangedFile>, String>),
    Refs(Result<RefsData, String>),
    Done {
        title: String,
        result: Result<OpOk, String>,
        net: bool,
    },
}

// ---------------------------------------------------------------- UI enums

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CenterTab {
    Pending,
    History,
    Shelves,
    Locks,
    Log,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeftTab {
    Workspace,
    Branches,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffTarget {
    Working { path: String },
    Commit { id: String, parent: Option<String>, file: ChangedFile },
    Shelf { id: String, base: Option<String>, file: ChangedFile },
    Incoming { path: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchTarget {
    Local(String),
    Remote(String),
}

#[derive(Clone, Debug)]
pub enum Action {
    Refresh,
    Fetch,
    GetLatest,
    /// Answer to the binaries trust prompt.
    TrustBinaries(bool),
    LaunchEditor,
    ShelveOverlapAndSync(Vec<String>),
    Push,
    ShowDiff(DiffTarget),
    OpenSubmit(u32),
    OpenShelve(Vec<String>, Option<u32>),
    OpenRevert(Vec<String>),
    NewChangelist(Vec<String>),
    EditChangelist(u32),
    DeleteChangelist(u32),
    MoveFiles(Vec<String>, u32),
    Lock(Vec<String>),
    Unlock(Vec<String>, bool),
    OpenFile(String),
    RevealFile(String),
    FileHistory(String),
    RevealInTree(String),
    Resolve(String, Side),
    MarkResolved(String),
    MergeTool(String),
    ContinueOp,
    AbortOp,
    Switch(SwitchTarget),
    SwitchNow(SwitchTarget, bool),
    NewBranch(Option<String>),
    DeleteBranch(String, bool),
    DeleteRemoteBranch(String),
    MergeBranch(String),
    SelectCommit(String),
    RevertCommit(String, bool),
    CherryPick(String),
    UndoLastCommit,
    NewTag(String),
    DeleteTag(String),
    PushTag(String),
    GetRevision(String, String),
    SelectShelf(String),
    OpenUnshelve(Shelf, Vec<String>),
    ShareShelf(Shelf),
    UnshareShelf(Shelf),
    DeleteShelf(Shelf),
    StashApply(usize, bool),
    StashDrop(usize),
    OpenSubmodule(String),
    UpdateSubmodules,
    RemoveIndexLock,
    DoRemoveIndexLock,
    SetupIdentity,
    AddRemote,
    TrackLfs(String),
    Confirm {
        title: String,
        body: String,
        action: Box<Action>,
    },
    // Confirmed / dialog-produced actions:
    DoSubmit {
        paths: Vec<String>,
        message: String,
        cl_id: u32,
        push: bool,
        unlock: Vec<String>,
    },
    DoShelve {
        paths: Vec<String>,
        description: String,
        revert: bool,
        share: bool,
        cl_id: Option<u32>,
    },
    DoUnshelve {
        shelf: Shelf,
        paths: Vec<String>,
    },
    RestoreWip(Shelf),
    DoRevert(Vec<String>),
    DoDeleteShelf(Shelf),
    DoDeleteBranch(String, bool),
    DoDeleteRemoteBranch(String),
    DoRevertCommit(String, bool),
    DoUndoLastCommit,
    DoAbortOp,
    DoMerge(String),
    DoGetRevision(String, String),
    DoForceUnlock(Vec<String>),
}

// ---------------------------------------------------------------- dialogs

pub struct SubmitDlg {
    pub cl_id: u32,
    pub message: String,
    pub files: Vec<(String, bool)>,
    pub push: bool,
    pub unlock: bool,
    /// Sizes of the files over the "large file" limit, measured once when the dialog opens.
    pub large: HashMap<String, u64>,
}

pub struct ShelveDlg {
    pub cl_id: Option<u32>,
    pub description: String,
    pub files: Vec<(String, bool)>,
    pub revert: bool,
    pub share: bool,
}

pub struct UnshelveDlg {
    pub shelf: Shelf,
    pub files: Vec<(ChangedFile, bool)>,
}

pub enum Dialog {
    Submit(SubmitDlg),
    Shelve(ShelveDlg),
    Unshelve(UnshelveDlg),
    Revert(Vec<(String, bool)>),
    Changelist { id: Option<u32>, description: String, move_files: Vec<String> },
    NewBranch { name: String, start: Option<String>, switch: bool },
    SwitchDirty(SwitchTarget),
    SyncBlocked(Vec<String>),
    TrustBinaries(crate::binaries::LoadedConfig),
    Confirm { title: String, body: String, action: Action },
    NewTag { rev: String, name: String, message: String, push: bool },
    AddRemote { name: String, url: String },
    Identity { name: String, email: String, global: bool },
}

/// A message shown at the bottom-right for a few seconds (errors stay until dismissed).
pub struct Toast {
    pub text: String,
    pub error: bool,
    pub created: Instant,
}

/// The Pending view's files, grouped by changelist. Rebuilt only when the status, the locks or the changelists
/// change, so drawing a frame costs the same with ten pending files or ten thousand.
#[derive(Default)]
pub struct PendingIndex {
    /// Every pending path, sorted.
    pub all: Vec<String>,
    by_cl: HashMap<u32, Rc<Vec<String>>>,
    /// How many files of each changelist have local changes (the rest are only checked out).
    changed: HashMap<u32, usize>,
    /// All files in display order (Default first, then each changelist), for shift-click ranges.
    pub order: Rc<Vec<String>>,
}

impl PendingIndex {
    pub fn files(&self, id: u32) -> Rc<Vec<String>> {
        self.by_cl.get(&id).cloned().unwrap_or_default()
    }

    pub fn changed_in(&self, id: u32) -> usize {
        self.changed.get(&id).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------- tab state

type PendingCache = ((u64, u64, u64), Rc<PendingIndex>);

pub struct RepoTab {
    pub repo: Repo,
    pub title: String,
    /// Path inside the parent repository when opened as a submodule tab.
    pub parent: Option<String>,
    pub(super) local: Worker<Msg>,
    net: Worker<Msg>,
    /// Precompiled binaries (only for repositories with `.g4v/binaries.toml`).
    pub bin: super::binaries::BinUi,

    pub status: Option<StatusSnapshot>,
    pub status_err: Option<String>,
    status_inflight: bool,
    last_status: Instant,
    /// How long the last `git status` took; huge workspaces are polled less often.
    status_cost: Duration,
    /// Bumped whenever a new status / lock list arrives (cache keys).
    status_gen: u64,
    locks_gen: u64,
    pending_cache: RefCell<Option<PendingCache>>,
    pub status_by_path: HashMap<String, usize>,
    pub changed_dirs: HashSet<String>,

    pub changelists: ChangelistStore,

    pub tree: FileTree,
    pub tracked: Vec<String>,
    tree_known: bool,
    tracked_set: HashSet<String>,
    /// Changed files that aren't tracked yet but are shown in the tree.
    tree_extra: HashSet<String>,
    pub lockable: HashSet<String>,
    pub lfs_paths: HashSet<String>,
    pub uses_lfs: bool,
    pub lfs_installed: bool,
    tree_stale: bool,

    pub remote: Option<String>,
    pub incoming: HashMap<String, char>,
    pub incoming_dirs: HashSet<String>,
    pub last_fetch: Option<Instant>,
    pub fetch_err: Option<String>,
    fetch_inflight: bool,

    pub locks: Vec<Lock>,
    pub locks_by_path: HashMap<String, usize>,
    pub locks_err: Option<String>,
    /// The lock list has been read from the server at least once.
    locks_known: bool,
    pub locked_dirs: HashSet<String>,

    pub history: Vec<Commit>,
    pub graph: Vec<GraphRow>,
    pub history_scope: HistoryScope,
    pub history_filter: String,
    /// History table sort: column and ascending? `None` = history order with the branch graph.
    pub history_sort: Option<(HistoryColumn, bool)>,
    /// Set by "Reset view" to restore the default column widths on the next frame.
    pub history_reset_columns: bool,
    history_gen: u64,
    pub history_loading: bool,
    pub history_err: Option<String>,
    pub selected_commit: Option<String>,
    pub commit_detail: Option<Rc<CommitDetail>>,

    pub shelves: Vec<Shelf>,
    pub selected_shelf: Option<String>,
    pub shelf_files: HashMap<String, Rc<Vec<ChangedFile>>>,
    pub show_backups: bool,

    pub refs: Option<RefsData>,

    pub center: CenterTab,
    pub left: LeftTab,
    pub selection: BTreeSet<String>,
    pub sel_anchor: Option<String>,
    pub diff_target: Option<DiffTarget>,
    pub diff: Option<Result<DiffData, String>>,
    pub diff_gen: u64,
    pub diff_loading: bool,
    /// Modification time and size of the working file whose diff is shown (to notice when it is saved again).
    diff_stamp: Option<(SystemTime, u64)>,
    /// The description typed into the last Submit dialog that didn't end in a commit: (changelist, text).
    pub submit_draft: Option<(u32, String)>,
    pub textures: (Option<egui::TextureHandle>, Option<egui::TextureHandle>),
    /// New diff data arrived; images must be (re-)uploaded.
    pub textures_stale: bool,

    pub dialog: Option<Dialog>,
    pub toasts: Vec<Toast>,
    /// Requests for the app (open a submodule tab, etc.).
    pub app_requests: Vec<AppRequest>,
    pub branch_filter: String,
    pub collapsed_cls: HashSet<u32>,
    /// Cached `user.name`/`user.email` (refreshed with the branch list).
    pub identity: crate::git::ops::Identity,
}

pub enum AppRequest {
    OpenRepo(std::path::PathBuf, Option<String>),
}

impl RepoTab {
    pub fn new(repo: Repo, ctx: &egui::Context, parent: Option<String>) -> Self {
        let c1 = ctx.clone();
        let c2 = ctx.clone();
        let local = Worker::new(repo.clone(), "local", move || c1.request_repaint());
        let net = Worker::new(repo.clone(), "net", move || c2.request_repaint());
        let changelists = ChangelistStore::load(&repo.meta_dir());
        let title = match &parent {
            Some(p) => format!("{} › {}", p, repo.name()),
            None => repo.name(),
        };
        let mut tab = RepoTab {
            title,
            parent,
            local,
            net,
            status: None,
            status_err: None,
            status_inflight: false,
            last_status: Instant::now(),
            status_cost: Duration::ZERO,
            status_gen: 0,
            locks_gen: 0,
            pending_cache: RefCell::new(None),
            status_by_path: HashMap::new(),
            changed_dirs: HashSet::new(),
            changelists,
            tree: FileTree::default(),
            tracked: Vec::new(),
            tree_known: false,
            tracked_set: HashSet::new(),
            tree_extra: HashSet::new(),
            lockable: HashSet::new(),
            lfs_paths: HashSet::new(),
            uses_lfs: repo.uses_lfs(),
            lfs_installed: true,
            tree_stale: true,
            incoming: HashMap::new(),
            incoming_dirs: HashSet::new(),
            last_fetch: None,
            fetch_err: None,
            fetch_inflight: false,
            locks: Vec::new(),
            locks_by_path: HashMap::new(),
            locks_err: None,
            locks_known: false,
            locked_dirs: HashSet::new(),
            history: Vec::new(),
            graph: Vec::new(),
            history_scope: HistoryScope::Current,
            history_filter: String::new(),
            history_sort: None,
            history_reset_columns: false,
            history_gen: 0,
            history_loading: false,
            history_err: None,
            selected_commit: None,
            commit_detail: None,
            shelves: Vec::new(),
            selected_shelf: None,
            shelf_files: HashMap::new(),
            show_backups: false,
            refs: None,
            center: CenterTab::Pending,
            left: LeftTab::Workspace,
            selection: BTreeSet::new(),
            sel_anchor: None,
            diff_target: None,
            diff: None,
            diff_gen: 0,
            diff_loading: false,
            diff_stamp: None,
            submit_draft: None,
            textures: (None, None),
            textures_stale: true,
            dialog: None,
            toasts: Vec::new(),
            app_requests: Vec::new(),
            branch_filter: String::new(),
            collapsed_cls: HashSet::new(),
            identity: repo.identity(),
            remote: repo.default_remote(),
            bin: super::binaries::BinUi::load(repo.root()),
            repo,
        };
        if let Some(e) = tab.changelists.load_error.clone() {
            tab.error(e);
        }
        tab.refresh_all();
        tab.request_fetch();
        tab
    }

    // ------------------------------------------------------------ helpers

    /// Clears the diff pane (when its subject is no longer selected).
    pub fn clear_diff(&mut self) {
        self.diff_target = None;
        self.diff = None;
        self.diff_gen += 1;
        self.diff_loading = false;
        self.diff_stamp = None;
        self.textures = (None, None);
        self.textures_stale = true;
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), error: false, created: Instant::now() });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), error: true, created: Instant::now() });
    }

    pub fn busy_label(&self) -> Option<String> {
        if let Some(p) = self.bin.progress_line() {
            return Some(p);
        }
        self.local.current().filter(|c| c != "status").or_else(|| self.net.current().filter(|c| c != "Checking server"))
    }

    pub fn is_busy(&self) -> bool {
        self.local.current().is_some_and(|c| c != "status") || self.local.queued() > 1
    }

    pub fn net_busy(&self) -> bool {
        self.net.busy()
    }

    pub fn has_pending_work(&self) -> bool {
        self.local.busy()
    }

    pub fn files(&self) -> &[FileStatus] {
        self.status.as_ref().map(|s| s.files.as_slice()).unwrap_or(&[])
    }

    pub fn file(&self, path: &str) -> Option<&FileStatus> {
        let s = self.status.as_ref()?;
        self.status_by_path.get(path).and_then(|&i| s.files.get(i))
    }

    pub fn lock_of(&self, path: &str) -> Option<&Lock> {
        self.locks_by_path.get(path).and_then(|&i| self.locks.get(i))
    }

    pub fn locked_by_me(&self) -> Vec<String> {
        self.locks.iter().filter(|l| l.ours).map(|l| l.path.clone()).collect()
    }

    pub fn op(&self) -> RepoOp {
        self.status.as_ref().map(|s| s.op).unwrap_or_default()
    }

    pub fn has_conflicts(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.has_conflicts())
    }

    /// What the Pending view shows: changed files plus files I have locked, grouped by changelist.
    pub fn pending(&self) -> Rc<PendingIndex> {
        let key = (self.status_gen, self.locks_gen, self.changelists.rev());
        if let Some((k, idx)) = self.pending_cache.borrow().as_ref() {
            if *k == key {
                return idx.clone();
            }
        }
        let mut all: Vec<String> = self.files().iter().filter(|f| f.kind != ChangeKind::Conflict).map(|f| f.path.clone()).collect();
        for l in self.locks.iter().filter(|l| l.ours) {
            if !self.status_by_path.contains_key(&l.path) {
                all.push(l.path.clone());
            }
        }
        all.sort();
        all.dedup();
        let grouped = self.changelists.group(&all);
        let mut order = Vec::with_capacity(all.len());
        let mut by_cl = HashMap::new();
        let mut changed = HashMap::new();
        for id in std::iter::once(DEFAULT_ID).chain(self.changelists.lists.iter().map(|c| c.id)) {
            if let Some(files) = grouped.get(&id) {
                order.extend(files.iter().cloned());
                changed.insert(id, files.iter().filter(|p| self.status_by_path.contains_key(*p)).count());
                by_cl.insert(id, Rc::new(files.clone()));
            }
        }
        let idx = Rc::new(PendingIndex { all, by_cl, changed, order: Rc::new(order) });
        *self.pending_cache.borrow_mut() = Some((key, idx.clone()));
        idx
    }

    /// Paths shown in the Pending view: changed files plus files I have locked.
    pub fn pending_paths(&self) -> Vec<String> {
        self.pending().all.clone()
    }

    /// Paths in changelist `id`.
    pub fn cl_files(&self, id: u32) -> Rc<Vec<String>> {
        self.pending().files(id)
    }

    pub fn changelist_name(&self, id: u32) -> String {
        if id == DEFAULT_ID {
            "Default".into()
        } else {
            self.changelists.get(id).map(|c| first_line(&c.description)).unwrap_or_else(|| format!("#{id}"))
        }
    }

    fn save_changelists(&mut self) {
        if let Err(e) = self.changelists.save() {
            self.error(format!("Could not save changelists: {e}"));
        }
    }

    /// Files that need attention before submitting.
    pub fn locked_by_others<'a>(&self, paths: impl IntoIterator<Item = &'a String>) -> Vec<(String, String)> {
        paths.into_iter().filter_map(|p| self.lock_of(p).filter(|l| !l.ours).map(|l| (p.clone(), l.owner.clone()))).collect()
    }

    pub fn lockable_not_locked<'a>(&self, paths: impl IntoIterator<Item = &'a String>) -> Vec<String> {
        paths
            .into_iter()
            .filter(|p| self.lockable.contains(*p) && self.lock_of(p).is_none())
            .filter(|p| self.file(p).is_some_and(|f| f.kind != ChangeKind::Add))
            .cloned()
            .collect()
    }

    /// Files over the size limit that aren't stored with LFS. This touches the disk once per file, so call it when
    /// a dialog opens, not on every frame.
    pub fn large_non_lfs(&self, paths: &[String], threshold_mb: u64) -> Vec<(String, u64)> {
        let limit = threshold_mb.saturating_mul(1024 * 1024);
        paths
            .iter()
            .filter(|p| !self.lfs_paths.contains(*p))
            .filter(|p| self.file(p).is_some_and(|f| matches!(f.kind, ChangeKind::Add | ChangeKind::Edit | ChangeKind::Move)))
            .filter_map(|p| self.repo.file_size(p).filter(|s| *s >= limit).map(|s| (p.clone(), s)))
            .collect()
    }

    /// (modification time, size) of a working file.
    fn working_stamp(&self, path: &str) -> Option<(SystemTime, u64)> {
        let m = std::fs::metadata(self.repo.abs(path)).ok()?;
        Some((m.modified().ok()?, m.len()))
    }

    // ------------------------------------------------------------ requests

    pub fn refresh_all(&mut self) {
        let root = self.repo.root().to_path_buf();
        self.bin.reload(&root);
        self.request_status(true);
        self.tree_stale = true;
        self.request_tree();
        self.request_refs();
        self.request_shelves();
        self.request_history();
        self.request_locks();
    }

    pub fn request_status(&mut self, force: bool) {
        if self.status_inflight && !force {
            return;
        }
        self.status_inflight = true;
        self.last_status = Instant::now();
        self.local.submit("status", |r| {
            let started = Instant::now();
            let res = r.status().map_err(es);
            Msg::Status(res, started.elapsed())
        });
    }

    fn request_tree(&mut self) {
        if !self.tree_stale {
            return;
        }
        self.tree_stale = false;
        self.local.submit("Reading files", |r| {
            Msg::Tree((|| {
                let tracked = r.tracked_files().map_err(es)?;
                // Attributes can come from nested folders or .git/info/attributes, not just the root file.
                let mut candidates = tracked.clone();
                candidates.extend(r.status().map_err(es)?.files.into_iter().map(|f| f.path));
                candidates.sort();
                candidates.dedup();
                let lockable = r.lockable_paths(&candidates).map_err(es)?;
                let lfs = r.lfs_tracked_paths(&candidates).map_err(es)?;
                let uses_lfs = r.uses_lfs() || !lockable.is_empty() || !lfs.is_empty();
                let lfs_installed = !uses_lfs || r.lfs_installed();
                Ok(TreeData { tracked, lockable, lfs, uses_lfs, lfs_installed })
            })())
        });
    }

    pub fn request_refs(&mut self) {
        self.local.submit("Reading branches", |r| {
            Msg::Refs(r.branches().map_err(es).map(|branches| RefsData {
                identity: r.identity(),
                default_remote: r.default_remote(),
                branches,
                remotes: r.remotes(),
                submodules: r.submodules(),
                stashes: r.stashes(),
            }))
        });
    }

    pub fn request_shelves(&mut self) {
        self.local.submit("Reading shelves", |r| Msg::Shelves(r.list_shelves().map_err(es)));
    }

    pub fn request_history(&mut self) {
        self.history_gen += 1;
        self.history_loading = true;
        let gen = self.history_gen;
        let scope = self.history_scope.clone();
        let has_up = self.status.as_ref().map(|s| s.branch.upstream.is_some()).unwrap_or(true);
        self.local.submit("Reading history", move |r| {
            let res = r.log(&scope, 2000, has_up);
            // Retry without the upstream if it vanished.
            let res = match res {
                Err(_) if has_up && scope == HistoryScope::Current => r.log(&scope, 2000, false),
                other => other,
            };
            Msg::History(gen, res.map_err(es))
        });
    }

    pub fn request_locks(&mut self) {
        if !self.uses_lfs {
            return;
        }
        self.net.submit("Checking locks", |r| Msg::Locks(r.lfs_locks().map_err(es)));
    }

    pub fn request_fetch(&mut self) {
        if self.fetch_inflight {
            return;
        }
        self.fetch_inflight = true;
        let bin_cfg = self.bin.active().cloned();
        self.net.submit("Checking server", move |r| {
            Msg::Fetched((|| {
                let remote = r.fetch().map_err(es)?;
                let incoming = r.incoming_changes().map_err(es)?;
                let binaries = bin_cfg.and_then(|c| super::binaries::status_after_fetch(r, &c, remote.as_deref()));
                Ok(FetchData { remote, incoming, binaries })
            })())
        });
        self.request_locks();
    }

    pub fn request_diff(&mut self, target: DiffTarget) {
        self.diff_gen += 1;
        let gen = self.diff_gen;
        self.diff_stamp = match &target {
            DiffTarget::Working { path } => self.working_stamp(path),
            _ => None,
        };
        self.diff_target = Some(target.clone());
        self.diff_loading = true;
        let has_head = self.status.as_ref().is_some_and(|s| s.branch.oid.is_some());
        let file = match &target {
            DiffTarget::Working { path } => self.file(path).cloned(),
            _ => None,
        };
        self.local.submit("Loading diff", move |r| Msg::Diff(gen, load_diff(r, &target, file.as_ref(), has_head)));
    }

    // ------------------------------------------------------------ per-frame

    pub fn tick(&mut self, ctx: &egui::Context, settings: &Settings) {
        for msg in self.local.poll().into_iter().chain(self.net.poll()) {
            self.handle_msg(msg, settings);
        }
        for error in self.local.poll_errors().into_iter().chain(self.net.poll_errors()) {
            self.error(error);
            self.status_inflight = false;
            self.fetch_inflight = false;
            self.diff_loading = false;
            self.history_loading = false;
            self.tree_stale = true;
        }
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if self.status_inflight && !self.local.busy() {
            // The status job is gone without an answer (it can only have crashed); don't wait for it forever.
            self.status_inflight = false;
        }
        // Poll the working copy for changes made by other programs (Photoshop, Blender, ...). In a huge workspace a
        // scan takes a while, so leave at least five times its duration between scans: polling never uses more than
        // a sixth of a core and never queues up in front of what the artist asks for.
        let base = if focused { Duration::from_secs(2) } else { Duration::from_secs(10) };
        let interval = base.max(self.status_cost * 5).min(Duration::from_secs(60));
        if self.last_status.elapsed() >= interval && !self.local.busy() {
            self.request_status(false);
        }
        if self.tree_stale && !self.local.busy() {
            self.request_tree();
        }
        if settings.fetch_interval_secs > 0 {
            let due = self.last_fetch.is_none_or(|t| t.elapsed() >= Duration::from_secs(settings.fetch_interval_secs));
            if due && !self.fetch_inflight && !self.local.busy() && self.last_fetch.is_some() {
                self.request_fetch();
            }
        }
        self.toasts.retain(|t| t.error || t.created.elapsed() < Duration::from_secs(6));
        ctx.request_repaint_after(Duration::from_millis(if focused { 1000 } else { 5000 }));
    }

    fn handle_msg(&mut self, msg: Msg, settings: &Settings) {
        match msg {
            Msg::Status(res, cost) => {
                self.status_inflight = false;
                self.status_cost = cost;
                // Measure the pause from when the scan finished, not from when it was asked for.
                self.last_status = Instant::now();
                match res {
                    Ok(s) => self.apply_status(s),
                    Err(e) => self.status_err = Some(e),
                }
            }
            Msg::Tree(res) => match res {
                Ok(t) => {
                    let newly_uses_lfs = t.uses_lfs && !self.uses_lfs;
                    self.tree_known = true;
                    self.tracked_set = t.tracked.iter().cloned().collect();
                    self.tracked = t.tracked;
                    self.lockable = t.lockable;
                    self.lfs_paths = t.lfs;
                    self.uses_lfs = t.uses_lfs;
                    self.lfs_installed = t.lfs_installed;
                    if newly_uses_lfs {
                        self.request_locks();
                    }
                    self.rebuild_tree();
                    self.prune_changelists();
                }
                Err(e) => self.error(format!("Could not list files: {e}")),
            },
            Msg::Fetched(res) => {
                self.fetch_inflight = false;
                self.last_fetch = Some(Instant::now());
                match res {
                    Ok(f) => {
                        self.fetch_err = None;
                        self.remote = f.remote;
                        self.bin.status = f.binaries;
                        let new: HashMap<String, char> = f.incoming.into_iter().map(|(c, p)| (p, c)).collect();
                        let changed = new != self.incoming;
                        self.incoming = new;
                        self.incoming_dirs = ancestor_dirs(self.incoming.keys());
                        if changed {
                            self.rebuild_tree();
                            self.request_history();
                        }
                        self.request_status(true);
                        self.request_shelves();
                        self.request_refs();
                    }
                    Err(e) => self.fetch_err = Some(e),
                }
            }
            Msg::Incoming(res) => {
                self.fetch_inflight = false;
                if let Ok(list) = res {
                    let new: HashMap<String, char> = list.into_iter().map(|(c, p)| (p, c)).collect();
                    if new != self.incoming {
                        self.incoming = new;
                        self.incoming_dirs = ancestor_dirs(self.incoming.keys());
                        self.rebuild_tree();
                    }
                }
            }
            Msg::Locks(res) => match res {
                Ok(mut l) => {
                    // Mine first, then by path: the order the Locks tab shows.
                    l.sort_by(|a, b| b.ours.cmp(&a.ours).then(a.path.cmp(&b.path)));
                    self.locks = l;
                    self.locks_gen += 1;
                    self.locks_known = true;
                    self.locks_by_path = self.locks.iter().enumerate().map(|(i, l)| (l.path.clone(), i)).collect();
                    self.locked_dirs = ancestor_dirs(self.locks.iter().map(|l| &l.path));
                    self.locks_err = None;
                    self.tree.mark_dirty();
                    self.prune_changelists();
                }
                Err(e) => {
                    self.locks_known = false;
                    self.locks_err = Some(e);
                }
            },
            Msg::History(gen, res) => {
                if gen == self.history_gen {
                    self.history_loading = false;
                    match res {
                        Ok(c) => {
                            self.graph = build_graph(&c);
                            self.history = c;
                            self.history_err = None;
                        }
                        Err(e) => self.history_err = Some(e),
                    }
                }
            }
            Msg::CommitDetail(res) => match res {
                Ok(d) => {
                    if self.selected_commit.as_deref() == Some(d.commit.id.as_str()) {
                        self.commit_detail = Some(Rc::new(d));
                    }
                }
                Err(e) => self.error(e),
            },
            Msg::Diff(gen, res) => {
                if gen == self.diff_gen {
                    self.diff_loading = false;
                    self.diff = Some(res);
                    self.textures_stale = true;
                }
            }
            Msg::Shelves(res) => match res {
                Ok(s) => {
                    self.shelves = s;
                    if let Some(sel) = &self.selected_shelf {
                        if !self.shelves.iter().any(|s| &s.refname == sel) {
                            self.selected_shelf = None;
                        }
                    }
                }
                Err(e) => self.error(format!("Could not list shelves: {e}")),
            },
            Msg::ShelfFiles(id, res) => match res {
                Ok(f) => {
                    self.shelf_files.insert(id, Rc::new(f));
                }
                Err(e) => self.error(e),
            },
            Msg::Refs(res) => match res {
                Ok(r) => {
                    self.identity = r.identity.clone();
                    if r.default_remote.is_some() {
                        self.remote = r.default_remote.clone();
                    }
                    self.refs = Some(r);
                }
                Err(e) => self.error(format!("Could not list branches: {e}")),
            },
            Msg::Done { title, result, net } => {
                self.on_done(title, result, settings);
                if net {
                    self.request_locks();
                }
                self.after_change();
            }
        }
    }

    fn apply_status(&mut self, s: StatusSnapshot) {
        let files_changed = self.status.as_ref().map(|o| o.files != s.files).unwrap_or(true);
        let head_changed = self.status.as_ref().map(|o| o.branch != s.branch).unwrap_or(true);
        let shown = match &self.diff_target {
            Some(DiffTarget::Working { path }) => Some((path.clone(), self.file(path).cloned())),
            _ => None,
        };
        if files_changed {
            self.status_by_path = s.files.iter().enumerate().map(|(i, f)| (f.path.clone(), i)).collect();
            self.changed_dirs = ancestor_dirs(s.files.iter().map(|f| &f.path));
        }
        self.status = Some(s);
        self.status_gen += 1;
        self.status_err = None;
        if files_changed {
            self.prune_changelists();
            // Unchanged files stay selected too (they may be about to be checked out).
            let mut live: HashSet<String> = self.pending().all.iter().cloned().collect();
            live.extend(self.locks.iter().map(|l| l.path.clone()));
            let tracked = &self.tracked_set;
            self.selection.retain(|p| live.contains(p) || tracked.contains(p));
            let extra: HashSet<String> = self.files().iter().filter(|f| !tracked.contains(&f.path)).map(|f| f.path.clone()).collect();
            if extra != self.tree_extra || self.tree.nodes.is_empty() {
                self.rebuild_tree();
            } else {
                self.tree.mark_dirty();
            }
        }
        // Keep the diff of the selected working file current: reload it when its status changes or it is saved
        // again, but not every time some other file in the workspace changes.
        if let Some((path, before)) = shown {
            match self.file(&path).cloned() {
                None => self.clear_diff(),
                now => {
                    if now != before || self.working_stamp(&path) != self.diff_stamp {
                        self.request_diff(DiffTarget::Working { path });
                    }
                }
            }
        }
        if head_changed {
            self.tree_stale = true;
            self.request_history();
            self.request_refs();
        }
    }

    /// Forgets changelist entries for files that are no longer pending.
    ///
    /// Deliberately cautious, because a wrongly dropped entry silently moves a file to the Default changelist:
    /// * not while a merge or rebase is under way (git sets uncommitted work aside until it finishes),
    /// * not before the first status and file list have arrived,
    /// * and until the server has told us which files I have checked out, entries for lockable files are kept: they
    ///   may be "checked out, unchanged" files that haven't shown up yet (or can't, when working offline).
    fn prune_changelists(&mut self) {
        let Some(st) = &self.status else { return };
        if !self.tree_known || st.op != RepoOp::None || st.index_lock || self.changelists.lists.iter().all(|l| l.files.is_empty()) {
            return;
        }
        let locks_pending = self.uses_lfs && !self.locks_known;
        if locks_pending && self.tracked.is_empty() {
            return;
        }
        let mut keep: HashSet<String> = self.pending().all.iter().cloned().collect();
        if locks_pending {
            let entries = self.changelists.lists.iter().flat_map(|l| l.files.iter());
            keep.extend(entries.filter(|f| self.lockable.contains(*f) || self.lfs_paths.contains(*f)).cloned());
        }
        if self.changelists.prune(&keep) {
            self.save_changelists();
        }
    }

    fn rebuild_tree(&mut self) {
        let extra: HashSet<String> = self.files().iter().filter(|f| !self.tracked_set.contains(&f.path)).map(|f| f.path.clone()).collect();
        let mut files: Vec<String> = self.tracked.clone();
        files.extend(self.files().iter().filter(|f| extra.contains(&f.path)).map(|f| f.path.clone()));
        self.tree_extra = extra;
        let have: HashSet<String> = files.iter().cloned().collect();
        let ghosts: Vec<String> = self.incoming.iter().filter(|(p, c)| **c != 'D' && !have.contains(*p)).map(|(p, _)| p.clone()).collect();
        self.tree.rebuild(&files, &ghosts);
    }

    /// Something changed in the repository: refresh everything that may be affected.
    fn after_change(&mut self) {
        self.request_status(true);
        self.tree_stale = true;
        self.request_refs();
        self.request_shelves();
        self.request_history();
        // Incoming files depend on HEAD; recompute without hitting the network.
        self.net.submit("Comparing with server", |r| {
            if r.current_op() != RepoOp::None {
                return Msg::Incoming(Err("operation in progress".into()));
            }
            Msg::Incoming(r.incoming_changes().map_err(es))
        });
        self.fetch_inflight = true;
    }

    fn on_done(&mut self, title: String, result: Result<OpOk, String>, settings: &Settings) {
        let ok = match result {
            Ok(ok) => ok,
            Err(e) => {
                match hint_for(&e) {
                    Some(hint) => self.error(format!("{title} failed. {hint}\n\nDetails:\n{e}")),
                    None => self.error(format!("{title} failed:\n{e}")),
                }
                return;
            }
        };
        match ok {
            OpOk::Message(m) => self.toast(m),
            OpOk::Sync(outcome) => self.on_sync(outcome),
            OpOk::Binaries(outcome) => self.bin_on_done(outcome),
            OpOk::Submitted { commit, cl, cl_id, push, unlocked, unlock_failed } => {
                // The change is committed, so the typed description is no longer needed.
                self.submit_draft = None;
                if cl_id != DEFAULT_ID {
                    self.changelists.delete(cl_id);
                    self.save_changelists();
                }
                let short = crate::git::history::cl_label(cl, &commit);
                match push {
                    None => self.toast(format!("Committed {short} locally. Use Push to upload it to the server.")),
                    Some(Ok(m)) => self.toast(format!("Submitted {short}. {m}")),
                    Some(Err(e)) => {
                        let hint = hint_for(&e).map(|h| format!("{h}\n\n")).unwrap_or_default();
                        self.error(format!(
                            "Your change was committed locally ({short}) but could not be uploaded.\n{hint}Nothing is lost. Press Push to try again.\n\nDetails:\n{e}"
                        ))
                    }
                }
                if !unlocked.is_empty() {
                    self.toast(format!("Released {} lock(s).", unlocked.len()));
                }
                if !unlock_failed.is_empty() {
                    self.error(failure_summary("Could not release the lock on", &unlock_failed));
                }
            }
            OpOk::Shelved { shelf, reverted, share, cl_id } => {
                if reverted {
                    if let Some(id) = cl_id.filter(|i| *i != DEFAULT_ID) {
                        self.changelists.delete(id);
                        self.save_changelists();
                    }
                }
                let mut m = format!("Shelved \"{}\"", first_line(&shelf.description));
                if reverted {
                    m.push_str(" and reverted the files");
                }
                match share {
                    Some(Ok(())) => m.push_str(" — shared with the team"),
                    Some(Err(e)) => self.error(format!("Shelf saved locally but could not be shared: {e}")),
                    None => {}
                }
                self.toast(m);
            }
            OpOk::Unshelved { paths, description, backup, new_changelist } => {
                let mut m = if new_changelist {
                    let id = self.changelists.create(&description);
                    self.changelists.move_files(&paths, id);
                    self.save_changelists();
                    format!("Unshelved {} file(s) into changelist \"{}\".", paths.len(), first_line(&description))
                } else {
                    format!("Restored {} file(s).", paths.len())
                };
                if let Some(b) = backup {
                    m.push_str(&format!(" Your previous versions were backed up as shelf \"{b}\"."));
                }
                self.toast(m);
                self.center = CenterTab::Pending;
            }
            OpOk::Locks { verb, ok, failed } => {
                if !ok.is_empty() {
                    self.toast(format!("{verb} {} file(s).", ok.len()));
                }
                if !failed.is_empty() {
                    self.error(failure_summary(&format!("{verb}: could not process"), &failed));
                }
            }
            OpOk::BlockedResolved { shelf, sync } => {
                self.toast(format!(
                    "Your edits were shelved as \"{}\". Unshelve them from the Shelves tab when you're ready.",
                    first_line(&shelf.description)
                ));
                self.on_sync(sync);
            }
        }
        let _ = settings;
    }

    pub(super) fn on_sync(&mut self, outcome: SyncOutcome) {
        match outcome {
            SyncOutcome::UpToDate => self.toast("You already have the latest version."),
            SyncOutcome::Updated { commits } => self.toast(format!("Got latest: {commits} new change(s) from the server.")),
            SyncOutcome::NoUpstream => self.toast("This branch isn't on the server yet — submit or push to publish it."),
            SyncOutcome::Blocked { overlap } => self.dialog = Some(Dialog::SyncBlocked(overlap)),
            SyncOutcome::Conflicts => {
                self.center = CenterTab::Pending;
                self.toast(
                    "Some files were changed both by you and on the server. Resolve the conflicts in the Pending tab, then press Continue.",
                );
            }
        }
    }

    // ------------------------------------------------------------ actions

    pub fn handle(&mut self, action: Action, settings: &Settings) {
        match action {
            Action::Refresh => {
                self.refresh_all();
                self.request_fetch();
            }
            Action::Fetch => self.request_fetch(),
            Action::GetLatest => {
                if self.has_conflicts() || self.op() != RepoOp::None {
                    self.error("Finish resolving the current conflicts first (Pending tab).");
                    return;
                }
                if self.bin_get_latest(settings) {
                    return;
                }
                let mode = settings.pull_mode;
                self.local.submit("Getting latest", move |r| {
                    let dirty: Vec<String> = r.status().map(|s| s.files.into_iter().map(|f| f.path).collect()).unwrap_or_default();
                    Msg::Done { title: "Get Latest".into(), result: r.sync(mode, &dirty).map(OpOk::Sync).map_err(es), net: false }
                });
            }
            Action::TrustBinaries(trusted) => self.bin_set_trust(trusted, settings),
            Action::LaunchEditor => self.bin_launch(),
            Action::ShelveOverlapAndSync(paths) => {
                let mode = settings.pull_mode;
                let branch = self.branch_name();
                self.local.submit("Shelving and getting latest", move |r| {
                    let res = (|| {
                        let files = fresh_statuses(r, &paths)?;
                        let mut all = paths.clone();
                        all.extend(files.iter().filter_map(|f| f.orig_path.clone()));
                        let shelf = r.create_shelf(&all, &format!("My edits set aside to get latest on {branch}")).map_err(es)?;
                        r.revert_files(&files).map_err(es)?;
                        let dirty: Vec<String> = r.status().map(|s| s.files.into_iter().map(|f| f.path).collect()).unwrap_or_default();
                        let sync = r.sync(mode, &dirty).map_err(es)?;
                        Ok(OpOk::BlockedResolved { shelf, sync })
                    })();
                    Msg::Done { title: "Get Latest".into(), result: res, net: false }
                });
            }
            Action::Push => {
                let mode = settings.pull_mode;
                self.local.submit("Uploading", move |r| Msg::Done {
                    title: "Push".into(),
                    result: push_with_retry(r, mode).map(OpOk::Message),
                    net: false,
                });
            }
            Action::ShowDiff(t) => self.request_diff(t),
            Action::OpenSubmit(cl_id) => self.open_submit(cl_id, settings),
            Action::OpenShelve(paths, cl_id) => {
                let files: Vec<String> = paths.into_iter().filter(|p| self.file(p).is_some()).collect();
                if files.is_empty() {
                    self.error("There are no changed files to shelve.");
                    return;
                }
                let description = cl_id
                    .filter(|i| *i != DEFAULT_ID)
                    .and_then(|i| self.changelists.get(i))
                    .map(|c| c.description.clone())
                    .unwrap_or_default();
                self.dialog = Some(Dialog::Shelve(ShelveDlg {
                    cl_id,
                    description,
                    files: files.into_iter().map(|f| (f, true)).collect(),
                    revert: false,
                    share: false,
                }));
            }
            Action::OpenRevert(paths) => {
                let files: Vec<(String, bool)> =
                    paths.into_iter().filter(|p| self.file(p).is_some_and(|f| f.kind != ChangeKind::Conflict)).map(|p| (p, true)).collect();
                if files.is_empty() {
                    self.error("Nothing to revert: the selected files have no local changes.");
                    return;
                }
                self.dialog = Some(Dialog::Revert(files));
            }
            Action::NewChangelist(files) => {
                self.dialog = Some(Dialog::Changelist { id: None, description: String::new(), move_files: files })
            }
            Action::EditChangelist(id) => {
                if let Some(c) = self.changelists.get(id) {
                    self.dialog = Some(Dialog::Changelist { id: Some(id), description: c.description.clone(), move_files: vec![] });
                }
            }
            Action::DeleteChangelist(id) => {
                self.changelists.delete(id);
                self.save_changelists();
            }
            Action::MoveFiles(paths, to) => {
                self.changelists.move_files(&paths, to);
                self.save_changelists();
            }
            Action::Lock(paths) => {
                if !self.uses_lfs {
                    self.error("File locking needs Git LFS. Ask your repository admin to enable it.");
                    return;
                }
                let paths: Vec<String> = paths.into_iter().filter(|p| self.lock_of(p).is_none()).collect();
                if paths.is_empty() {
                    self.toast("Those files are already locked.");
                    return;
                }
                self.net.submit("Checking out (locking)", move |r| {
                    let (ok, failed) = r.lfs_lock(&paths);
                    Msg::Done { title: "Check out".into(), result: Ok(OpOk::Locks { verb: "Checked out", ok, failed }), net: true }
                });
            }
            Action::Unlock(paths, force) => {
                let mine: Vec<String> = paths.iter().filter(|p| self.lock_of(p).is_some_and(|l| l.ours)).cloned().collect();
                let theirs: Vec<String> = paths.iter().filter(|p| self.lock_of(p).is_some_and(|l| !l.ours)).cloned().collect();
                if !theirs.is_empty() && force {
                    let owners: Vec<String> = theirs
                        .iter()
                        .map(|p| format!("{p} (locked by {})", self.lock_of(p).map(|l| l.owner.clone()).unwrap_or_default()))
                        .collect();
                    self.dialog = Some(Dialog::Confirm {
                        title: "Break someone else's lock?".into(),
                        body: format!(
                            "These files are checked out by someone else. Breaking their lock may cause them to lose work:\n\n{}",
                            owners.join("\n")
                        ),
                        action: Action::DoForceUnlock(theirs),
                    });
                }
                if !mine.is_empty() {
                    let modified: Vec<&String> = mine.iter().filter(|p| self.file(p).is_some()).collect();
                    if !modified.is_empty() {
                        self.toast(format!(
                            "Note: {} of these files still have unsubmitted changes. Others may now edit them.",
                            modified.len()
                        ));
                    }
                    self.net.submit("Releasing locks", move |r| {
                        let (ok, failed) = r.lfs_unlock(&mine, false);
                        Msg::Done { title: "Release lock".into(), result: Ok(OpOk::Locks { verb: "Released", ok, failed }), net: true }
                    });
                }
            }
            Action::DoForceUnlock(paths) => {
                self.net.submit("Breaking locks", move |r| {
                    let (ok, failed) = r.lfs_unlock(&paths, true);
                    Msg::Done { title: "Break lock".into(), result: Ok(OpOk::Locks { verb: "Broke lock on", ok, failed }), net: true }
                });
            }
            Action::OpenFile(p) => crate::git::ops::open_path(&self.repo.abs(&p)),
            Action::RevealFile(p) => crate::git::ops::reveal_path(&self.repo.abs(&p)),
            Action::FileHistory(p) => {
                self.history_scope = HistoryScope::File(p);
                self.center = CenterTab::History;
                self.request_history();
            }
            Action::RevealInTree(p) => {
                self.left = LeftTab::Workspace;
                self.tree.search.clear();
                self.tree.filter = TreeFilter::All;
                self.tree.reveal(&p);
                self.selection = [p].into_iter().collect();
            }
            Action::Resolve(path, side) => {
                let Some(f) = self.file(&path).cloned() else { return };
                let op = self.op();
                self.local.submit("Resolving", move |r| Msg::Done {
                    title: "Resolve".into(),
                    result: r.resolve_take(&f, side, op).map(|_| OpOk::Message(format!("Resolved {}", f.path))).map_err(es),
                    net: false,
                });
            }
            Action::MarkResolved(path) => {
                self.local.submit("Resolving", move |r| Msg::Done {
                    title: "Mark resolved".into(),
                    result: r.mark_resolved(&path).map(|_| OpOk::Message(format!("Marked {path} as resolved"))).map_err(es),
                    net: false,
                });
            }
            Action::MergeTool(path) => {
                self.local.submit("Waiting for merge tool", move |r| Msg::Done {
                    title: "Merge tool".into(),
                    result: r.run_mergetool(&path).map(|_| OpOk::Message(format!("Merged {path}"))).map_err(es),
                    net: false,
                });
            }
            Action::ContinueOp => {
                let op = self.op();
                if self.has_conflicts() {
                    self.error("Resolve every conflicted file first.");
                    return;
                }
                self.local.submit("Continuing", move |r| {
                    let res = r.continue_op(op).map_err(es).and_then(|_| {
                        let st = r.status().map_err(es)?;
                        if st.has_conflicts() {
                            Ok(OpOk::Sync(SyncOutcome::Conflicts))
                        } else {
                            Ok(OpOk::Message(format!("Finished the {}.", op.label())))
                        }
                    });
                    Msg::Done { title: "Continue".into(), result: res, net: false }
                });
            }
            Action::AbortOp => {
                let op = self.op();
                self.dialog = Some(Dialog::Confirm {
                    title: format!("Abort the {}?", op.label()),
                    body: "This returns to the state before the operation. Current file edits and conflict-resolution work are backed up in Shelves before aborting.".into(),
                    action: Action::DoAbortOp,
                });
            }
            Action::DoAbortOp => {
                let op = self.op();
                self.local.submit("Aborting", move |r| Msg::Done {
                    title: "Abort".into(),
                    result: r.abort_op(op).map(|_| OpOk::Message("Back to where you were.".into())).map_err(es),
                    net: false,
                });
            }
            Action::Switch(target) => {
                let dirty = self.files().iter().any(|f| !f.submodule);
                if dirty {
                    self.dialog = Some(Dialog::SwitchDirty(target));
                } else {
                    self.handle(Action::SwitchNow(target, false), settings);
                }
            }
            Action::SwitchNow(target, shelve_first) => {
                self.local.submit("Switching branch", move |r| {
                    let res = (|| {
                        let status = r.status().map_err(es)?;
                        let from = status.branch.head.unwrap_or_else(|| "detached".into());
                        let files: Vec<_> = status.files.into_iter().filter(|f| !f.submodule).collect();
                        if shelve_first && !files.is_empty() {
                            let mut paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
                            paths.extend(files.iter().filter_map(|f| f.orig_path.clone()));
                            r.create_shelf(&paths, &wip_description(&from)).map_err(es)?;
                            r.revert_files(&files).map_err(es)?;
                        }
                        match &target {
                            SwitchTarget::Local(b) => r.switch_branch(b).map_err(es)?,
                            SwitchTarget::Remote(b) => r.checkout_remote_branch(b).map_err(es)?,
                        }
                        let name = match &target {
                            SwitchTarget::Local(b) | SwitchTarget::Remote(b) => b.clone(),
                        };
                        Ok(OpOk::Message(if shelve_first {
                            format!("Switched to {name}. Your changes were shelved and will be offered back when you return to {from}.")
                        } else {
                            format!("Switched to {name}.")
                        }))
                    })();
                    Msg::Done { title: "Switch branch".into(), result: res, net: false }
                });
            }
            Action::NewBranch(start) => {
                self.dialog = Some(Dialog::NewBranch { name: String::new(), start, switch: true });
            }
            Action::DeleteBranch(name, _) => {
                self.dialog = Some(Dialog::Confirm {
                    title: format!("Delete branch {name}?"),
                    body: "The branch is only deleted if all of its work has been merged somewhere else.".into(),
                    action: Action::DoDeleteBranch(name, false),
                });
            }
            Action::DoDeleteBranch(name, force) => {
                self.local.submit("Deleting branch", move |r| {
                    let res = r.delete_branch(&name, force).map(|_| OpOk::Message(format!("Deleted branch {name}"))).map_err(|e| {
                        if e.stderr.contains("not fully merged") {
                            format!("{name} has work that isn't merged anywhere. Merge it first, or delete it from the branch's menu with 'Delete (discard unmerged work)'.")
                        } else {
                            es(e)
                        }
                    });
                    Msg::Done { title: "Delete branch".into(), result: res, net: false }
                });
            }
            Action::DeleteRemoteBranch(name) => {
                self.dialog = Some(Dialog::Confirm {
                    title: format!("Delete {name} from the server?"),
                    body: "This removes the branch for everyone on the team.".into(),
                    action: Action::DoDeleteRemoteBranch(name),
                });
            }
            Action::DoDeleteRemoteBranch(name) => {
                self.local.submit("Deleting remote branch", move |r| Msg::Done {
                    title: "Delete remote branch".into(),
                    result: r.delete_remote_branch(&name).map(|_| OpOk::Message(format!("Deleted {name} from the server"))).map_err(es),
                    net: false,
                });
            }
            Action::MergeBranch(name) => {
                let cur = self.branch_name();
                self.dialog = Some(Dialog::Confirm {
                    title: format!("Merge {name} into {cur}?"),
                    body: format!("All changes from {name} will be brought into {cur}. If the same files were changed on both, you'll be asked to resolve them."),
                    action: Action::DoMerge(name),
                });
            }
            Action::DoMerge(name) => {
                self.local.submit("Merging", move |r| {
                    let res = match r.merge_into_current(&name) {
                        Ok(()) => Ok(OpOk::Message(format!("Merged {name}."))),
                        Err(e) => match r.status() {
                            Ok(s) if s.has_conflicts() => Ok(OpOk::Sync(SyncOutcome::Conflicts)),
                            _ => Err(es(e)),
                        },
                    };
                    Msg::Done { title: "Merge".into(), result: res, net: false }
                });
            }
            Action::Confirm { action, .. } => self.handle(*action, settings),
            Action::SelectCommit(id) => {
                if self.selected_commit.as_ref() != Some(&id) {
                    self.clear_diff();
                }
                self.selected_commit = Some(id.clone());
                self.commit_detail = None;
                self.local.submit("Loading change", move |r| Msg::CommitDetail(r.commit_detail(&id).map_err(es)));
            }
            Action::RevertCommit(id, merge) => {
                self.dialog = Some(Dialog::Confirm {
                    title: "Back out this change?".into(),
                    body: "Creates a new change that undoes it. History is kept, so this is safe even after it was submitted.".into(),
                    action: Action::DoRevertCommit(id, merge),
                });
            }
            Action::DoRevertCommit(id, merge) => {
                self.local.submit("Backing out change", move |r| {
                    let res = match r.revert_commit(&id, merge) {
                        Ok(()) => Ok(OpOk::Message("Created a change that backs it out. Push to share it.".into())),
                        Err(e) => match r.status() {
                            Ok(s) if s.has_conflicts() => Ok(OpOk::Sync(SyncOutcome::Conflicts)),
                            _ => Err(es(e)),
                        },
                    };
                    Msg::Done { title: "Back out".into(), result: res, net: false }
                });
            }
            Action::CherryPick(id) => {
                self.local.submit("Copying change", move |r| {
                    let res = match r.cherry_pick(&id) {
                        Ok(()) => Ok(OpOk::Message("Copied the change onto this branch.".into())),
                        Err(e) => match r.status() {
                            Ok(s) if s.has_conflicts() => Ok(OpOk::Sync(SyncOutcome::Conflicts)),
                            _ => Err(es(e)),
                        },
                    };
                    Msg::Done { title: "Cherry-pick".into(), result: res, net: false }
                });
            }
            Action::UndoLastCommit => {
                self.dialog = Some(Dialog::Confirm {
                    title: "Undo last commit?".into(),
                    body: "The commit is removed but all of its changes stay in your workspace, ready to fix and submit again.".into(),
                    action: Action::DoUndoLastCommit,
                });
            }
            Action::DoUndoLastCommit => {
                self.local.submit("Undoing commit", |r| Msg::Done {
                    title: "Undo commit".into(),
                    result: r
                        .undo_last_commit()
                        .map(|_| OpOk::Message("Commit undone; its changes are back in your workspace.".into()))
                        .map_err(es),
                    net: false,
                });
            }
            Action::NewTag(rev) => {
                self.dialog = Some(Dialog::NewTag { rev, name: String::new(), message: String::new(), push: true });
            }
            Action::DeleteTag(name) => {
                self.local.submit("Deleting tag", move |r| Msg::Done {
                    title: "Delete tag".into(),
                    result: r.delete_tag(&name).map(|_| OpOk::Message(format!("Deleted tag {name} locally"))).map_err(es),
                    net: false,
                });
            }
            Action::PushTag(name) => {
                let Some(remote) = self.remote.clone().or_else(|| self.repo.default_remote()) else {
                    self.error("No server configured.");
                    return;
                };
                self.local.submit("Pushing tag", move |r| Msg::Done {
                    title: "Push tag".into(),
                    result: r.push_tag(&remote, &name).map(|_| OpOk::Message(format!("Pushed tag {name}"))).map_err(es),
                    net: false,
                });
            }
            Action::GetRevision(rev, path) => {
                self.dialog = Some(Dialog::Confirm {
                    title: format!("Get this revision of {path}?"),
                    body: "Your workspace copy will be replaced with this older version. If you had local edits, they are backed up to a shelf first.".into(),
                    action: Action::DoGetRevision(rev, path),
                });
            }
            Action::DoGetRevision(rev, path) => {
                self.local.submit("Getting revision", move |r| {
                    let res = (|| {
                        // Looked up here rather than taken from the screen, which can be a moment out of date.
                        let backup =
                            backup_local_edits(r, std::slice::from_ref(&path), &format!("before getting older revision of {path}"))?;
                        r.restore_file_from(&rev, &path).map_err(es)?;
                        Ok(OpOk::Message(match backup {
                            Some(_) => format!("Restored {path}. Your previous edits are in the Shelves tab (backups)."),
                            None => {
                                let label =
                                    r.cl_of(&rev).map(|(id, n)| crate::git::history::cl_label(n, &id)).unwrap_or_else(|_| rev.clone());
                                format!("Restored {path} from {label}.")
                            }
                        }))
                    })();
                    Msg::Done { title: "Get revision".into(), result: res, net: false }
                });
            }
            Action::SelectShelf(refname) => {
                if self.selected_shelf.as_ref() != Some(&refname) {
                    self.clear_diff();
                }
                self.selected_shelf = Some(refname.clone());
                if let Some(s) = self.shelves.iter().find(|s| s.refname == refname).cloned() {
                    if !self.shelf_files.contains_key(&s.id) {
                        self.local.submit("Loading shelf", move |r| Msg::ShelfFiles(s.id.clone(), r.shelf_files(&s).map_err(es)));
                    }
                }
            }
            Action::OpenUnshelve(shelf, preselect) => {
                let Some(files) = self.shelf_files.get(&shelf.id).cloned() else {
                    self.error("Still loading the shelf's files, try again in a moment.");
                    return;
                };
                let files = files
                    .iter()
                    .cloned()
                    .map(|f| {
                        let on = preselect.is_empty() || preselect.contains(&f.path);
                        (f, on)
                    })
                    .collect();
                self.dialog = Some(Dialog::Unshelve(UnshelveDlg { shelf, files }));
            }
            Action::ShareShelf(shelf) => {
                let Some(remote) = self.remote.clone().or_else(|| self.repo.default_remote()) else {
                    self.error("This repository has no server to share with.");
                    return;
                };
                self.local.submit("Sharing shelf", move |r| Msg::Done {
                    title: "Share shelf".into(),
                    result: r
                        .share_shelf(&shelf, &remote)
                        .map(|_| OpOk::Message("Shelf shared. Teammates will see it after their next refresh.".into()))
                        .map_err(es),
                    net: false,
                });
            }
            Action::UnshareShelf(shelf) => {
                let Some(remote) = shelf.remote.clone().or_else(|| self.remote.clone()).or_else(|| self.repo.default_remote()) else {
                    return;
                };
                self.local.submit("Removing shelf from server", move |r| Msg::Done {
                    title: "Unshare shelf".into(),
                    result: r
                        .unshare_shelf(&shelf.name, &remote)
                        .map(|_| OpOk::Message("Shelf removed from the server.".into()))
                        .map_err(es),
                    net: false,
                });
            }
            Action::DeleteShelf(shelf) => {
                let extra = if shelf.shared { "\n\nIt will also be removed from the server." } else { "" };
                self.dialog = Some(Dialog::Confirm {
                    title: "Delete shelf?".into(),
                    body: format!("\"{}\" will be deleted permanently.{extra}", first_line(&shelf.description)),
                    action: Action::DoDeleteShelf(shelf),
                });
            }
            Action::DoDeleteShelf(shelf) => {
                let remote = self.remote.clone().or_else(|| self.repo.default_remote());
                self.local.submit("Deleting shelf", move |r| {
                    let res = (|| {
                        if shelf.remote.is_some() {
                            // Someone else's shared shelf: just forget our copy.
                            r.delete_shelf(&shelf).map_err(es)?;
                            return Ok(OpOk::Message("Removed the shelf from your list.".into()));
                        }
                        if shelf.shared {
                            if let Some(rm) = &remote {
                                r.unshare_shelf(&shelf.name, rm).map_err(es)?;
                            }
                        }
                        r.delete_shelf(&shelf).map_err(es)?;
                        Ok(OpOk::Message("Shelf deleted.".into()))
                    })();
                    Msg::Done { title: "Delete shelf".into(), result: res, net: false }
                });
            }
            Action::StashApply(i, pop) => {
                self.local.submit("Applying stash", move |r| Msg::Done {
                    title: "Apply stash".into(),
                    result: r.stash_apply(i, pop).map(|_| OpOk::Message("Stash applied.".into())).map_err(es),
                    net: false,
                });
            }
            Action::StashDrop(i) => {
                self.local.submit("Dropping stash", move |r| Msg::Done {
                    title: "Drop stash".into(),
                    result: r.stash_drop(i).map(|_| OpOk::Message("Stash dropped.".into())).map_err(es),
                    net: false,
                });
            }
            Action::OpenSubmodule(path) => {
                self.app_requests.push(AppRequest::OpenRepo(self.repo.abs(&path), Some(self.repo.name())));
            }
            Action::UpdateSubmodules => {
                self.local.submit("Updating submodules", |r| Msg::Done {
                    title: "Update submodules".into(),
                    result: r.update_submodules_if_any().map(|_| OpOk::Message("Submodules updated.".into())).map_err(es),
                    net: false,
                });
            }
            Action::RemoveIndexLock => {
                self.dialog = Some(Dialog::Confirm {
                    title: "Remove Git's lock file?".into(),
                    body: "Git creates this file while it is working and removes it when it is done. Only remove it if nothing else is using this project right now: no other g4v window, terminal, game engine or sync tool that is still running a Git command. Removing it while one is still working can damage the project's index.\n\nIf you're not sure, wait a minute and press Refresh first.".into(),
                    action: Action::DoRemoveIndexLock,
                });
            }
            Action::DoRemoveIndexLock => {
                match self.repo.remove_index_lock() {
                    Ok(()) => self.toast("Removed the stale lock file."),
                    Err(e) => self.error(format!("Could not remove the lock: {e}")),
                }
                self.request_status(true);
            }
            Action::SetupIdentity => {
                let id = self.repo.identity();
                self.dialog =
                    Some(Dialog::Identity { name: id.name.unwrap_or_default(), email: id.email.unwrap_or_default(), global: true });
            }
            Action::AddRemote => {
                self.dialog = Some(Dialog::AddRemote { name: "origin".into(), url: String::new() });
            }
            Action::TrackLfs(pattern) => {
                self.local.submit("Tracking with LFS", move |r| Msg::Done {
                    title: "Track with LFS".into(),
                    result: r
                        .lfs_track(&pattern, false)
                        .map(|_| {
                            OpOk::Message(format!("{pattern} files are now stored with Git LFS. Remember to submit .gitattributes too."))
                        })
                        .map_err(es),
                    net: false,
                });
            }
            Action::DoSubmit { mut paths, message, cl_id, push, unlock } => {
                let mode = settings.pull_mode;
                // A moved file is two changes: the new path appears and the old one disappears. Submit both.
                let old_paths: Vec<String> = paths.iter().filter_map(|p| self.file(p).and_then(|f| f.orig_path.clone())).collect();
                paths.extend(old_paths);
                self.local.submit("Submitting", move |r| {
                    let res = (|| {
                        // Recheck at execution time: the dialog may have been open while a teammate checked out a file.
                        let lockable = r.lockable_paths(&paths).map_err(es)?;
                        if push && !lockable.is_empty() {
                            let locks = r.lfs_locks().map_err(|e| {
                                format!(
                                    "Could not verify file locks: {e}. Reconnect and try again, or untick Upload to save a local commit."
                                )
                            })?;
                            let selected: HashSet<&str> = paths.iter().map(String::as_str).collect();
                            if let Some(lock) = locks.iter().find(|l| !l.ours && selected.contains(l.path.as_str())) {
                                return Err(format!(
                                    "{} is checked out by {}. Untick that file or ask them to release it.",
                                    lock.path, lock.owner
                                ));
                            }
                        }
                        let commit = r.commit_paths(&paths, &message).map_err(es)?;
                        let push_res = push.then(|| push_with_retry(r, mode));
                        let (unlocked, unlock_failed) = match &push_res {
                            Some(Ok(_)) if !unlock.is_empty() => r.lfs_unlock(&unlock, false),
                            _ => (vec![], vec![]),
                        };
                        // Uploading may have rebased the commit onto newer work, so number what's at HEAD now.
                        let (commit, cl) = r.cl_of("HEAD").unwrap_or((commit, 0));
                        Ok(OpOk::Submitted { commit, cl, cl_id, push: push_res, unlocked, unlock_failed })
                    })();
                    Msg::Done { title: "Submit".into(), result: res, net: true }
                });
                self.selection.clear();
            }
            Action::DoShelve { paths, description, revert, share, cl_id } => {
                let remote = self.remote.clone().or_else(|| self.repo.default_remote());
                self.local.submit("Shelving", move |r| {
                    let res = (|| {
                        let files = fresh_statuses(r, &paths)?;
                        let mut all = paths.clone();
                        all.extend(files.iter().filter_map(|f| f.orig_path.clone()));
                        let shelf = r.create_shelf(&all, &description).map_err(es)?;
                        if revert {
                            r.revert_files(&files).map_err(es)?;
                        }
                        let share = share.then(|| match &remote {
                            Some(rm) => r.share_shelf(&shelf, rm).map_err(es),
                            None => Err("no server configured".to_string()),
                        });
                        Ok(OpOk::Shelved { shelf, reverted: revert, share, cl_id })
                    })();
                    Msg::Done { title: "Shelve".into(), result: res, net: false }
                });
            }
            Action::RestoreWip(shelf) => {
                // Bring back work shelved when leaving this branch, then drop the shelf.
                self.local.submit("Restoring your work", move |r| {
                    let res = (|| {
                        // The shelf replaces whole files, so anything edited since coming back is backed up first.
                        let shelved: Vec<String> = r.shelf_files(&shelf).map_err(es)?.into_iter().map(|f| f.path).collect();
                        let backup = backup_local_edits(r, &shelved, "before restoring shelved work")?;
                        let written = r.unshelve(&shelf, None).map_err(es)?;
                        r.delete_shelf(&shelf).map_err(es)?;
                        Ok(OpOk::Unshelved { paths: written, description: shelf.display_name(), backup, new_changelist: false })
                    })();
                    Msg::Done { title: "Restore".into(), result: res, net: false }
                });
            }
            Action::DoUnshelve { shelf, paths } => {
                self.local.submit("Unshelving", move |r| {
                    let res = (|| {
                        let backup = backup_local_edits(r, &paths, &format!("before unshelving \"{}\"", first_line(&shelf.description)))?;
                        let written = r.unshelve(&shelf, Some(&paths)).map_err(es)?;
                        Ok(OpOk::Unshelved { paths: written, description: shelf.display_name(), backup, new_changelist: true })
                    })();
                    Msg::Done { title: "Unshelve".into(), result: res, net: false }
                });
            }
            Action::DoRevert(paths) => {
                self.local.submit("Reverting", move |r| {
                    let res = (|| {
                        let files = fresh_statuses(r, &paths)?;
                        let n = files.len();
                        let mut all: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
                        all.extend(files.iter().filter_map(|f| f.orig_path.clone()));
                        let non_sub: Vec<String> = files.iter().filter(|f| !f.submodule).map(|f| f.path.clone()).collect();
                        let backup = if non_sub.is_empty() {
                            None
                        } else {
                            Some(r.create_shelf(&all, &format!("{BACKUP_TAG}before reverting {n} file(s)")).map_err(es)?)
                        };
                        r.revert_files(&files).map_err(es)?;
                        Ok(OpOk::Message(match backup {
                            Some(_) => format!("Reverted {n} file(s). A backup was saved in Shelves › Backups in case you need it."),
                            None => format!("Reverted {n} file(s)."),
                        }))
                    })();
                    Msg::Done { title: "Revert".into(), result: res, net: false }
                });
                self.selection.clear();
            }
        }
    }

    /// Runs a simple background operation that reports a message.
    pub fn run_simple(&mut self, label: &str, f: impl FnOnce(&Repo) -> crate::git::GitResult<String> + Send + 'static) {
        let title = label.to_string();
        self.local.submit(label, move |r| Msg::Done { title, result: f(r).map(OpOk::Message).map_err(es), net: false });
    }

    pub fn branch_name(&self) -> String {
        self.status.as_ref().and_then(|s| s.branch.head.clone()).unwrap_or_else(|| "(no branch)".into())
    }

    fn open_submit(&mut self, cl_id: u32, settings: &Settings) {
        if !self.identity.complete() {
            self.identity = self.repo.identity();
        }
        if !self.identity.complete() {
            self.handle(Action::SetupIdentity, settings);
            self.toast("Tell git who you are first — this is shown on every change you submit.");
            return;
        }
        if self.status.as_ref().is_some_and(|s| s.branch.head.is_none() && s.branch.oid.is_some()) {
            self.error("You're not on a branch (you are viewing an old version). Create a branch first so your work isn't lost.");
            self.handle(Action::NewBranch(None), settings);
            return;
        }
        if self.op() != RepoOp::None {
            self.error("Finish resolving the current conflicts first (Pending tab), then press Continue.");
            return;
        }
        let files: Vec<String> = self.cl_files(cl_id).iter().filter(|p| self.file(p).is_some()).cloned().collect();
        if files.is_empty() {
            self.error("This changelist has no changed files to submit.");
            return;
        }
        // A description typed earlier that didn't make it into a commit (cancelled, or the submit failed) comes back.
        let draft = self.submit_draft.as_ref().filter(|(id, text)| *id == cl_id && !text.trim().is_empty()).map(|(_, t)| t.clone());
        let message = draft.unwrap_or_else(|| {
            if cl_id == DEFAULT_ID {
                String::new()
            } else {
                self.changelists.get(cl_id).map(|c| c.description.clone()).unwrap_or_default()
            }
        });
        let has_remote = self.remote.is_some();
        let large = self.large_non_lfs(&files, settings.large_file_mb).into_iter().collect();
        self.dialog = Some(Dialog::Submit(SubmitDlg {
            cl_id,
            message,
            files: files.into_iter().map(|f| (f, true)).collect(),
            push: settings.push_on_submit && has_remote,
            unlock: settings.unlock_after_submit,
            large,
        }));
    }

    /// Shelves left behind when switching away from `branch`.
    pub fn wip_shelf_for_current(&self) -> Option<&Shelf> {
        let b = self.status.as_ref()?.branch.head.clone()?;
        let want = wip_description(&b);
        self.shelves.iter().find(|s| s.remote.is_none() && s.description == want)
    }
}

pub fn wip_description(branch: &str) -> String {
    format!("Work in progress on branch '{branch}'")
}

pub fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().trim().to_string()
}

/// Shelves the local changes to any of `paths` as an automatic backup. Returns the backup's name, if one was needed.
fn fresh_statuses(r: &Repo, paths: &[String]) -> Result<Vec<FileStatus>, String> {
    let wanted: HashSet<&str> = paths.iter().map(String::as_str).collect();
    Ok(r.status().map_err(es)?.files.into_iter().filter(|f| wanted.contains(f.path.as_str())).collect())
}

fn backup_local_edits(r: &Repo, paths: &[String], why: &str) -> Result<Option<String>, String> {
    let wanted: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut dirty: Vec<String> = r
        .status()
        .map_err(es)?
        .files
        .into_iter()
        .filter(|f| wanted.contains(f.path.as_str()) || f.orig_path.as_deref().is_some_and(|p| wanted.contains(p)))
        .flat_map(|f| std::iter::once(f.path).chain(f.orig_path))
        .collect();
    // Ignored files are invisible to status but can still be overwritten by an older revision or a shelf.
    if !paths.is_empty() {
        // check-ignore consumes literal filenames, but rejects Git's global literal-pathspec magic.
        let ignored = r
            .git()
            .args(["check-ignore", "-z", "--stdin"])
            .env("GIT_LITERAL_PATHSPECS", "0")
            .stdin_paths(paths)
            .ok_codes(&[0, 1])
            .run()
            .map_err(es)?;
        dirty.extend(crate::git::split_nul(&ignored.stdout).into_iter().filter(|p| r.abs(p).is_file() || r.abs(p).is_symlink()));
    }
    dirty.sort();
    dirty.dedup();
    if dirty.is_empty() {
        return Ok(None);
    }
    Ok(Some(r.create_shelf(&dirty, &format!("{BACKUP_TAG}{why}")).map_err(es)?.display_name()))
}

/// One message for a batch of per-file failures (instead of one pop-up per file).
fn failure_summary(what: &str, failed: &[(String, String)]) -> String {
    const SHOWN: usize = 5;
    let mut m = format!("{what} {} file(s):", failed.len());
    for (p, e) in failed.iter().take(SHOWN) {
        m.push_str(&format!("\n• {p}: {}", first_line(e)));
    }
    if failed.len() > SHOWN {
        m.push_str(&format!("\n…and {} more (see the Log tab).", failed.len() - SHOWN));
    }
    if let Some(h) = failed.first().and_then(|(_, e)| hint_for(e)) {
        m.push_str(&format!("\n\n{h}"));
    }
    m
}

/// A plain-language explanation for the git errors artists run into most.
pub fn hint_for(err: &str) -> Option<&'static str> {
    let e = err.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| e.contains(n));
    if has(&["index.lock"]) {
        Some("Another Git program is working in this project. Wait for it to finish and try again; if it crashed, g4v will offer to remove its lock file.")
    } else if has(&[
        "permission denied",
        "unable to unlink",
        "used by another process",
        "resource busy",
        "unable to create file",
        "access is denied",
    ]) {
        Some("A file couldn't be changed, most likely because it is open in another program (Photoshop, Maya, Unreal…). Close it there and try again.")
    } else if has(&["no space left", "not enough space", "disk full", "disk quota"]) {
        Some("The disk is full. Free up some space, refresh the project, and check the operation's result before trying again. Automatic backups are in Shelves.")
    } else if has(&["filename too long", "file name too long"]) {
        Some("A file's folder path is too long for this computer. Move the project closer to the drive root (for example D:\\proj) or shorten the folder names.")
    } else if has(&[
        "authentication failed",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "http 401",
        "403",
    ]) {
        Some("The server didn't accept your sign-in. Sign in again when the login window appears, or ask your admin to check your access.")
    } else if has(&[
        "could not resolve host",
        "unable to access",
        "timed out",
        "connection refused",
        "network is unreachable",
        "could not read from remote",
    ]) {
        Some("The server can't be reached. Check your internet or VPN connection; your work is safe on this computer in the meantime.")
    } else if has(&["exceeds github's file size limit", "gh001", "file size limit", "large files detected"]) {
        Some("The server refuses files this large. Store that file type with Git LFS (right-click the file) and submit again.")
    } else if has(&["locked by", "cannot update locked files", "lock exists", "already locked"]) {
        Some("Someone else has the file checked out. See the Locks tab for who, and ask them to release it.")
    } else if has(&["lfs: ", "batch response", "smudge filter lfs failed", "git-lfs"]) {
        Some("Git LFS (the large-file storage) reported a problem. Check that you're online and signed in, then try again.")
    } else {
        None
    }
}

/// Push; if the server has newer commits, get latest and push again once.
fn push_with_retry(r: &Repo, mode: PullMode) -> Result<String, String> {
    match r.push_current().map_err(es)? {
        PushOutcome::Pushed => Ok("Uploaded to the server.".into()),
        PushOutcome::Rejected(_) => {
            let dirty: Vec<String> = r.status().map(|s| s.files.into_iter().map(|f| f.path).collect()).unwrap_or_default();
            match r.sync(mode, &dirty).map_err(es)? {
                SyncOutcome::Updated { .. } | SyncOutcome::UpToDate => match r.push_current().map_err(es)? {
                    PushOutcome::Pushed => Ok("The server had newer changes; got latest and uploaded.".into()),
                    PushOutcome::Rejected(e) => Err(format!("The server keeps rejecting the upload: {e}")),
                },
                SyncOutcome::Conflicts => {
                    Err("Someone else changed the same files. Resolve the conflicts in the Pending tab, press Continue, then Push.".into())
                }
                SyncOutcome::Blocked { overlap } => Err(format!(
                    "The server has newer versions of files you are still editing ({}). Get Latest, then Push.",
                    overlap.join(", ")
                )),
                SyncOutcome::NoUpstream => Err("This branch has no upstream on the server.".into()),
            }
        }
    }
}

const MAX_IMAGE_SIDE: u32 = 2048;
/// Images larger than this are described by size only instead of being decoded for a preview.
const MAX_PREVIEW_BYTES: u64 = 256 * 1024 * 1024;

fn decode_image(bytes: &[u8]) -> Option<egui::ColorImage> {
    // Compressed size is not a bound on decoded memory (e.g. a huge single-colour TIFF).
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_PREVIEW_BYTES);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let img =
        if img.width() > MAX_IMAGE_SIDE || img.height() > MAX_IMAGE_SIDE { img.thumbnail(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE) } else { img };
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

#[allow(clippy::type_complexity)]
fn load_diff(r: &Repo, target: &DiffTarget, file: Option<&FileStatus>, has_head: bool) -> Result<DiffData, String> {
    // (old rev, old path, new rev or None for working copy, new path)
    let (title, diff, old, new): (String, FileDiff, Option<(String, String)>, Option<(Option<String>, String)>) = match target {
        DiffTarget::Working { path } => {
            let untracked = file.is_some_and(|f| f.untracked);
            let orig = file.and_then(|f| f.orig_path.clone());
            let d = r.diff_worktree(path, orig.as_deref(), untracked, has_head).map_err(es)?;
            let in_head = has_head && file.is_none_or(|f| f.in_head() || f.orig_path.is_some());
            let deleted = file.is_some_and(|f| f.kind == ChangeKind::Delete);
            (
                format!("{path} — workspace vs. latest submitted"),
                d,
                in_head.then(|| ("HEAD".to_string(), orig.unwrap_or_else(|| path.clone()))),
                (!deleted).then(|| (None, path.clone())),
            )
        }
        DiffTarget::Commit { id, parent, file } | DiffTarget::Shelf { id, base: parent, file } => {
            let d = r.diff_revs(parent.as_deref(), id, &file.path, file.old_path.as_deref()).map_err(es)?;
            let old = (file.status != 'A')
                .then(|| parent.clone())
                .flatten()
                .map(|p| (p, file.old_path.clone().unwrap_or_else(|| file.path.clone())));
            let new = (file.status != 'D').then(|| (Some(id.clone()), file.path.clone()));
            let what = if matches!(target, DiffTarget::Shelf { .. }) {
                "shelved version".to_string()
            } else {
                r.cl_of(id).map(|(full, n)| crate::git::history::cl_label(n, &full)).unwrap_or_else(|_| id[..id.len().min(8)].to_string())
            };
            (format!("{} — {what}", file.path), d, old, new)
        }
        DiffTarget::Incoming { path } => {
            let d = r.diff_revs(Some("HEAD"), "@{upstream}", path, None).map_err(es)?;
            (
                format!("{path} — your version vs. newer version on the server"),
                d,
                Some(("HEAD".into(), path.clone())),
                Some((Some("@{upstream}".into()), path.clone())),
            )
        }
    };
    let path = match target {
        DiffTarget::Working { path } | DiffTarget::Incoming { path } => path.clone(),
        DiffTarget::Commit { file, .. } | DiffTarget::Shelf { file, .. } => file.path.clone(),
    };
    let mut data = DiffData { title, path: path.clone(), diff, old_image: None, new_image: None, old_size: None, new_size: None };
    let want_sizes = is_image_path(&path) || data.diff.binary || data.diff.lfs.is_some();
    if want_sizes {
        // Sizes come from the file system, the LFS pointer or git's object header: a 4 GB video is never read into
        // memory (or downloaded from the LFS server) just to say how big it is.
        let (old_ptr, new_ptr) = data.diff.lfs.clone().unwrap_or((None, None));
        data.old_size = old.as_ref().and_then(|(rev, p)| old_ptr.map(|l| l.size).or_else(|| r.blob_size(rev, p)));
        data.new_size = new.as_ref().and_then(|(rev, p)| match rev {
            Some(rev) => new_ptr.map(|l| l.size).or_else(|| r.blob_size(rev, p)),
            None => r.file_size(p),
        });
        if is_image_path(&path) {
            let small = |size: Option<u64>| size.is_some_and(|s| s <= MAX_PREVIEW_BYTES);
            if small(data.old_size) {
                data.old_image = old.and_then(|(rev, p)| r.blob_bytes(&rev, &p).ok()).as_deref().and_then(decode_image);
            }
            if small(data.new_size) {
                let bytes = new.and_then(|(rev, p)| match rev {
                    Some(rev) => r.blob_bytes(&rev, &p).ok(),
                    None => std::fs::read(r.abs(&p)).ok(),
                });
                data.new_image = bytes.as_deref().and_then(decode_image);
            }
        }
    }
    Ok(data)
}
