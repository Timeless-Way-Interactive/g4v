//! A binaries build running on this machine, for the loading bar.
//!
//! The project's build script (on a machine that is also a CI runner) writes its progress to
//! `%ProgramData%\g4v\builds\<owner>~<repo>.json`, replacing the file atomically. g4v only reads it. The script
//! works out the fraction and time left itself, from how long each phase took on earlier runs and from live
//! counters such as the compiler's `[n/m]`; see the DxDB repository's `docs/binaries/BuildProgress.psm1`.
//!
//! The file outlives the build. A running build only counts while the process that wrote it is alive; a finished
//! one is shown for a short while and then ignored.

use serde::Deserialize;
use std::path::PathBuf;

/// How long a finished build stays on screen.
pub const FINISHED_SHOWN_SECS: i64 = 120;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct BuildProgress {
    pub version: u32,
    pub repo: String,
    pub pid: u32,
    pub commit: String,
    #[serde(default)]
    pub key: Option<String>,
    /// `running`, `succeeded` or `failed`.
    pub state: String,
    pub started: i64,
    pub updated: i64,
    #[serde(default)]
    pub phase_label: String,
    #[serde(default)]
    pub phase_index: u32,
    #[serde(default)]
    pub phase_count: u32,
    #[serde(default)]
    pub detail: Option<String>,
    /// 0..=1, of the whole build in time, never decreasing.
    pub fraction: f32,
    #[serde(default)]
    pub eta_seconds: Option<u64>,
    /// Other builds queued after this one.
    #[serde(default)]
    pub waiting: u32,
}

impl BuildProgress {
    pub fn running(&self) -> bool {
        self.state == "running"
    }
    pub fn failed(&self) -> bool {
        self.state == "failed"
    }
}

/// `owner/repo` -> the file name stem the build script uses. Must match `Get-BuildProgressSlug`: lower case,
/// anything outside `[a-z0-9._-]` becomes `~`.
pub fn slug(release_repo: &str) -> String {
    release_repo.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '~' }).collect()
}

/// The folder build scripts write to. `G4V_BUILD_PROGRESS_DIR` overrides it (tests, unusual setups).
pub fn progress_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("G4V_BUILD_PROGRESS_DIR") {
        return Some(PathBuf::from(d));
    }
    if cfg!(windows) {
        std::env::var_os("ProgramData").map(|p| PathBuf::from(p).join("g4v").join("builds"))
    } else {
        Some(std::env::temp_dir().join("g4v-builds"))
    }
}

/// The build for `release_repo` worth showing right now, if any.
pub fn current(release_repo: &str, now: i64) -> Option<BuildProgress> {
    let path = progress_dir()?.join(format!("{}.json", slug(release_repo)));
    let bytes = std::fs::read(path).ok()?;
    let p: BuildProgress = serde_json::from_slice(super::strip_bom(&bytes)).ok()?;
    worth_showing(p, now, pid_alive)
}

/// The rules, apart from the file system and the process table.
pub fn worth_showing(p: BuildProgress, now: i64, alive: impl Fn(u32) -> bool) -> Option<BuildProgress> {
    if p.version != 1 {
        return None;
    }
    if p.running() {
        // A runner that was killed leaves "running" behind; its process is gone.
        return alive(p.pid).then_some(p);
    }
    (now - p.updated <= FINISHED_SHOWN_SECS).then_some(p)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: plain Win32 calls on a handle this function opens and closes.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == STILL_ACTIVE as u32
    }
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// "about 42 min left", "about 1 h 05 min left", "less than a minute left".
pub fn describe_eta(secs: u64) -> String {
    match secs {
        0..=59 => "less than a minute left".into(),
        60..=3599 => format!("about {} min left", secs.div_ceil(60)),
        _ => format!("about {} h {:02} min left", secs / 3600, (secs % 3600).div_ceil(60).min(59)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(state: &str, updated: i64) -> BuildProgress {
        serde_json::from_str(&format!(
            r#"{{"version":1,"repo":"O/R","pid":42,"commit":"abc","key":null,"state":"{state}","started":100,
                "updated":{updated},"phase":"editor","phase_label":"Compiling the editor","phase_index":3,"phase_count":6,
                "detail":"1 / 2","fraction":0.5,"eta_seconds":90,"waiting":1}}"#
        ))
        .unwrap()
    }

    #[test]
    fn slug_matches_the_build_script() {
        assert_eq!(slug("Timeless-Way-Interactive/Delta-x-Dungeon-Breaker"), "timeless-way-interactive~delta-x-dungeon-breaker");
    }

    #[test]
    fn running_needs_a_live_process() {
        assert!(worth_showing(sample("running", 0), 10_000, |_| true).is_some());
        assert!(worth_showing(sample("running", 0), 10_000, |_| false).is_none());
    }

    #[test]
    fn finished_shows_briefly() {
        assert!(worth_showing(sample("succeeded", 1000), 1000 + FINISHED_SHOWN_SECS, |_| false).is_some());
        assert!(worth_showing(sample("failed", 1000), 1001 + FINISHED_SHOWN_SECS, |_| false).is_none());
    }

    #[test]
    fn eta_text() {
        assert_eq!(describe_eta(30), "less than a minute left");
        assert_eq!(describe_eta(61), "about 2 min left");
        assert_eq!(describe_eta(3900), "about 1 h 05 min left");
    }

    #[test]
    fn this_process_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    /// Files written by the real BuildProgress.psm1 (a failed run), under Windows PowerShell 5.1 and PowerShell 7.
    #[test]
    fn reads_what_the_build_script_writes() {
        for bytes in [
            &include_bytes!("../../tests/fixtures/build-progress-ps51.json")[..],
            include_bytes!("../../tests/fixtures/build-progress-ps7.json"),
        ] {
            let p: BuildProgress = serde_json::from_slice(crate::binaries::strip_bom(bytes)).unwrap();
            assert_eq!(p.repo, "Timeless-Way-Interactive/Delta-x-Dungeon-Breaker");
            assert!(p.failed());
            assert_eq!((p.phase_index, p.phase_count, p.phase_label.as_str()), (3, 6, "Compiling the editor"));
            assert_eq!(p.detail.as_deref(), Some("compile error"));
            assert!(worth_showing(p.clone(), p.updated + 5, |_| false).is_some());
        }
    }
}
