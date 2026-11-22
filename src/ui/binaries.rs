//! The UI side of precompiled binaries: the trust prompt, Get Latest through [`crate::binaries::sync`], the Launch
//! button, the "waiting for a build" badge, the banners for an unfinished install and, on a machine that builds,
//! the loading bar for the build in progress.
//!
//! A project can brand two of these with images in its `.g4v` folder: `launch-icon.png` on the Launch button and
//! `build-banner.png` as the loading bar's art. Both are optional.

use super::tab::{Action, Dialog, Msg, OpOk, RepoTab};
use super::theme::{self, icon, ACCENT, GREEN, MUTED, RED, YELLOW};
use crate::binaries::builder::{self, BuildProgress};
use crate::binaries::config::{BinariesConfig, LoadedConfig};
use crate::binaries::sync::{self, BinStatus, BinSyncOutcome};
use crate::binaries::trust::{Decision, TrustState, TrustStore};
use crate::binaries::{install, source};
use crate::settings::Settings;
use eframe::egui::{self, Color32, RichText, Ui};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LAUNCH_ICON: &str = ".g4v/launch-icon.png";
const BUILD_BANNER: &str = ".g4v/build-banner.png";
/// Larger images are ignored rather than uploaded as textures.
const MAX_ART_BYTES: u64 = 4 << 20;
const MAX_ART_SIDE: u32 = 2048;

/// The project's images, as textures. The banner comes in colour (the part that's done) and greyscale.
#[derive(Default)]
pub struct Art {
    loaded: bool,
    pub icon: Option<egui::TextureHandle>,
    pub banner: Option<(egui::TextureHandle, egui::TextureHandle)>,
}

/// Per-tab binaries state.
pub struct BinUi {
    /// `None`: the repository doesn't use binaries. `Some(Err)`: its config is invalid.
    pub cfg: Option<Result<LoadedConfig, String>>,
    pub trust: TrustState,
    /// Worked out after each fetch.
    pub status: Option<BinStatus>,
    /// The running job's latest progress line.
    pub progress: Arc<Mutex<Option<String>>>,
    /// A build of this project running (or just finished) on this machine.
    pub build: Option<BuildProgress>,
    build_checked: Option<Instant>,
    pub art: Art,
    /// The open trust prompt came from Get Latest, so answering it carries on with that Get Latest. From the set-up
    /// banner it doesn't: allowing only turns the features on.
    pub get_latest_after_trust: bool,
}

impl BinUi {
    pub fn load(root: &Path) -> BinUi {
        let mut b = BinUi {
            cfg: None,
            trust: TrustState::Unknown,
            status: None,
            progress: Arc::default(),
            get_latest_after_trust: false,
            build: None,
            build_checked: None,
            art: Art::default(),
        };
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
        // A sync may have brought new images.
        self.art.loaded = false;
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

    /// Every frame; does real work at most once a second: re-reads this machine's build progress (one small file
    /// and one process check) and loads the project's images when they may have changed.
    pub fn tick(&mut self, ctx: &egui::Context, root: &Path) {
        let Some(cfg) = self.active().cloned() else {
            self.build = None;
            return;
        };
        if self.build_checked.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.build_checked = Some(Instant::now());
            self.build = builder::current(&cfg.release_repo, chrono::Utc::now().timestamp());
        }
        if !self.art.loaded {
            self.art.loaded = true;
            self.art.icon = load_art(root, LAUNCH_ICON).map(|img| ctx.load_texture("g4v-launch-icon", img, egui::TextureOptions::LINEAR));
            self.art.banner = load_art(root, BUILD_BANNER).map(|img| {
                let grey = greyscale(&img);
                (
                    ctx.load_texture("g4v-build-banner", img, egui::TextureOptions::LINEAR),
                    ctx.load_texture("g4v-build-banner-grey", grey, egui::TextureOptions::LINEAR),
                )
            });
        }
    }
}

/// A project image, if it's there, inside the repository (no links out), and of a sensible size.
fn load_art(root: &Path, rel: &str) -> Option<egui::ColorImage> {
    let path = crate::binaries::paths::resolve_inside(root, rel).ok()?;
    if std::fs::metadata(&path).ok()?.len() > MAX_ART_BYTES {
        return None;
    }
    let img = image::load_from_memory(&std::fs::read(&path).ok()?).ok()?;
    if img.width() > MAX_ART_SIDE || img.height() > MAX_ART_SIDE {
        return None;
    }
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

fn greyscale(img: &egui::ColorImage) -> egui::ColorImage {
    let pixels: Vec<Color32> = img
        .pixels
        .iter()
        .map(|c| {
            let [r, g, b, a] = c.to_srgba_unmultiplied();
            let l = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
            Color32::from_rgba_unmultiplied(l, l, l, a)
        })
        .collect();
    let mut out = img.clone();
    out.pixels = pixels;
    out
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
                self.bin.get_latest_after_trust = true;
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
        // Carry on with the Get Latest that asked, if one did.
        if std::mem::take(&mut self.bin.get_latest_after_trust) {
            self.handle(Action::GetLatest, settings);
        }
    }

    /// The set-up banner's button: the trust prompt on its own.
    pub(super) fn bin_set_up(&mut self) {
        self.bin.reload(self.repo.root());
        if let (Some(Ok(c)), TrustState::Unknown) = (&self.bin.cfg, self.bin.trust) {
            self.dialog = Some(Dialog::TrustBinaries(c.clone()));
            self.bin.get_latest_after_trust = false;
        }
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
            let enabled = blocker.is_none() && !self.is_busy();
            let clicked = match &self.bin.art.icon {
                Some(icon) => {
                    let image =
                        egui::Image::from_texture(egui::load::SizedTexture::new(icon.id(), egui::vec2(22.0, 22.0))).corner_radius(4);
                    let btn = egui::Button::image_and_text(image, RichText::new("Launch").size(14.5)).min_size(egui::vec2(0.0, 30.0));
                    ui.add_enabled(enabled, btn).on_hover_text(&tip).on_disabled_hover_text(&tip).clicked()
                }
                None => theme::tool_button(ui, "▶", "Launch", enabled, None, &tip).clicked(),
            };
            if clicked {
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
        if matches!(self.bin.cfg, Some(Ok(_))) && self.bin.trust == TrustState::Unknown {
            // Nothing below shows until the project is trusted, and the prompt otherwise only appears on Get Latest:
            // without this, someone who has never clicked Get Latest has no way to know the features exist.
            theme::banner(ui, ACCENT, |ui| {
                ui.label(format!("{} This project can install a ready-made editor for you, so you never need to compile it.", icon::INFO));
                if ui.button(RichText::new("Set up").strong()).clicked() {
                    actions.push(Action::SetUpBinaries);
                }
            });
            return;
        }
        let Some(cfg) = self.bin.active().cloned() else { return };
        if let Some(p) = self.bin.build.clone() {
            self.build_bar(ui, &p);
        }
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

    /// The loading bar for a build running on this machine: the project's banner art in greyscale, filling with
    /// colour from the left as the build goes. Without art, a plain progress bar.
    fn build_bar(&self, ui: &mut Ui, p: &BuildProgress) {
        let color = if p.failed() {
            RED
        } else if p.running() {
            ACCENT
        } else {
            GREEN
        };
        let f = p.fraction.clamp(0.0, 1.0);
        egui::Frame::new()
            .fill(color.gamma_multiply(0.10))
            .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6)))
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    match &self.bin.art.banner {
                        Some((colour, grey)) => {
                            let size = colour.size_vec2();
                            let h = 86.0;
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(h * size.x / size.y, h), egui::Sense::hover());
                            let painter = ui.painter_at(rect);
                            let uv = |x: f32| egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(x, 1.0));
                            painter.image(grey.id(), rect, uv(1.0), Color32::from_gray(140));
                            let split = rect.left() + rect.width() * f;
                            painter.image(
                                colour.id(),
                                egui::Rect::from_min_max(rect.min, egui::pos2(split, rect.bottom())),
                                uv(f),
                                Color32::WHITE,
                            );
                            if p.running() {
                                painter.line_segment(
                                    [egui::pos2(split, rect.top()), egui::pos2(split, rect.bottom())],
                                    egui::Stroke::new(2.0, Color32::WHITE.gamma_multiply(0.85)),
                                );
                            }
                            let strip = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 4.0), rect.max);
                            painter.rect_filled(strip, 0.0, Color32::from_black_alpha(160));
                            painter.rect_filled(egui::Rect::from_min_max(strip.min, egui::pos2(split, strip.bottom())), 0.0, color);
                            resp.on_hover_text(format!("{:.0}% of the build's expected time", f * 100.0));
                        }
                        None => {
                            ui.add(egui::ProgressBar::new(f).desired_width(240.0).show_percentage());
                        }
                    }
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        let title = match p.state.as_str() {
                            "running" => format!("This machine is building the editor — {:.0}%", f * 100.0),
                            "succeeded" => "Build published".to_string(),
                            _ => "Build failed".to_string(),
                        };
                        ui.label(RichText::new(title).strong().size(15.0).color(color));
                        if p.running() {
                            ui.label(format!("Step {} of {}: {}", p.phase_index, p.phase_count, p.phase_label));
                        }
                        if let Some(d) = p.detail.as_deref().filter(|d| !d.is_empty()) {
                            ui.label(RichText::new(d).color(MUTED).small());
                        }
                        let mut bits = Vec::new();
                        if p.running() {
                            if let Some(e) = p.eta_seconds {
                                bits.push(builder::describe_eta(e));
                            }
                        }
                        bits.push(format!("started {}", theme::format_time(p.started)));
                        bits.push(format!("commit {}", &p.commit[..p.commit.len().min(10)]));
                        if p.waiting > 0 {
                            bits.push(format!("{} more waiting", p.waiting));
                        }
                        ui.label(RichText::new(bits.join("  ·  ")).color(MUTED).small());
                    });
                });
            });
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
