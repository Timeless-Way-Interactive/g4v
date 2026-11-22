//! Downloading, verifying and swapping a build into the workspace.
//!
//! The release manifest drives everything: g4v writes exactly the files it lists, each one inside a configured
//! install root, and removes only files the *previous* manifest installed that the new one lacks. Everything else
//! in those folders (local debug symbols, other build configurations) is left alone.
//!
//! The swap never needs a journal entry per file to be undone after a crash. For each file it first moves the
//! existing one into the backup folder, then moves the staged one into place — both renames on one volume. So,
//! for any manifest file:
//! * in the backup folder → it was replaced: put the backup back;
//! * still staged → it was never reached: leave the workspace copy alone;
//! * neither → it was new: remove it.
//!
//! Putting a backup back never consumes it: the file is hard-linked (or copied) into place, and the backup folder
//! is only deleted once the whole rollback — files, branch, submodules, setup — has finished. So a rollback that is
//! itself interrupted can simply run again: every step reaches the same result the second time. (Moving the
//! backup instead would leave a retry seeing neither a backup nor a staged copy, and it would delete the very file
//! the first attempt restored.)

use super::config::BinariesConfig;
use super::notes::Note;
use super::paths::{is_safe_rel, resolve_inside};
use super::source::{AssetSource, FetchError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Manifest {
    pub key: String,
    pub files: Vec<ManifestFile>,
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Manifest, String> {
        serde_json::from_slice(super::strip_bom(bytes)).map_err(|e| format!("the build's manifest is unreadable: {e}"))
    }

    /// Every path safe, inside an install root, not the marker or installed manifest, and listed once.
    pub fn validate(&self, cfg: &BinariesConfig, key: &str) -> Result<(), String> {
        if self.key != key {
            return Err(format!("the manifest is for build {}, not {key}", self.key));
        }
        if self.files.is_empty() {
            return Err("the build's manifest lists no files".into());
        }
        let reserved = [cfg.marker.to_lowercase(), cfg.installed_manifest().to_lowercase()];
        let mut seen = HashSet::new();
        for f in &self.files {
            if !is_safe_rel(&f.path) || !cfg.in_install_root(&f.path) {
                return Err(format!("the build tries to write outside its install folders: \"{}\"", f.path));
            }
            let lower = f.path.to_lowercase();
            if reserved.contains(&lower) {
                return Err(format!("the build tries to overwrite g4v's own record: \"{}\"", f.path));
            }
            // Windows file names ignore case; two entries differing only in case would fight over one file.
            if !seen.insert(lower) {
                return Err(format!("the manifest lists \"{}\" twice", f.path));
            }
            if f.sha256.len() != 64 || !f.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("the manifest has no valid checksum for \"{}\"", f.path));
            }
        }
        Ok(())
    }
}

/// Folder where downloads are kept between syncs (the last few builds, so switching back is instant).
pub fn cache_root() -> PathBuf {
    if let Some(d) = std::env::var_os("G4V_BINARIES_CACHE") {
        return PathBuf::from(d);
    }
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("g4v").join("bincache")
}

/// Keeps the `keep` most recently used builds in the cache.
pub fn prune_cache(root: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
        .collect();
    dirs.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (_, d) in dirs.into_iter().skip(keep) {
        let _ = std::fs::remove_dir_all(d);
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(super::hex(&h.finalize()))
}

/// Status messages from the install, for the UI.
pub type Report<'a> = &'a dyn Fn(String);

/// A verified build, extracted and waiting to be swapped in.
pub struct Staged {
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub dir: PathBuf,
}

#[derive(Debug)]
pub enum PrepareError {
    /// The release is gone or incomplete; try an older build.
    Missing,
    Failed(String),
}

impl From<String> for PrepareError {
    fn from(s: String) -> Self {
        PrepareError::Failed(s)
    }
}

fn fetch_cached(
    src: &dyn AssetSource,
    cache: &Path,
    release: &str,
    asset: &super::notes::AssetRef,
    report: Report,
) -> Result<PathBuf, PrepareError> {
    let dest = cache.join(&asset.name);
    if dest.exists() {
        match &asset.sha256 {
            Some(want) if sha256_file(&dest).map(|h| h.eq_ignore_ascii_case(want)).unwrap_or(false) => return Ok(dest),
            Some(_) => {
                let _ = std::fs::remove_file(&dest);
            }
            // Without a checksum to compare, the cached copy is still checked file by file after extraction.
            None => return Ok(dest),
        }
    }
    let name = asset.name.clone();
    let progress = |done: u64, total: Option<u64>| match total {
        Some(t) if t > 0 => report(format!("Downloading {name}: {} of {} MB", done >> 20, t >> 20)),
        _ => report(format!("Downloading {name}: {} MB", done >> 20)),
    };
    match src.fetch(release, &asset.name, &dest, &progress) {
        Ok(()) => {}
        Err(FetchError::NotFound) => return Err(PrepareError::Missing),
        Err(FetchError::Other(e)) => return Err(PrepareError::Failed(format!("Downloading {}: {e}", asset.name))),
    }
    if let Some(want) = &asset.sha256 {
        let got = sha256_file(&dest).map_err(|e| e.to_string())?;
        if !got.eq_ignore_ascii_case(want) {
            let _ = std::fs::remove_file(&dest);
            return Err(PrepareError::Failed(format!("{} is damaged (checksum mismatch); try again", asset.name)));
        }
    }
    Ok(dest)
}

/// Downloads (or reuses) the build in `note`, checks it and extracts it into `staging`. Changes nothing in the
/// workspace.
pub fn prepare(
    cfg: &BinariesConfig,
    note: &Note,
    src: &dyn AssetSource,
    cache_root: &Path,
    staging: &Path,
    report: Report,
) -> Result<Staged, PrepareError> {
    let (Some(archive), Some(manifest_ref)) = (&note.archive, &note.manifest) else { return Err(PrepareError::Missing) };
    let cache = cache_root.join(&note.key);
    std::fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    // Touch, so pruning keeps the builds in use.
    let _ = std::fs::write(cache.join(".used"), b"");

    let manifest_path = fetch_cached(src, &cache, &note.release, manifest_ref, report)?;
    let manifest_bytes = std::fs::read(&manifest_path).map_err(|e| e.to_string())?;
    let manifest = Manifest::parse(&manifest_bytes)?;
    manifest.validate(cfg, &note.key)?;

    let zip_path = fetch_cached(src, &cache, &note.release, archive, report)?;

    report("Checking the download…".into());
    if staging.exists() {
        std::fs::remove_dir_all(staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    }
    std::fs::create_dir_all(staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let file = std::fs::File::open(&zip_path).map_err(|e| e.to_string())?;
    let mut zip =
        zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| format!("{} is not a valid archive: {e}", archive.name))?;
    let total = manifest.files.len();
    for (i, f) in manifest.files.iter().enumerate() {
        if i % 50 == 0 {
            report(format!("Unpacking {i} of {total} files…"));
        }
        // Only entries the manifest lists are ever extracted, under the manifest's (checked) name.
        let mut entry = match zip.by_name(&f.path) {
            Ok(e) => e,
            Err(_) => return Err(PrepareError::Failed(format!("the archive is missing {}", f.path))),
        };
        let out_path = staging.join(&f.path);
        if let Some(p) = out_path.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        let mut out = std::fs::File::create(&out_path).map_err(|e| format!("{}: {e}", out_path.display()))?;
        let mut h = Sha256::new();
        let mut n: u64 = 0;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let r = entry.read(&mut buf).map_err(|e| format!("{}: {e}", f.path))?;
            if r == 0 {
                break;
            }
            h.update(&buf[..r]);
            out.write_all(&buf[..r]).map_err(|e| format!("{}: {e}", out_path.display()))?;
            n += r as u64;
            if n > f.bytes {
                break;
            }
        }
        if n != f.bytes || !super::hex(&h.finalize()).eq_ignore_ascii_case(&f.sha256) {
            return Err(PrepareError::Failed(format!("{} in the download doesn't match the manifest; try again", f.path)));
        }
    }
    Ok(Staged { manifest, manifest_bytes, dir: staging.to_path_buf() })
}

/// Reads the installed manifest and checks the install is whole: the marker holds its key and every file it lists
/// is present with the right size. A matching marker on its own proves nothing — an install may have been cut off.
pub fn installed(root: &Path, cfg: &BinariesConfig) -> Option<Manifest> {
    let marker = std::fs::read_to_string(resolve_inside(root, &cfg.marker).ok()?).ok()?;
    let bytes = std::fs::read(resolve_inside(root, &cfg.installed_manifest()).ok()?).ok()?;
    let m = Manifest::parse(&bytes).ok()?;
    if m.key != marker.trim() {
        return None;
    }
    for f in &m.files {
        let p = resolve_inside(root, &f.path).ok()?;
        if std::fs::metadata(&p).map(|md| md.len()).ok()? != f.bytes {
            return None;
        }
    }
    Some(m)
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        // Different volumes: copy, then remove the source (after the copy is safely written).
        Err(_) if from.exists() && !to.exists() => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

/// Swaps the staged build into the workspace, backing up what it replaces into `backup`. On error the caller
/// undoes it with [`restore`].
pub fn swap(root: &Path, cfg: &BinariesConfig, staged: &Staged, backup: &Path, report: Report) -> Result<(), String> {
    let previous = installed_manifest_any(root, cfg);
    let marker = resolve_inside(root, &cfg.marker)?;
    let inst_manifest = resolve_inside(root, &cfg.installed_manifest())?;
    std::fs::create_dir_all(backup).map_err(|e| format!("{}: {e}", backup.display()))?;
    // The marker goes first, so an interrupted swap never looks complete. It's backed up, not deleted, so a
    // rollback brings it back with the files.
    for (rel, abs) in [(&cfg.marker, &marker), (&cfg.installed_manifest(), &inst_manifest)] {
        if abs.exists() {
            move_file(abs, &backup.join(rel)).map_err(|e| format!("{}: {e}", abs.display()))?;
        }
    }
    let total = staged.manifest.files.len();
    for (i, f) in staged.manifest.files.iter().enumerate() {
        if i % 50 == 0 {
            report(format!("Installing {i} of {total} files…"));
        }
        let dst = resolve_inside(root, &f.path)?;
        if dst.is_dir() {
            return Err(format!("{} is a folder; the build needs a file there", dst.display()));
        }
        if dst.exists() {
            move_file(&dst, &backup.join(&f.path))
                .map_err(|e| format!("{} is in use or protected ({e}). Close any program using it and try again.", dst.display()))?;
        }
        move_file(&staged.dir.join(&f.path), &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    }
    if let Some(prev) = previous {
        let now: HashSet<String> = staged.manifest.files.iter().map(|f| f.path.to_lowercase()).collect();
        for f in prev.files {
            if is_safe_rel(&f.path) && cfg.in_install_root(&f.path) && !now.contains(&f.path.to_lowercase()) {
                let abs = resolve_inside(root, &f.path)?;
                if abs.is_file() {
                    move_file(&abs, &backup.join(&f.path)).map_err(|e| format!("{}: {e}", abs.display()))?;
                }
            }
        }
    }
    write_atomic(&inst_manifest, &staged.manifest_bytes)?;
    write_atomic(&marker, staged.manifest.key.as_bytes())?;
    Ok(())
}

/// The previous install's manifest even if that install is incomplete (its files are still ours to clean up).
fn installed_manifest_any(root: &Path, cfg: &BinariesConfig) -> Option<Manifest> {
    let bytes = std::fs::read(resolve_inside(root, &cfg.installed_manifest()).ok()?).ok()?;
    Manifest::parse(&bytes).ok()
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent folder")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    tmp.write_all(bytes).map_err(|e| e.to_string())?;
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    tmp.persist(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// Puts `bak` at `dst` and keeps `bak`: a hard link where possible (same volume, instant), else a copy. Written
/// under a temporary name first, so `dst` is either the old file or the restored one, never half of either.
///
/// The temporary file is always one this call creates (exclusively: a hard link or `create_new` fails rather than
/// replace anything), under a fresh name, and only that file is ever removed. A file of the user's that happens to
/// sit next to `dst` under any name is never touched.
fn put_back(bak: &Path, dst: &Path) -> std::io::Result<()> {
    let dir = dst.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let base = dst.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut created = None;
    for n in 0..10_000u32 {
        let tmp = dir.join(format!(".{base}.g4v-restore-{}-{n}", std::process::id()));
        match std::fs::hard_link(bak, &tmp) {
            Ok(()) => {
                created = Some(tmp);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // No hard links here (another volume, a file system without them): copy into a file created new.
            Err(_) => match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
                Ok(mut f) => {
                    let copied = std::fs::File::open(bak).and_then(|mut src| std::io::copy(&mut src, &mut f)).and_then(|_| f.sync_all());
                    drop(f);
                    if let Err(e) = copied {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e);
                    }
                    created = Some(tmp);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            },
        }
    }
    let tmp = created.ok_or_else(|| std::io::Error::other(format!("no free temporary name next to {}", dst.display())))?;
    // rename replaces an existing file on Windows and Unix alike.
    std::fs::rename(&tmp, dst).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Undoes a (possibly partial or crashed) [`swap`] using only what is on disk: the new manifest, the staging
/// folder and the backup folder. See the module docs for the rule. Safe to run any number of times: the backup and
/// staging folders are left for the caller to delete once its whole rollback has succeeded.
pub fn restore(root: &Path, cfg: &BinariesConfig, new_manifest: Option<&Manifest>, staging: &Path, backup: &Path) -> Result<(), String> {
    let mut errors = Vec::new();
    // g4v's own record first. The current marker / installed manifest is the swap's (and must go) when the old one
    // is in the backup, or when it already names the new build. Otherwise it's the old one, never touched.
    let new_key = new_manifest.map(|m| m.key.as_str());
    let marker_key = |p: &Path| std::fs::read_to_string(p).ok().map(|k| k.trim().to_string());
    let manifest_key = |p: &Path| std::fs::read(p).ok().and_then(|b| Manifest::parse(&b).ok()).map(|m| m.key);
    for (rel, read_key) in
        [(cfg.marker.clone(), &marker_key as &dyn Fn(&Path) -> Option<String>), (cfg.installed_manifest(), &manifest_key)]
    {
        let Ok(abs) = resolve_inside(root, &rel) else { continue };
        if abs.exists() && (backup.join(&rel).exists() || (new_key.is_some() && read_key(&abs).as_deref() == new_key)) {
            let _ = std::fs::remove_file(&abs);
        }
    }
    if let Some(m) = new_manifest {
        for f in &m.files {
            let Ok(dst) = resolve_inside(root, &f.path) else { continue };
            let bak = backup.join(&f.path);
            if bak.exists() {
                if let Err(e) = put_back(&bak, &dst) {
                    errors.push(format!("{}: {e}", dst.display()));
                }
            } else if !staging.join(&f.path).exists() && dst.is_file() {
                if let Err(e) = std::fs::remove_file(&dst) {
                    errors.push(format!("{}: {e}", dst.display()));
                }
            }
        }
    }
    // Everything else in the backup: stale files from the previous install, the old marker and manifest.
    let mut stack = vec![backup.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(rel) = p.strip_prefix(backup) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let Ok(dst) = resolve_inside(root, &rel) else { continue };
            if let Err(e) = put_back(&p, &dst) {
                errors.push(format!("{}: {e}", dst.display()));
            }
        }
    }
    if !errors.is_empty() {
        // Without its files, the old install isn't whole: drop the marker so nothing trusts it.
        if let Ok(m) = resolve_inside(root, &cfg.marker) {
            let _ = std::fs::remove_file(m);
        }
        return Err(format!("Some files could not be put back:\n{}", errors.join("\n")));
    }
    Ok(())
}
