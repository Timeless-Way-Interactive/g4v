//! Where release assets come from.
//!
//! * [`GitHubSource`] — the REST releases API, authenticated with the github.com token Git Credential Manager
//!   already stores for the user (`git credential fill`), so artists need nothing beyond their normal sign-in.
//! * [`DirSource`] — `<dir>/<release>/<asset>` on disk, for tests and for studios that mirror releases to a share.
//!   Selected by setting `G4V_BINARIES_SOURCE_DIR`.

use crate::git::Repo;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum FetchError {
    /// The release or asset doesn't exist (never published, pruned, or not finished publishing).
    NotFound,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::NotFound => write!(f, "not found on the server"),
            FetchError::Other(s) => write!(f, "{s}"),
        }
    }
}

/// Bytes done, total if known.
pub type Progress<'a> = &'a dyn Fn(u64, Option<u64>);

pub trait AssetSource: Send + Sync {
    /// Downloads `asset` of `release` to `dest` (replacing it). May resume a `<dest>.partial` left by an earlier try.
    fn fetch(&self, release: &str, asset: &str, dest: &Path, progress: Progress) -> Result<(), FetchError>;
}

/// Picks the source for `repo` / `release_repo`.
pub fn for_repo(repo: &Repo, release_repo: &str) -> Box<dyn AssetSource> {
    if let Some(dir) = std::env::var_os("G4V_BINARIES_SOURCE_DIR") {
        return Box::new(DirSource { dir: PathBuf::from(dir) });
    }
    let api = std::env::var("G4V_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into());
    Box::new(GitHubSource { api, repo: release_repo.to_string(), token: github_token(repo) })
}

pub struct DirSource {
    pub dir: PathBuf,
}

impl AssetSource for DirSource {
    fn fetch(&self, release: &str, asset: &str, dest: &Path, progress: Progress) -> Result<(), FetchError> {
        let src = self.dir.join(release).join(asset);
        let len = match std::fs::metadata(&src) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(FetchError::NotFound),
            Err(e) => return Err(FetchError::Other(e.to_string())),
        };
        std::fs::copy(&src, dest).map_err(|e| FetchError::Other(format!("copy {}: {e}", src.display())))?;
        progress(len, Some(len));
        Ok(())
    }
}

pub struct GitHubSource {
    pub api: String,
    pub repo: String,
    pub token: Option<String>,
}

/// Asks the user's credential helper for their github.com token. Never prompts in a terminal; a credential
/// manager may show its own sign-in window, which is the normal sign-in experience.
pub fn github_token(repo: &Repo) -> Option<String> {
    let out = repo.git().args(["credential", "fill"]).stdin(b"protocol=https\nhost=github.com\n\n".to_vec()).quiet().run().ok()?;
    out.text().lines().find_map(|l| l.strip_prefix("password=").map(str::to_string)).filter(|t| !t.is_empty())
}

impl GitHubSource {
    fn agent() -> ureq::Agent {
        // ureq drops the Authorization header when a redirect leaves the host (release downloads redirect to a
        // storage host), so the token is only ever sent to the API.
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(20))
            .timeout_read(std::time::Duration::from_secs(120))
            .user_agent(concat!("g4v/", env!("CARGO_PKG_VERSION")))
            .build()
    }

    fn request(&self, agent: &ureq::Agent, url: &str, accept: &str) -> ureq::Request {
        let mut r = agent.get(url).set("Accept", accept).set("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = &self.token {
            r = r.set("Authorization", &format!("Bearer {t}"));
        }
        r
    }

    fn asset_id(&self, agent: &ureq::Agent, release: &str, asset: &str) -> Result<u64, FetchError> {
        let url = format!("{}/repos/{}/releases/tags/{}", self.api, self.repo, release);
        let body: serde_json::Value = match self.request(agent, &url, "application/vnd.github+json").call() {
            Ok(r) => serde_json::from_reader(r.into_reader().take(8 << 20))
                .map_err(|e| FetchError::Other(format!("unreadable release listing: {e}")))?,
            Err(ureq::Error::Status(404, _)) => return Err(FetchError::NotFound),
            Err(ureq::Error::Status(401 | 403, _)) => {
                return Err(FetchError::Other(
                    "GitHub refused the request. Sign in to github.com with Git (any push or fetch) and try again.".into(),
                ))
            }
            Err(e) => return Err(FetchError::Other(format!("could not reach GitHub: {e}"))),
        };
        body["assets"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"] == asset))
            .and_then(|x| x["id"].as_u64())
            .ok_or(FetchError::NotFound)
    }
}

impl AssetSource for GitHubSource {
    fn fetch(&self, release: &str, asset: &str, dest: &Path, progress: Progress) -> Result<(), FetchError> {
        let agent = Self::agent();
        let id = self.asset_id(&agent, release, asset)?;
        let url = format!("{}/repos/{}/releases/assets/{id}", self.api, self.repo);
        let partial = PathBuf::from(format!("{}.partial", dest.display()));
        let have = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
        let mut req = self.request(&agent, &url, "application/octet-stream");
        if have > 0 {
            req = req.set("Range", &format!("bytes={have}-"));
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(404, _)) => return Err(FetchError::NotFound),
            // The partial file is longer than the asset (it changed): start over.
            Err(ureq::Error::Status(416, _)) => {
                let _ = std::fs::remove_file(&partial);
                return self.fetch(release, asset, dest, progress);
            }
            Err(e) => return Err(FetchError::Other(format!("download failed: {e}"))),
        };
        let resumed = resp.status() == 206;
        let total = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok()).map(|n| if resumed { n + have } else { n });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(&partial)
            .map_err(|e| FetchError::Other(format!("{}: {e}", partial.display())))?;
        let mut done = if resumed { have } else { 0 };
        let mut reader = resp.into_reader();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = reader.read(&mut buf).map_err(|e| FetchError::Other(format!("download interrupted: {e}")))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(|e| FetchError::Other(format!("could not write download: {e}")))?;
            done += n as u64;
            progress(done, total);
        }
        file.sync_all().map_err(|e| FetchError::Other(e.to_string()))?;
        drop(file);
        if total.is_some_and(|t| t != done) {
            return Err(FetchError::Other(format!("download ended early ({done} of {} bytes); try again to resume", total.unwrap_or(0))));
        }
        std::fs::rename(&partial, dest).map_err(|e| FetchError::Other(format!("{}: {e}", dest.display())))?;
        Ok(())
    }
}
