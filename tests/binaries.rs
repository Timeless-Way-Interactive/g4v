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
    // c1 -> c2 doesn't change deps/: no post-sync.
    get_latest(&w, &r, &cfg).unwrap();
    assert!(!r.abs("ran.txt").exists());

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
