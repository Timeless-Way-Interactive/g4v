# g4v

**A P4V-style desktop client for Git, written in Rust.** It's built for artists, designers and anyone else who wants
version control to *just work*. You get pending changelists, shelves, file check-out (Git LFS locks), out-of-date
indicators and a Get Latest you can trust, with the rest of Git kept out of sight.

![Pending changelists with an image diff](docs/pending.png)

## Why

Git is powerful, but the mistakes it allows are easy to make and scary for non-programmers: committing the wrong
files, losing work to a reset, editing a binary file that someone else is also changing, forgetting to pull, or getting
stuck in a half-finished rebase. g4v uses the Perforce workflow that many studios already know, and every
destructive step is guarded or backed up.

| You know this from P4V        | In g4v                                                    | Under the hood (plain Git)                                         |
|-------------------------------|-----------------------------------------------------------|--------------------------------------------------------------------|
| Workspace / depot tree        | **Workspace** tree with status overlays                   | `git ls-files`, `git status`                                        |
| Get Latest Revision           | **Get Latest** (⬇ badge shows how many changes are new)  | `fetch` + fast-forward / rebase / merge, with safety checks         |
| *Not at latest revision*      | Yellow **⬇** on files and folders; red **⚠** if you edit them too | periodic `git fetch` + `git diff HEAD...@{upstream}`      |
| Check Out (exclusive lock)    | **Check Out** / **Release**, 🔒 indicators with lock owner     | `git lfs lock` / `git lfs unlock`                                   |
| Pending changelists           | **Pending** tab: Default + numbered changelists, drag & drop | local metadata in `.git/g4v/changelists.json`                   |
| Submit                        | **Submit**: commit exactly those files and upload them   | `git commit --only <paths>` + `git push` (auto-integrates if rejected) |
| Shelve / Unshelve             | **Shelve** any files and optionally **share** them with the team | commit objects under `refs/g4v/shelves/*` (pushable)        |
| Revert                        | **Revert** (an automatic backup shelf is always made first) | `git restore`                                                    |
| Resolve                       | **Keep mine / Take theirs / Merge tool / Mark resolved**  | `checkout --ours/--theirs`, with rebase sides already swapped for you |
| Submitted changelists         | **History** tab with **CL #** numbers, branch graph, details and diffs | `git log`, `git show`; CL # = first-parent position  |
| Streams                       | **Branches** panel and toolbar branch picker              | `git switch`, `git merge`                                          |
| `p4 opened -a`                | **Locks** tab: who has what checked out                   | `git lfs locks --verify`                                           |

Features that have no P4V equivalent follow SourceTree's lead: **submodules open in their own tabs**, plus tags,
remotes, stashes, cherry-pick and "back out this change" (revert commit).

## Features

* **Workspace tree** that stays fast in very large repos (only visible rows are laid out). Filters: *changed by me*,
  *out of date*, *checked out*. Files that are new on the server show up greyed out, so you can see what's coming.
* **Pending changelists** that work like Perforce: create, rename, delete, move files by drag & drop or right-click.
  Files you've locked but not changed show as *checked out, unchanged* so you remember to release them.
* **Submit dialog with guard rails:**
  * blocks files that someone else has locked
  * warns about large files that aren't stored in LFS, with a one-click *store `*.ext` with LFS* fix
  * warns about lockable files you edited without checking them out, with a one-click *check out now*
  * warns about files that have newer versions on the server
  * requires a description, and remembers the one you typed if you cancel
  * commits **only** the chosen files, even if something else was staged by another tool
  * if the push is rejected because the server moved on, g4v gets latest and pushes again automatically
  * releases your locks after a successful upload (optional)
* **Get Latest you can trust:** before touching anything, g4v checks which files you're editing that also changed on
  the server. If any do, it stops and offers to shelve or revert them, so local work is never overwritten. When there
  are no local commits it fast-forwards; otherwise it rebases or merges (your choice in Settings). Submodules are
  updated too.
* **Conflict resolution** for people who don't know what "ours" means: *Keep mine* and *Take theirs* do the right
  thing during both merges and rebases. *Continue* and *Abort* are always one click away.
* **Shelves:** shelve any subset of files without touching your workspace (or revert them in the same step), share them
  with teammates through the server, and unshelve into a new changelist. When you switch branches with uncommitted
  work, g4v can shelve it and offer it back when you return.
* **Automatic backups:** every revert, unshelve-over-local-edits and "get this revision" first saves a backup shelf
  (*Shelves › Show automatic backups*). Backups are pruned after a configurable retention period (30 days by default).
* **Image diffs:** PNG/JPG/TGA/TIFF/WebP/BMP/GIF are shown side by side, including files stored in LFS. Other binary
  files show their size change, and text files get a line-numbered diff.
* **Changelist numbers:** every submitted change gets a Perforce-style **CL #** (CL #1 is the first commit, and each
  change on a branch is one higher). It's worked out from the history itself, so it's the same for everyone and never
  changes once uploaded. A change you haven't uploaded yet can get a new number if Get Latest puts it on top of newer
  work, much like P4V renumbers on submit. Search History by `42`, `#42` or `CL 42`. The Git commit id is still in the
  tooltip, the right-click menu and the details panel.
* **History** as a table with **CL #**, **Description**, **Author** and **Date** columns. Click a column title to sort by
  it and click again to flip the direction. Columns can be resized and remember their widths. The branch graph shows
  in the default newest-first view, and **Reset view** clears the search and sorting and restores it. Branch/tag labels,
  commit details and per-file diffs are all there. Right-click a change to create a
  branch or tag, back it out, cherry-pick it, or undo your last unpushed commit while keeping its changes. You can also
  restore any older revision of a single file.
* **Banners for common mistakes:** detached HEAD, missing name/email, unpushed commits, branches that only exist
  locally, no server configured, a stale `index.lock` left by a crashed tool, LFS not installed, and edits to files
  someone else has locked.
* **Command log:** every git command g4v runs is listed with its duration and output, so there's nothing hidden.
* Multiple repositories in tabs, recent repositories, clone (with submodules and LFS), new repository, dark and light
  themes, and zoom.

![History with graph and diff](docs/history.png)

## How it stays reliable

* **It uses your installed `git`.** g4v doesn't reimplement Git. Every operation runs the real `git` command line,
  so LFS filters, hooks, credential managers, submodules and your config behave exactly as they would in a terminal.
* **No terminal prompts.** Editors and terminal prompts are disabled for every command (`GIT_EDITOR=true`,
  `GIT_TERMINAL_PROMPT=0`). Sign-in goes through your credential manager's own window. Network services,
  filters and hooks can still take time; the UI remains available while they run.
* **Paths are taken literally.** `GIT_LITERAL_PATHSPECS=1` is set and file lists go through
  `--pathspec-from-file`, so names like `hero [final] v2.psd` are never treated as patterns and huge selections never
  run into command-line length limits.
* **One operation at a time.** All work runs on background threads, so the UI never freezes, and operations on a
  repository are queued one after another, so two of them can never collide on Git's index lock. Network checks run
  separately so they never block local work.
* **No force push, no hard reset.** Those buttons don't exist. Branch delete is the safe kind unless you explicitly
  choose to discard unmerged work.
* **Recovery for staged and ignored work.** Replacing staged changes first preserves the index in an automatic
  shelf. Restoring a shelf or revision also backs up ignored files that would be overwritten. Taking a conflict
  side or aborting an operation preserves current file edits in an automatic shelf.
* **Fresh submit checks.** Lockable files are checked against the server when Submit executes. If locks cannot be
  verified, reconnect or untick Upload to make a local commit. Nested attribute files are supported.
* **Everything g4v stores is plain Git data** you can inspect or remove with standard tools:
  * changelists live in `.git/g4v/changelists.json` (written atomically; a corrupt file is moved aside, not
    silently discarded)
  * shelves are commits referenced by `refs/g4v/shelves/<user>/<timestamp>`; shared shelves are fetched into
    `refs/g4v/remote-shelves/<remote>/…`
  * `git for-each-ref refs/g4v` lists them all

## Getting started

Requirements:

* Git 2.25 or newer ([git-scm.com](https://git-scm.com))
* [Git LFS](https://git-lfs.com), for large files and locking. Locking also needs a server that supports the LFS
  locking API (GitHub, GitLab, Bitbucket, Gitea and others).

Build and run:

```bash
cargo run --release
```

```bash
cargo run --release -- path/to/repo another/repo
```

### Windows installer

```powershell
powershell -ExecutionPolicy Bypass -File installer\build.ps1
```

This produces `dist\g4v-setup-<version>.exe`, a single self-contained installer. It installs per user into
`%LOCALAPPDATA%\Programs\g4v` (no administrator rights), adds Start menu and desktop shortcuts, and registers g4v in
*Settings › Apps* so it can be uninstalled normally. Unattended use: `g4v-setup.exe --silent [--dir PATH]` and
`uninstall.exe --uninstall --silent`. Uninstalling never touches your repositories or g4v settings.

**The installer includes Git.** Nobody needs to install Git, Git LFS or a credential manager first. `build.ps1`
downloads MinGit (Git for Windows' build for applications, which includes Git Credential Manager) and Git LFS,
refuses either unless its SHA-256 matches `installer\git-bundle.json`, and appends them to the setup program. They're
installed to `<install dir>\git`, with their own system config: LFS filters, `credential.helper=manager`,
`core.longpaths=true` and the Git for Windows defaults. The user's own `~/.gitconfig` still applies, and a Git
installed separately is unaffected.

g4v uses that Git unless `G4V_GIT` names another, or *Settings › Use the Git installed on this computer* is on (after
a restart). A `G4V_GIT` path that doesn't exist is ignored, and the start screen and Settings say so. It also puts it first on `PATH` for everything g4v starts: git hooks, a project's post-sync scripts and
the editor. *File › GitHub account…* shows who is signed in and signs in or out through the bundled Credential
Manager. Sign-ins are stored in Windows Credential Manager and shared with any other Git on the machine.

To update the bundled Git:
1. Change `version`, `url` and `sha256` together in `installer\git-bundle.json`. Each GitHub release lists the
   SHA-256 of its assets.
2. Run `build.ps1`. Its built-in checks confirm that the bundled `git`, `git lfs` and `git credential-manager`
   start, and that the bundle's own config is the one in effect.
3. Ship a new installer. Git security fixes reach users only this way, so watch the
   [Git for Windows releases](https://github.com/git-for-windows/git/releases).

Licenses: MinGit is GPL-2.0 (with the components listed in its `LICENSE.txt`), and Git LFS and Git Credential Manager
are MIT. `git\THIRD-PARTY-NOTICES.txt` names each one's version and source. They are shipped unmodified and run as
separate programs.

### macOS installer

On a Mac with the Xcode Command Line Tools and rustup:

```bash
installer/macos/build.sh
```

This produces `dist/g4v-<version>-macos.dmg`, a disk image with a universal (Apple Silicon + Intel) `g4v.app` and an
*Applications* shortcut: open it and drag g4v into Applications. It needs macOS 11 or later. Pass `--native` to
build only for the Mac you're on, which is faster. You don't need a Mac for this: run the **macOS installer** workflow
from the Actions tab (it also runs on `v*` tags) and download the `.dmg` from the run's artifacts.

By default the app is ad-hoc signed, so on first launch of a downloaded copy macOS asks you to confirm it:
right-click g4v in Applications and choose **Open**. To sign and notarize, set `G4V_SIGN_IDENTITY` to your
*Developer ID Application* identity and `G4V_NOTARY_PROFILE` to a `notarytool` keychain profile before building.
g4v uses the `git` and `git-lfs` on your system and also looks in the Homebrew and MacPorts folders, so
`brew install git git-lfs` is all you need.

To make binary files lockable (Perforce-style exclusive check-out):

```bash
git lfs track --lockable "*.psd" "*.fbx" "*.blend" "*.uasset"
```

Settings are stored in your OS config directory (`%APPDATA%\g4v\settings.json` on Windows). Set `G4V_CONFIG_DIR` to
use a different location, which is handy for portable installs and testing.

## Development

```text
src/
  git/            UI-free Git backend (fully tested)
    mod.rs          command runner: safe environment, literal pathspecs, command log
    status.rs       porcelain v2 parser, in-progress merge/rebase detection
    ops.rs          submit, push (with retry), revert, Get Latest, conflict resolution
    shelf.rs        shelves built from commit objects and a temporary index
    lfs.rs          locks, lockable/LFS attributes
    history.rs      log parsing and the lane graph
    branches.rs     branches, tags, remotes, submodules, stashes
    diff.rs         unified diff parser, LFS pointer detection
  changelists.rs  pending changelist store
  worker.rs       serial background job queue (panic-safe)
  ui/             egui front end (tab state, views, dialogs, diff viewer)
tests/workflows.rs  end-to-end tests: a bare "server" repo and two clones acting as teammates
```

```bash
cargo test
```

```bash
cargo clippy --all-targets
```

Large-batch profiling (temporary repositories; default 5,000 modified and 1,250 new files):

```bash
cargo test --release --test perf -- --ignored --nocapture --test-threads=1
```

Set `G4V_PERF_FILES` to change the batch size and `G4V_PERF_LOG=1` to print slow Git commands.
See [the hardening and profiling report](docs/hardening-review.md) for findings, measured results, and limits.

The integration tests run against real repositories and cover submitting only selected files, out-of-date detection,
the Get Latest safety stop, push rejection followed by automatic rebase, keep-mine and take-theirs during rebases and
merges, abort, shelve/revert/unshelve round trips, shared shelves between two clones, reverting every kind of change,
literal handling of special file names, the first commit in an empty repository, branches, and undoing the last
commit.

## Known limitations

* Locking needs a Git LFS server with locking support. Without one, the Locks tab explains how to set it up and
  everything else still works.
* There's no built-in three-way text merge editor. g4v launches your configured `git mergetool` for that.
* Hunk-level staging, interactive rebase and other advanced history editing are left out on purpose. They're the
  operations that most often confuse the people g4v is built for.

## License

MIT. See [LICENSE](LICENSE).
