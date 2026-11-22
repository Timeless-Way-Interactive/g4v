//! End-to-end tests of Get Latest with binaries against real git repositories: a bare "server" whose CI has
//! written build notes, releases served from a folder, and a teammate's clone.

use g4v::binaries::config::BinariesConfig;
use g4v::binaries::source::DirSource;
use g4v::binaries::sync::{self, BinSyncOutcome, Phase, Txn};
use g4v::binaries::{install, sha256_hex};
use g4v::git::ops::{PullMode, SyncOutcome};
use g4v::git::{CmdLog, Repo};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

static ENV: Once = Once::new();

fn isolate() {
    ENV.call_once(|| {
        let dir = std::env::temp_dir().join(format!("g4v-bintest-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("gitconfig");
        std::fs::write(
            &cfg,
            "[user]\n\tname = Tester\n\temail = tester@example.com\n[init]\n\tdefaultBranch = main\n[core]\n\tautocrlf = false\n[commit]\n\tgpgsign = false\n[protocol \"file\"]\n\tallow = always\n",
        )
        .unwrap();
        // SAFETY: set once, before any test spawns git, and never modified afterwards.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", &cfg);
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        }
    });
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").current_dir(dir).args(args).output().expect("git runs");
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const POST: &str = if cfg!(windows) { "post.cmd" } else { "post.sh" };

fn config(post_sync: bool) -> BinariesConfig {
    let post = if post_sync { format!("post_sync = {{ run = [\"{POST}\"], when_changed = [\"deps\"] }}\n") } else { String::new() };
    BinariesConfig::parse(&format!(
        "notes_ref = \"refs/notes/binaries\"\nrelease_repo = \"o/r\"\ninstall_roots = [\"bin\"]\nmarker = \"bin/.binkey\"\n\
         source_paths = [\"src\"]\nlaunch = [\"bin/app.exe\"]\n{post}"
    ))
    .unwrap()
}

struct World {
    tmp: tempfile::TempDir,
    seed: PathBuf,
    releases: PathBuf,
    cache: PathBuf,
    /// Commits, oldest first: c1 (code k1), c2 (content only, k1), c3 (code k2).
    c: Vec<String>,
}

impl World {
    fn new() -> World {
        isolate();
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server.git");
        git(tmp.path(), &["init", "--bare", "-b", "main", server.to_str().unwrap()]);
        let seed = tmp.path().join("seed");
        git(tmp.path(), &["clone", server.to_str().unwrap(), seed.to_str().unwrap()]);
        let w = |rel: &str, s: &str| {
            let p = seed.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, s).unwrap();
        };
        w(".gitignore", "bin/\n");
        w("src/main.c", "v1\n");
        w("deps/version", "1\n");
        w("content.txt", "a\n");
        w(
            POST,
            if cfg!(windows) {
                "@echo ran>> ran.txt\r\n@if exist fail.flag exit /b 3\r\n"
            } else {
                "echo ran >> ran.txt\n[ -f fail.flag ] && exit 3\nexit 0\n"
            },
        );
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-m", "c1 code"]);
        let c1 = git(&seed, &["rev-parse", "HEAD"]);
        w("content.txt", "b\n");
        git(&seed, &["commit", "-am", "c2 content"]);
        let c2 = git(&seed, &["rev-parse", "HEAD"]);
        w("src/main.c", "v2\n");
        w("deps/version", "2\n");
        git(&seed, &["commit", "-am", "c3 code"]);
        let c3 = git(&seed, &["rev-parse", "HEAD"]);
        git(&seed, &["push", "-u", "origin", "main"]);
        let releases = tmp.path().join("releases");
        let cache = tmp.path().join("cache");
        World { seed, releases, cache, c: vec![c1, c2, c3], tmp }
    }

    /// Publishes release `bin-<key>` with these files and returns the note CI would write for it.
    fn publish(&self, key: &str, files: &[(&str, &str)]) -> String {
        let dir = self.releases.join(format!("bin-{key}"));
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("build.zip");
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let mut entries = Vec::new();
        for (path, body) in files {
            zw.start_file(*path, zip::write::SimpleFileOptions::default()).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
            entries.push(serde_json::json!({ "path": path, "bytes": body.len(), "sha256": sha256_hex(body.as_bytes()) }));
        }
        zw.finish().unwrap();
        let manifest = serde_json::to_vec(&serde_json::json!({ "key": key, "files": entries })).unwrap();
        std::fs::write(dir.join("build.manifest.json"), &manifest).unwrap();
        let zip_sha = sha256_hex(&std::fs::read(&zip_path).unwrap());
        serde_json::json!({
            "state": "ready", "key": key, "release": format!("bin-{key}"),
            "archive": { "name": "build.zip", "sha256": zip_sha },
            "manifest": { "name": "build.manifest.json", "sha256": sha256_hex(&manifest) },
        })
        .to_string()
    }

    /// Writes notes on the server, the way the index job and the builder do.
    fn note(&self, commit: &str, json: &str) {
        git(&self.seed, &["notes", "--ref=binaries", "add", "-f", "-m", json, commit]);
        git(&self.seed, &["push", "-f", "origin", "refs/notes/binaries"]);
    }

    fn building(&self, commit: &str, key: &str) {
        self.note(commit, &format!(r#"{{"state":"building","key":"{key}","release":"bin-{key}"}}"#));
    }

    fn clone(&self, name: &str) -> Repo {
        let p = self.tmp.path().join(name);
        git(self.tmp.path(), &["clone", self.tmp.path().join("server.git").to_str().unwrap(), p.to_str().unwrap()]);
        Repo::open(&p, CmdLog::default()).unwrap()
    }

    fn src(&self) -> DirSource {
        DirSource { dir: self.releases.clone() }
    }

    /// Standard world: k1 published and ready on c1 and c2, c3 still building k2.
    fn standard() -> World {
        let w = World::new();
        let n1 = w.publish("k1", &[("bin/app.exe", "app v1"), ("bin/lib.dll", "lib v1")]);
        w.note(&w.c[0], &n1);
        w.note(&w.c[1], &n1);
        w.building(&w.c[2], "k2");
        w
    }
}

fn get_latest(w: &World, r: &Repo, cfg: &BinariesConfig) -> Result<BinSyncOutcome, String> {
    let dirty: Vec<String> = r.status().unwrap().files.into_iter().map(|f| f.path).collect();
    sync::get_latest(r, cfg, &w.src(), &w.cache, PullMode::Rebase, &dirty, &|_| {})
}

fn head(r: &Repo) -> String {
    r.run(&["rev-parse", "HEAD"]).unwrap()
}

fn read(r: &Repo, rel: &str) -> String {
    std::fs::read_to_string(r.abs(rel)).unwrap_or_default()
}

#[test]
fn clone_at_a_building_tip_goes_back_to_the_newest_build_and_installs_it() {
    let w = World::standard();
    let r = w.clone("artist");
    assert_eq!(head(&r), w.c[2]);
    let cfg = config(false);

    let out = get_latest(&w, &r, &cfg).unwrap();
    assert_eq!(out, BinSyncOutcome::Synced { moved: -1, installed: Some("k1".into()), waiting: 1 });
    assert_eq!(head(&r), w.c[1], "content-only c2 shares k1, so it is the newest change with a build");
    assert_eq!(read(&r, "bin/app.exe"), "app v1");
    assert_eq!(read(&r, "bin/.binkey"), "k1");
    assert!(sync::pending_txn(&r).is_none());
    assert_eq!(read(&r, "src/main.c"), "v1\n", "code that needs the unbuilt k2 is not handed out");

    let st = sync::status(&r, &cfg);
    assert!(st.up_to_date);
    assert_eq!((st.waiting, st.building), (1, 1));
    assert_eq!(get_latest(&w, &r, &cfg).unwrap(), BinSyncOutcome::UpToDate { waiting: 1 });
    assert!(sync::launch_blocker(&r, &cfg).is_none());
}

#[test]
fn a_finished_build_moves_forward_and_cleans_up_only_its_own_files() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    get_latest(&w, &r, &cfg).unwrap();
    std::fs::write(r.abs("bin/local.pdb"), "my symbols").unwrap();

    let n2 = w.publish("k2", &[("bin/app.exe", "app v2"), ("bin/sub/new.dll", "new")]);
    w.note(&w.c[2], &n2);
    let out = get_latest(&w, &r, &cfg).unwrap();
    assert_eq!(out, BinSyncOutcome::Synced { moved: 1, installed: Some("k2".into()), waiting: 0 });
    assert_eq!(head(&r), w.c[2]);
    assert_eq!(read(&r, "bin/app.exe"), "app v2");
    assert_eq!(read(&r, "bin/sub/new.dll"), "new");
    assert!(!r.abs("bin/lib.dll").exists(), "a file the previous build installed and this one lacks is removed");
    assert_eq!(read(&r, "bin/local.pdb"), "my symbols", "files g4v didn't install are left alone");
    assert!(install::installed(r.root(), &cfg).is_some_and(|m| m.key == "k2"));
}

#[test]
fn a_failed_swap_puts_branch_files_and_marker_back() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    get_latest(&w, &r, &cfg).unwrap();
    let n2 = w.publish("k2", &[("bin/app.exe", "app v2"), ("bin/blocked", "x")]);
    w.note(&w.c[2], &n2);
    // A folder where the build needs a file: the swap fails halfway through.
    std::fs::create_dir_all(r.abs("bin/blocked")).unwrap();

    let err = get_latest(&w, &r, &cfg).unwrap_err();
    assert!(err.contains("previous version is back"), "{err}");
    assert_eq!(head(&r), w.c[1]);
    assert_eq!(read(&r, "bin/app.exe"), "app v1");
    assert_eq!(read(&r, "bin/lib.dll"), "lib v1");
    assert_eq!(read(&r, "bin/.binkey"), "k1");
    assert!(install::installed(r.root(), &cfg).is_some_and(|m| m.key == "k1"));
    assert!(sync::pending_txn(&r).is_none());
}

#[test]
fn an_interrupted_transaction_is_rolled_back_first() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    get_latest(&w, &r, &cfg).unwrap();
    // Simulate a crash right after the checkout moved: the log says Moving, HEAD is already at c3.
    git(r.root(), &["merge", "--ff-only", &w.c[2]]);
    let txn =
        Txn { phase: Phase::Moving, target: w.c[2].clone(), key: None, old_head: w.c[1].clone(), submodules: vec![], deps_ran: false };
    std::fs::write(r.meta_dir().join("binaries-txn.json"), serde_json::to_vec(&txn).unwrap()).unwrap();
    assert!(sync::launch_blocker(&r, &cfg).is_some(), "launch waits until the log is resolved");
    assert!(!sync::status(&r, &cfg).up_to_date);

    assert!(sync::recover(&r, &cfg, &|_| {}).unwrap());
    assert_eq!(head(&r), w.c[1]);
    assert!(sync::pending_txn(&r).is_none());
    assert!(sync::launch_blocker(&r, &cfg).is_none());
}

#[test]
fn programmer_workspaces_and_repos_without_notes_use_the_normal_get_latest() {
    let w = World::standard();
    let r = w.clone("programmer");
    let cfg = config(false);
    git(r.root(), &["reset", "--hard", &w.c[0]]);
    std::fs::write(r.abs("src/extra.c"), "new file\n").unwrap();
    match get_latest(&w, &r, &cfg).unwrap() {
        BinSyncOutcome::Normal { reason, sync } => {
            assert!(reason.contains("src"), "{reason}");
            assert_eq!(sync, SyncOutcome::Updated { commits: 2 });
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(head(&r), w.c[2], "programmers go to the tip and build themselves");
    assert!(!r.abs("bin/.binkey").exists());

    let w2 = World::new();
    let r2 = w2.clone("early");
    git(r2.root(), &["reset", "--hard", &w2.c[0]]);
    match get_latest(&w2, &r2, &cfg).unwrap() {
        BinSyncOutcome::Normal { reason, .. } => assert!(reason.contains("no build information"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_pruned_release_falls_back_to_an_older_build() {
    let w = World::standard();
    let n2 = w.publish("k2", &[("bin/app.exe", "app v2")]);
    w.note(&w.c[2], &n2);
    std::fs::remove_dir_all(w.releases.join("bin-k2")).unwrap();
    let r = w.clone("artist");
    let cfg = config(false);
    let out = get_latest(&w, &r, &cfg).unwrap();
    assert!(matches!(out, BinSyncOutcome::Synced { moved: -1, .. }), "{out:?}");
    assert_eq!(read(&r, "bin/.binkey"), "k1");
}

#[test]
fn a_manifest_that_writes_outside_its_install_roots_is_refused() {
    let w = World::new();
    let evil = w.publish("k1", &[("bin/app.exe", "app"), ("src/main.c", "pwned")]);
    w.note(&w.c[2], &evil);
    let r = w.clone("artist");
    let cfg = config(false);
    let err = get_latest(&w, &r, &cfg).unwrap_err();
    assert!(err.contains("outside its install folders"), "{err}");
    assert_eq!(read(&r, "src/main.c"), "v2\n");
    assert!(!r.abs("bin").exists(), "nothing was written");
    assert!(sync::pending_txn(&r).is_none());
}

#[test]
fn local_edits_that_the_move_would_overwrite_stop_before_anything_changes() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    // From c1, the newest build is c2, which changes content.txt — the file being edited.
    git(r.root(), &["reset", "--hard", &w.c[0]]);
    std::fs::write(r.abs("content.txt"), "my edit\n").unwrap();
    let out = get_latest(&w, &r, &cfg).unwrap();
    assert_eq!(out, BinSyncOutcome::Blocked { overlap: vec!["content.txt".into()] });
    assert_eq!(head(&r), w.c[0]);
    assert!(!r.abs("bin").exists());
}

#[test]
fn post_sync_runs_when_its_paths_change_and_a_failure_blocks_launch() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(true);
    git(r.root(), &["reset", "--hard", &w.c[0]]);
    // A first install runs setup even though nothing "changed": it has never run in this workspace.
    get_latest(&w, &r, &cfg).unwrap();
    assert_eq!(read(&r, "ran.txt").lines().count(), 1);
    assert!(sync::setup_current(&r, &cfg));
    // Up to date, same deps/: not again.
    get_latest(&w, &r, &cfg).unwrap();
    assert_eq!(read(&r, "ran.txt").lines().count(), 1);

    // c2 -> c3 changes deps/, and the post-sync step fails both forwards and back.
    let n2 = w.publish("k2", &[("bin/app.exe", "app v2")]);
    w.note(&w.c[2], &n2);
    std::fs::write(r.abs("fail.flag"), "").unwrap();
    let err = get_latest(&w, &r, &cfg).unwrap_err();
    assert!(err.contains("post-sync step failed"), "{err}");
    assert_eq!(head(&r), w.c[1], "branch and files are back");
    assert_eq!(read(&r, "bin/app.exe"), "app v1");
    assert_eq!(sync::pending_txn(&r).map(|t| t.phase), Some(Phase::DepsIncomplete));
    assert!(sync::launch_blocker(&r, &cfg).is_some());

    // Once it can succeed, the next Get Latest finishes the rollback and then syncs.
    std::fs::remove_file(r.abs("fail.flag")).unwrap();
    let out = get_latest(&w, &r, &cfg).unwrap();
    assert!(matches!(out, BinSyncOutcome::Synced { moved: 1, .. }), "{out:?}");
    assert_eq!(read(&r, "bin/app.exe"), "app v2");
    assert!(read(&r, "ran.txt").lines().count() >= 3);
    assert!(sync::launch_blocker(&r, &cfg).is_none());
}

#[test]
fn a_clean_submodule_with_local_commits_counts_as_a_programmer_workspace() {
    let w = World::standard();
    // A library repo, pinned as submodule "lib" in a new superproject commit with a ready build.
    let lib_server = w.tmp.path().join("lib.git");
    git(w.tmp.path(), &["init", "--bare", "-b", "main", lib_server.to_str().unwrap()]);
    let lib_seed = w.tmp.path().join("lib-seed");
    git(w.tmp.path(), &["clone", lib_server.to_str().unwrap(), lib_seed.to_str().unwrap()]);
    std::fs::write(
        lib_seed.join("lib.c"),
        "1
",
    )
    .unwrap();
    git(&lib_seed, &["add", "-A"]);
    git(&lib_seed, &["commit", "-m", "lib"]);
    git(&lib_seed, &["push", "-u", "origin", "main"]);
    git(&w.seed, &["submodule", "add", lib_server.to_str().unwrap(), "lib"]);
    git(&w.seed, &["commit", "-m", "add lib"]);
    git(&w.seed, &["push"]);
    let c4 = git(&w.seed, &["rev-parse", "HEAD"]);
    let n = w.publish("k4", &[("bin/app.exe", "app v4")]);
    w.note(&c4, &n);

    let r = w.clone("dev");
    git(r.root(), &["submodule", "update", "--init"]);
    let cfg = config(false);
    assert!(matches!(get_latest(&w, &r, &cfg).unwrap(), BinSyncOutcome::Synced { .. }));
    // A clean submodule, three commits past its pin: nothing in the superproject's file list shows it.
    let lib = r.abs("lib");
    for i in 0..3 {
        std::fs::write(
            lib.join("lib.c"),
            format!(
                "local {i}
"
            ),
        )
        .unwrap();
        git(&lib, &["commit", "-qam", "local work"]);
    }
    match get_latest(&w, &r, &cfg).unwrap() {
        BinSyncOutcome::Normal { reason, .. } => assert!(reason.contains("lib"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

/// The real GitHub path: tag lookup, asset id, redirected download. Network, so opt-in:
/// `cargo test --test binaries -- --ignored github`.
#[test]
#[ignore]
fn github_source_downloads_a_public_release_asset() {
    use g4v::binaries::source::{AssetSource, FetchError, GitHubSource};
    let src = GitHubSource { api: "https://api.github.com".into(), repo: "BurntSushi/ripgrep".into(), token: None };
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("asset");
    src.fetch("14.1.0", "ripgrep-14.1.0-x86_64-pc-windows-msvc.zip.sha256", &dest, &|_, _| {}).unwrap();
    let text = std::fs::read_to_string(&dest).unwrap();
    assert!(text.contains("ripgrep-14.1.0-x86_64-pc-windows-msvc.zip"), "{text}");
    assert!(matches!(src.fetch("14.1.0", "no-such-asset", &dest, &|_, _| {}), Err(FetchError::NotFound)));
    assert!(matches!(src.fetch("no-such-tag-g4v", "x", &dest, &|_, _| {}), Err(FetchError::NotFound)));
}

/// Review finding: a rollback that is itself interrupted must be safe to run again. The first attempt used to move
/// the backups back (consuming them); a retry then saw neither a backup nor a staged copy and deleted the editor
/// the first attempt had just restored.
#[test]
fn an_interrupted_rollback_can_run_again_without_deleting_the_restored_build() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    get_latest(&w, &r, &cfg).unwrap(); // k1 installed at c2
    let n2 = w.publish("k2", &[("bin/app.exe", "app v2"), ("bin/lib.dll", "lib v2"), ("bin/new.dll", "new")]);
    let note: g4v::binaries::notes::Note = serde_json::from_str(&n2).unwrap();
    let work = r.meta_dir().join("binaries");
    let (staging, backup) = (work.join("staging"), work.join("backup"));
    let staged = install::prepare(&cfg, &note, &w.src(), &w.cache, &staging, &|_| {}).unwrap();
    install::swap(r.root(), &cfg, &staged, &backup, &|_| {}).unwrap();
    assert_eq!(read(&r, "bin/app.exe"), "app v2");

    // Attempt 1 restores the files, then "crashes" before the transaction is cleared. Attempt 2 runs it again.
    for attempt in 1..=2 {
        install::restore(r.root(), &cfg, Some(&staged.manifest), &staging, &backup).unwrap();
        assert_eq!(read(&r, "bin/app.exe"), "app v1", "attempt {attempt}");
        assert_eq!(read(&r, "bin/lib.dll"), "lib v1", "attempt {attempt}");
        assert!(!r.abs("bin/new.dll").exists(), "attempt {attempt}: a file only the new build had is gone");
        assert!(install::installed(r.root(), &cfg).is_some_and(|m| m.key == "k1"), "attempt {attempt}");
    }
    assert!(backup.exists(), "the backup stays until the whole rollback commits");
}

/// Review finding: Launch stayed enabled with a required DLL missing.
#[test]
fn a_damaged_install_blocks_launch_and_get_latest_repairs_it() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    get_latest(&w, &r, &cfg).unwrap();
    assert!(sync::launch_check(&r, &cfg).is_ok());
    std::fs::remove_file(r.abs("bin/lib.dll")).unwrap();
    let p = sync::launch_check(&r, &cfg).unwrap_err();
    assert_eq!(p.fix, sync::LaunchFix::Repair, "{}", p.message);
    get_latest(&w, &r, &cfg).unwrap();
    assert_eq!(read(&r, "bin/lib.dll"), "lib v1");
    assert!(sync::launch_check(&r, &cfg).is_ok());
}

/// Launch is refused when the installed build isn't the one the checked-out version needs.
#[test]
fn launch_refuses_an_editor_built_for_another_version() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    get_latest(&w, &r, &cfg).unwrap(); // k1 at c2
    let n2 = w.publish("k2", &[("bin/app.exe", "app v2")]);
    w.note(&w.c[2], &n2);
    r.fetch().unwrap();
    g4v::binaries::notes::fetch(&r, "origin", "refs/notes/binaries").unwrap();
    // A plain git pull to c3 leaves k1 installed.
    git(r.root(), &["merge", "--ff-only", &w.c[2]]);
    let p = sync::launch_check(&r, &cfg).unwrap_err();
    assert_eq!(p.fix, sync::LaunchFix::GetLatest, "{}", p.message);
}

/// Review finding: setup that never succeeded for this version blocks Launch until Run setup works.
#[test]
fn launch_waits_for_setup_and_run_setup_finishes_it() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(true);
    std::fs::write(r.abs("fail.flag"), "").unwrap();
    assert!(get_latest(&w, &r, &cfg).is_err());
    std::fs::remove_file(r.abs("fail.flag")).unwrap();
    // The failed Get Latest rolled back; a successful one leaves setup done.
    get_latest(&w, &r, &cfg).unwrap();
    assert!(sync::launch_check(&r, &cfg).is_ok());
    // Something deletes the record (or a run failed later): Launch says so and Run setup fixes it.
    std::fs::remove_file(r.meta_dir().join("binaries-setup.json")).unwrap();
    assert_eq!(sync::launch_check(&r, &cfg).unwrap_err().fix, sync::LaunchFix::RunSetup);
    sync::run_setup(&r, &cfg, &|_| {}).unwrap();
    assert!(sync::launch_check(&r, &cfg).is_ok());
}

/// Review finding: a second g4v must not recover or install while the first is mid-install.
#[test]
fn a_second_instance_waits_for_the_workspace_lock() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    let held = sync::lock_workspace(&r).unwrap();
    let err = get_latest(&w, &r, &cfg).unwrap_err();
    assert!(err.contains("already updating"), "{err}");
    assert!(sync::recover(&r, &cfg, &|_| {}).is_err());
    assert!(sync::run_setup(&r, &cfg, &|_| {}).is_err());
    assert!(sync::locked_by_other(&r));
    drop(held);
    assert!(!sync::locked_by_other(&r));
    get_latest(&w, &r, &cfg).unwrap();
}

#[test]
fn launcher_recovery_precedes_get_latest_and_launch() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    let old_head = head(&r);
    for path in [r.meta_dir().join("launcher-install"), r.abs(".binwork-0123456789abcdef/backup")] {
        std::fs::create_dir_all(&path).unwrap();
        assert!(get_latest(&w, &r, &cfg).unwrap_err().contains("Launch.bat"));
        assert!(sync::launch_check(&r, &cfg).unwrap_err().message.contains("Launch.bat"));
        assert_eq!(head(&r), old_head);
        std::fs::remove_dir(&path).unwrap();
    }
    std::fs::create_dir(r.abs(".binwork-art-backup")).unwrap();
    get_latest(&w, &r, &cfg).unwrap();
    assert!(r.abs(".binwork-art-backup").is_dir());
}

/// Review finding: a private submodule (no access) must not block clone or Get Latest for everyone else.
#[test]
fn an_optional_submodule_without_access_never_blocks_clone_or_get_latest() {
    let w = World::standard();
    let mk = |name: &str| {
        let server = w.tmp.path().join(format!("{name}.git"));
        git(w.tmp.path(), &["init", "--bare", "-b", "main", server.to_str().unwrap()]);
        let seed = w.tmp.path().join(format!("{name}-seed"));
        git(w.tmp.path(), &["clone", server.to_str().unwrap(), seed.to_str().unwrap()]);
        std::fs::write(seed.join("f.txt"), name).unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-m", name]);
        git(&seed, &["push", "-u", "origin", "main"]);
        server
    };
    let public = mk("public");
    let private = mk("private");
    git(&w.seed, &["submodule", "add", public.to_str().unwrap(), "Public"]);
    git(&w.seed, &["submodule", "add", private.to_str().unwrap(), "Private"]);
    std::fs::create_dir_all(w.seed.join(".g4v")).unwrap();
    std::fs::write(w.seed.join(".g4v/submodules.toml"), "optional = [\"Private\"]\n").unwrap();
    git(&w.seed, &["add", "-A"]);
    git(&w.seed, &["commit", "-m", "submodules"]);
    git(&w.seed, &["push"]);
    // This teammate can't reach the private repository.
    std::fs::rename(&private, w.tmp.path().join("private-moved.git")).unwrap();

    let dest = w.tmp.path().join("teammate");
    g4v::git::ops::clone(w.tmp.path().join("server.git").to_str().unwrap(), &dest, &CmdLog::default()).unwrap();
    assert_eq!(std::fs::read_to_string(dest.join("Public/f.txt")).unwrap(), "public");
    assert!(!dest.join("Private/f.txt").exists());
    let r = Repo::open(&dest, CmdLog::default()).unwrap();
    r.update_submodules_if_any().unwrap();
}

/// Review finding: rollback's temporary file used a fixed name next to each binary and removed whatever was there.
#[test]
fn rollback_never_touches_a_users_file_next_to_a_binary() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    get_latest(&w, &r, &cfg).unwrap(); // k1
                                       // The old fixed temporary name, and the first name the new code tries, both already taken by the user.
    let sentinels = ["bin/app.exe.g4v-restore".to_string(), format!("bin/.app.exe.g4v-restore-{}-0", std::process::id())];
    for s in &sentinels {
        std::fs::write(r.abs(s), "user data").unwrap();
    }
    let n2 = w.publish("k2", &[("bin/app.exe", "app v2")]);
    let note: g4v::binaries::notes::Note = serde_json::from_str(&n2).unwrap();
    let work = r.meta_dir().join("binaries");
    let (staging, backup) = (work.join("staging"), work.join("backup"));
    let staged = install::prepare(&cfg, &note, &w.src(), &w.cache, &staging, &|_| {}).unwrap();
    install::swap(r.root(), &cfg, &staged, &backup, &|_| {}).unwrap();
    install::restore(r.root(), &cfg, Some(&staged.manifest), &staging, &backup).unwrap();
    assert_eq!(read(&r, "bin/app.exe"), "app v1");
    for s in &sentinels {
        assert_eq!(read(&r, s), "user data", "{s} survived");
    }
    let stray: Vec<String> = std::fs::read_dir(r.abs("bin"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("g4v-restore") && !sentinels.iter().any(|s| s.ends_with(n.as_str())))
        .collect();
    assert!(stray.is_empty(), "no temporary files left: {stray:?}");
}

/// Review finding: with the executable and .binkey present but the manifest gone, Launch skipped every check.
#[test]
fn a_lost_install_record_still_counts_as_a_damaged_install() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(false);
    get_latest(&w, &r, &cfg).unwrap();
    std::fs::remove_file(r.abs("bin/.binmanifest.json")).unwrap();
    assert_eq!(sync::launch_check(&r, &cfg).unwrap_err().fix, sync::LaunchFix::Repair);
    get_latest(&w, &r, &cfg).unwrap();
    assert!(sync::launch_check(&r, &cfg).is_ok());
    // The other record lost instead: the same.
    std::fs::remove_file(r.abs("bin/.binkey")).unwrap();
    assert_eq!(sync::launch_check(&r, &cfg).unwrap_err().fix, sync::LaunchFix::Repair);
}

/// Review finding: a changed setup command at the same engine pin must run again.
#[test]
fn a_changed_setup_command_runs_again_at_the_same_engine_pin() {
    let w = World::standard();
    let r = w.clone("artist");
    let cfg = config(true);
    get_latest(&w, &r, &cfg).unwrap();
    assert!(sync::setup_current(&r, &cfg));
    let mut changed = cfg.clone();
    changed.post_sync.as_mut().unwrap().run.push("--extra".into());
    assert!(!sync::setup_current(&r, &changed), "new arguments: setup is due again");
    let mut watched = cfg.clone();
    watched.post_sync.as_mut().unwrap().when_changed.push("src".into());
    assert!(!sync::setup_current(&r, &watched), "new watched paths: setup is due again");
    sync::run_setup(&r, &changed, &|_| {}).unwrap();
    assert!(sync::setup_current(&r, &changed));
}

/// Review finding: Launch.bat's PowerShell installer and g4v must exclude each other. Both lock the same file the
/// same way (an OS byte-range lock on `<git dir>\g4v\binaries.lock`).
#[cfg(windows)]
#[test]
fn launch_bat_installer_and_g4v_share_one_lock() {
    use std::io::BufRead;
    let w = World::standard();
    let r = w.clone("artist");
    std::fs::create_dir_all(r.meta_dir()).unwrap();
    let path = r.meta_dir().join("binaries.lock").display().to_string().replace('\'', "''");
    let ps = |script: String| {
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    // PowerShell holds it: g4v sees it taken.
    let mut child = ps(format!(
        "$fs=[IO.File]::Open('{path}','OpenOrCreate','ReadWrite','ReadWrite'); $fs.Lock(0,[int64]::MaxValue); 'held'; Start-Sleep 3; $fs.Dispose()"
    ));
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "held");
    assert!(sync::locked_by_other(&r));
    assert!(get_latest(&w, &r, &config(false)).unwrap_err().contains("already updating"));
    child.wait().unwrap();
    assert!(!sync::locked_by_other(&r));
    // g4v holds it: PowerShell is refused.
    let held = sync::lock_workspace(&r).unwrap();
    let out = ps(format!(
        "$fs=[IO.File]::Open('{path}','OpenOrCreate','ReadWrite','ReadWrite'); try {{ $fs.Lock(0,[int64]::MaxValue); 'FREE' }} catch {{ 'LOCKED' }} finally {{ $fs.Dispose() }}"
    ))
    .wait_with_output()
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "LOCKED");
    drop(held);
}
