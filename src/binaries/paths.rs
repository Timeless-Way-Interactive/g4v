//! Repository-provided paths (from the config, a note or a release manifest) are untrusted input. They only ever
//! become file system paths through [`resolve_inside`], which refuses anything that could land outside the
//! workspace: absolute paths, `..`, drive letters, alternate data streams, backslashes, and any path whose existing
//! parent folders pass through a symbolic link or junction.

use std::path::{Path, PathBuf};

/// Is `p` a plain, forward-slash, relative path with only normal components?
pub fn is_safe_rel(p: &str) -> bool {
    if p.is_empty() || p.len() > 1024 || p.starts_with('/') || p.ends_with('/') {
        return false;
    }
    if p.contains('\\') || p.contains(':') || p.contains('\0') {
        return false;
    }
    p.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            // Windows silently drops trailing dots and spaces, so "a." and "a" are the same file.
            && !seg.ends_with('.')
            && !seg.ends_with(' ')
    })
}

/// Is `rel` equal to `prefix` or inside it (both already [`is_safe_rel`])?
pub fn is_under(rel: &str, prefix: &str) -> bool {
    rel == prefix || (rel.len() > prefix.len() && rel.starts_with(prefix) && rel.as_bytes()[prefix.len()] == b'/')
}

/// Joins `rel` onto `root` after checking it. Every folder between `root` and the final component that already
/// exists must be a real folder, not a link, so the result cannot be redirected elsewhere.
pub fn resolve_inside(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if !is_safe_rel(rel) {
        return Err(format!("refused unsafe path \"{rel}\""));
    }
    let segs: Vec<&str> = rel.split('/').collect();
    let mut path = root.to_path_buf();
    for (i, seg) in segs.iter().enumerate() {
        path.push(seg);
        let last = i + 1 == segs.len();
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("refused \"{rel}\": it passes through a linked folder ({})", path.display()));
            }
            Ok(m) if !last && !m.is_dir() => {
                return Err(format!("refused \"{rel}\": {} is a file, not a folder", path.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Nothing below a missing folder can be a link yet.
                for rest in &segs[i + 1..] {
                    path.push(rest);
                }
                return Ok(path);
            }
            Err(e) => return Err(format!("could not check \"{rel}\": {e}")),
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_paths() {
        for ok in ["Game/Binaries/Win64/a.dll", "a", "UnrealEngine/Engine/Binaries/x.exe", "x/.binkey"] {
            assert!(is_safe_rel(ok), "{ok}");
        }
        for bad in ["", "/abs", "a/../b", "..", "./a", "a//b", "a/", "C:/x", "C:x", "a\\b", "file.txt:stream", "a/b.", "a/b "] {
            assert!(!is_safe_rel(bad), "{bad}");
        }
    }

    #[test]
    fn under() {
        assert!(is_under("Game/Binaries/x", "Game/Binaries"));
        assert!(is_under("Game/Binaries", "Game/Binaries"));
        assert!(!is_under("Game/BinariesX/x", "Game/Binaries"));
        assert!(!is_under("Game", "Game/Binaries"));
    }

    #[test]
    fn resolve_refuses_files_as_folders() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), "x").unwrap();
        assert!(resolve_inside(tmp.path(), "f/inside").is_err());
        assert!(resolve_inside(tmp.path(), "new/dir/file").is_ok());
        assert!(resolve_inside(tmp.path(), "../escape").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_refuses_links() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
        assert!(resolve_inside(tmp.path(), "link/file").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn resolve_refuses_junctions() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            assert!(resolve_inside(tmp.path(), "link/file").is_err());
        }
    }
}
