//! The UI side of precompiled binaries: the trust prompt, Get Latest through [`crate::binaries::sync`], the Launch
//! button, the "waiting for a build" badge and the banners for an unfinished install.

use super::tab::{Action, Dialog, Msg, OpOk, RepoTab};
use super::theme::{self, icon, ACCENT, MUTED, RED, YELLOW};
use crate::binaries::config::{BinariesConfig, LoadedConfig};
use crate::binaries::sync::{self, BinStatus, BinSyncOutcome};
use crate::binaries::trust::{Decision, TrustState, TrustStore};
use crate::binaries::{install, source};
use crate::settings::Settings;
use eframe::egui::{self, RichText, Ui};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Per-tab binaries state.
pub struct BinUi {
    /// `None`: the repository doesn't use binaries. `Some(Err)`: its config is invalid.
    pub cfg: Option<Result<LoadedConfig, String>>,
    pub trust: TrustState,
    /// Worked out after each fetch.
    pub status: Option<BinStatus>,
    /// The running job's latest progress line.
    pub progress: Arc<Mutex<Option<String>>>,
}

impl BinUi {
    pub fn load(root: &Path) -> BinUi {
        let mut b = BinUi { cfg: None, trust: TrustState::Unknown, status: None, progress: Arc::default() };
        b.reload(root);
        b
    }

    /// Re-reads the config and the trust decision (both are small files; a sync may have changed the config).
    pub fn reload(&mut self, root: &Path) {
        self.cfg = LoadedConfig::load(root).transpose();
        self.trust = match &self.cfg {
            Some(Ok(c)) => TrustStore::load().state(root, &c.hash),
            _ => TrustState::Unknown,
        };
    }

    /// The config, if the repository uses binaries and the user trusts it.
    pub fn active(&self) -> Option<&BinariesConfig> {
        match (&self.cfg, self.trust) {
            (Some(Ok(c)), TrustState::Trusted) => Some(&c.config),
            _ => None,
        }
    }

    pub fn progress_line(&self) -> Option<String> {
        self.progress.lock().ok().and_then(|p| p.clone())
    }
}

/// Fetches the build notes and works out the toolbar state. Runs on the network worker after a fetch.
pub fn status_after_fetch(r: &crate::git::Repo, cfg: &BinariesConfig, remote: Option<&str>) -> Option<BinStatus> {
    if let Some(remote) = remote {
        let _ = crate::binaries::notes::fetch(r, remote, &cfg.notes_ref);
    }
    Some(sync::status(r, cfg))
}

impl RepoTab {
    /// Get Latest when the repository uses binaries. Returns `false` to fall through to the normal Get Latest.
    pub(super) fn bin_get_latest(&mut self, settings: &Settings) -> bool {
        self.bin.reload(self.repo.root());
        match (&self.bin.cfg, self.bin.trust) {
            (None, _) | (_, TrustState::Declined) => false,
            (Some(Err(e)), _) => {
                self.error(format!("This project's binaries settings can't be used, so Get Latest won't install a build.\n{e}"));
                false
            }
            (Some(Ok(c)), TrustState::Unknown) => {
                self.dialog = Some(Dialog::TrustBinaries(c.clone()));
                true
            }
            (Some(Ok(c)), TrustState::Trusted) => {
                let cfg = c.config.clone();
                let mode = settings.pull_mode;
                let progress = self.bin.progress.clone();
                self.local.submit("Getting latest", move |r| {
                    let report = |s: String| {
                        if let Ok(mut p) = progress.lock() {
                            *p = Some(s);
                        }
                    };
                    let dirty: Vec<String> = r.status().map(|s| s.files.into_iter().map(|f| f.path).collect()).unwrap_or_default();
                    let src = source::for_repo(r, &cfg.release_repo);
                    let res = sync::get_latest(r, &cfg, src.as_ref(), &install::cache_root(), mode, &dirty, &report);
                    if let Ok(mut p) = progress.lock() {
                        *p = None;
                    }
                    Msg::Done { title: "Get Latest".into(), result: res.map(OpOk::Binaries), net: false }
                });
                true
            }
        }
    }

    pub(super) fn bin_set_trust(&mut self, trusted: bool, settings: &Settings) {
        let Some(Ok(c)) = &self.bin.cfg else { return };
        let mut store = TrustStore::load();
        store.set(self.repo.root(), &c.hash, if trusted { Decision::Trusted } else { Decision::Declined });
        if let Err(e) = store.save() {
            self.error(format!("Could not save your choice: {e}"));
        }
        self.bin.reload(self.repo.root());
        self.request_fetch();
        // Carry on with the Get Latest that asked.
        self.handle(Action::GetLatest, settings);
    }

    pub(super) fn bin_on_done(&mut self, outcome: BinSyncOutcome) {
        match outcome {
            BinSyncOutcome::Normal { reason, sync } => {
                if self.bin.active().is_some() {
                    self.toast(format!("Got latest without installing a build: {reason}."));
                }
                self.on_sync(sync);
            }
            BinSyncOutcome::UpToDate { waiting: 0 } => self.toast("You already have the latest version, and its build is installed."),
            BinSyncOutcome::UpToDate { waiting } => {
                self.toast(format!("You're on the newest change that has a build. {waiting} newer change(s) are still building."))
            }
            BinSyncOutcome::Synced { moved, installed, waiting } => {
                let mut parts = Vec::new();
                match moved {
                    0 => {}
                    n if n > 0 => parts.push(format!("Got {n} new change(s)")),
                    n => parts.push(format!("Went back {} change(s) to the newest one that has a build", -n)),
                }
                if let Some(k) = installed {
                    parts.push(format!("installed build {k}"));
                }
                if waiting > 0 {
                    parts.push(format!("{waiting} newer change(s) are still building"));
                }
                if parts.is_empty() {
                    parts.push("Up to date".into());
                }
                self.toast(format!("{}.", parts.join("; ")));
            }
            BinSyncOutcome::NoBuild { building } => {
                let extra = if building > 0 {
                    format!(" {building} change(s) are building now; try again when they finish.")
                } else {
                    String::new()
                };
                self.error(format!(
                    "No editor build is available for this branch yet, so nothing was changed.{extra}\nAsk a programmer to run the build workflow."
                ))
            }
            BinSyncOutcome::Blocked { overlap } => self.dialog = Some(Dialog::SyncBlocked(overlap)),
        }
        self.request_fetch();
    }

    pub(super) fn bin_launch(&mut self) {
        let Some(cfg) = self.bin.active().cloned() else { return };
        match sync::launch(&self.repo, &cfg) {
            Ok(()) => self.toast("Starting the editor…"),
            Err(e) => self.error(e),
        }
    }

    /// The Launch button and the "waiting for builds" note, next to Get Latest.
    pub(super) fn bin_toolbar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Some(cfg) = self.bin.active() else { return };
        if !cfg.launch.is_empty() {
            let blocker = sync::launch_blocker(&self.repo, cfg);
            let tip = blocker.clone().unwrap_or_else(|| format!("Start {}", cfg.launch[0]));
            if theme::tool_button(ui, "▶", "Launch", blocker.is_none() && !self.is_busy(), None, &tip).clicked() {
                actions.push(Action::LaunchEditor);
            }
        }
        if let Some(st) = &self.bin.status {
            if st.waiting > 0 {
                ui.label(RichText::new(format!("⏳ {} newer change(s) building", st.waiting)).color(MUTED))
                    .on_hover_text("Get Latest stops at the newest change that has a build, and picks these up once they're built.");
            }
        }
    }

    pub(super) fn bin_banners(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        if let Some(Err(e)) = &self.bin.cfg {
            let e = e.clone();
            theme::banner(ui, RED, |ui| {
                ui.label(format!("{} This project's binaries settings are invalid; Get Latest won't install builds. {e}", icon::WARN));
            });
            return;
        }
        let Some(cfg) = self.bin.active().cloned() else { return };
        if let Some(t) = sync::pending_txn(&self.repo) {
            let text = sync::describe_incomplete(&t);
            theme::banner(ui, RED, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{} {text}", icon::WARN));
                    if ui.button("Get Latest").clicked() {
                        actions.push(Action::GetLatest);
                    }
                });
            });
            return;
        }
        let installed =
            self.bin.status.as_ref().map(|s| s.installed.is_some()).unwrap_or_else(|| install::installed(self.repo.root(), &cfg).is_some());
        let index = self.bin.status.as_ref().is_some_and(|s| s.index);
        if index && !installed {
            theme::banner(ui, YELLOW, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{} The editor isn't installed in this workspace yet.", icon::INFO));
                    if ui.button(RichText::new("Get Latest to install it").strong()).clicked() {
                        actions.push(Action::GetLatest);
                    }
                });
            });
        }
    }

    /// The trust prompt. Returns `true` when the dialog should close.
    pub(super) fn bin_trust_dialog(ui: &mut Ui, cfg: &LoadedConfig, actions: &mut Vec<Action>) -> bool {
        let c = &cfg.config;
        ui.heading(format!("{} This project can install its editor for you", icon::INFO));
        ui.label(
            "With your permission, Get Latest will stop at the newest change that has a ready-made build, download that \
             build, and install it — so you never need to compile anything.",
        );
        ui.add_space(6.0);
        ui.label(theme::strong("It will:"));
        ui.label(format!("• download builds from github.com/{}", c.release_repo));
        ui.label(format!("• write files only inside: {}", c.install_roots.join(", ")));
        if let Some(ps) = &c.post_sync {
            ui.label(format!("• run this project's command after some syncs: {}", ps.run.join(" ")));
        }
        if !c.launch.is_empty() {
            ui.label(format!("• let you start {} with the Launch button", c.launch[0]));
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("Only allow this for projects you trust. If the project's settings change, g4v asks again.").color(MUTED).small(),
        );
        ui.add_space(6.0);
        ui.separator();
        let mut close = false;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("Allow").strong().color(ACCENT)).min_size(egui::vec2(90.0, 28.0))).clicked() {
                    actions.push(Action::TrustBinaries(true));
                    close = true;
                }
                if ui.add(egui::Button::new("Not for this project").min_size(egui::vec2(80.0, 28.0))).clicked() {
                    actions.push(Action::TrustBinaries(false));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        close
    }
}
