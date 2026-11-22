//! The g4v desktop application (egui).

mod binaries;
mod detail;
mod dialogs;
pub mod tab;
pub mod theme;
pub mod tree;
mod views;
mod widgets;

use crate::git::{CmdLog, Repo};
use crate::settings::Settings;
use eframe::egui::{self, RichText};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use tab::{AppRequest, RepoTab};
use theme::{icon, ACCENT, MUTED, RED};

/// The answer of a background account operation: the GitHub accounts afterwards.
type AccountReply = Receiver<Result<Vec<String>, String>>;

enum GlobalDialog {
    Clone {
        url: String,
        parent: String,
        name: String,
        name_edited: bool,
    },
    Cloning {
        url: String,
        rx: Receiver<Result<PathBuf, String>>,
    },
    Init {
        path: String,
    },
    /// Holds the global git identity, looked up once when the dialog opens.
    Settings(crate::git::ops::Identity),
    /// GitHub sign-in through Git Credential Manager. `pending` is a sign-in, sign-out or listing in progress; it
    /// answers with the account list afterwards.
    Account {
        accounts: Option<Result<Vec<String>, String>>,
        pending: Option<(String, AccountReply)>,
    },
    About,
    Help,
    Error(String),
}

pub struct G4vApp {
    settings: Settings,
    tabs: Vec<RepoTab>,
    active: usize,
    log: CmdLog,
    dialog: Option<GlobalDialog>,
    git: Result<String, String>,
    applied_theme: Option<(bool, f32)>,
}

impl G4vApp {
    pub fn new(cc: &eframe::CreationContext<'_>, open: Vec<PathBuf>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let settings = Settings::load();
        let mut app = G4vApp {
            settings,
            tabs: Vec::new(),
            active: 0,
            log: CmdLog::default(),
            dialog: None,
            git: crate::git::git_version(),
            applied_theme: None,
        };
        if app.git.is_ok() {
            let mut paths = app.settings.open_repos.clone();
            paths.extend(open.iter().cloned());
            let restore_active = app.settings.active_repo;
            for p in paths {
                app.open_repo(&cc.egui_ctx, p, None, false);
            }
            app.active =
                if open.is_empty() { restore_active.min(app.tabs.len().saturating_sub(1)) } else { app.tabs.len().saturating_sub(1) };
        }
        app
    }

    fn open_repo(&mut self, ctx: &egui::Context, path: PathBuf, parent: Option<String>, report: bool) {
        match Repo::open(&path, self.log.clone()) {
            Ok(repo) => {
                if let Some(i) = self.tabs.iter().position(|t| t.repo.root() == repo.root()) {
                    self.active = i;
                    return;
                }
                let root = repo.root().to_path_buf();
                self.tabs.push(RepoTab::new(repo, ctx, parent));
                self.active = self.tabs.len() - 1;
                self.settings.add_recent(root);
                self.persist();
            }
            Err(e) => {
                if report {
                    self.dialog = Some(GlobalDialog::Error(e.to_string()));
                }
            }
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            if self.tabs[i].has_pending_work() {
                self.dialog = Some(GlobalDialog::Error(
                    "This project still has an operation running. Wait for it to finish before closing the tab.".into(),
                ));
                return;
            }
            self.tabs.remove(i);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
            self.persist();
        }
    }

    fn persist(&mut self) {
        // Submodule tabs are re-opened from their parent; only top-level repos are restored.
        self.settings.open_repos = self.tabs.iter().filter(|t| t.parent.is_none()).map(|t| t.repo.root().to_path_buf()).collect();
        self.settings.active_repo = self.active;
        self.settings.save();
    }

    fn pick_and_open(&mut self, ctx: &egui::Context) {
        if let Some(dir) = rfd::FileDialog::new().set_title("Open a Git repository (workspace folder)").pick_folder() {
            self.open_repo(ctx, dir, None, true);
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open repository…").clicked() {
                    ui.close();
                    self.pick_and_open(ui.ctx());
                }
                if ui.button("Clone repository…").clicked() {
                    ui.close();
                    self.open_clone_dialog();
                }
                if ui.button("New repository…").clicked() {
                    ui.close();
                    self.dialog = Some(GlobalDialog::Init { path: String::new() });
                }
                ui.menu_button("Recent", |ui| {
                    let recents = self.settings.recent_repos.clone();
                    if recents.is_empty() {
                        ui.label(theme::small("Nothing yet"));
                    }
                    for r in recents {
                        if ui.button(r.display().to_string()).clicked() {
                            ui.close();
                            self.open_repo(ui.ctx(), r, None, true);
                        }
                    }
                });
                ui.separator();
                if !self.tabs.is_empty() && ui.button("Close tab").clicked() {
                    ui.close();
                    self.close_tab(self.active);
                }
                if ui.button("GitHub account…").clicked() {
                    ui.close();
                    self.dialog = Some(account_dialog("Checking…", |_| Ok(())));
                }
                if ui.button("Settings…").clicked() {
                    ui.close();
                    self.dialog = Some(GlobalDialog::Settings(crate::git::ops::global_identity(&self.log)));
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                if ui.checkbox(&mut self.settings.dark_mode, "Dark mode").changed() {
                    self.settings.save();
                }
                ui.horizontal(|ui| {
                    ui.label("Zoom");
                    if ui.button("−").clicked() {
                        self.settings.ui_scale = (self.settings.ui_scale - 0.1).max(0.6);
                        self.settings.save();
                    }
                    ui.label(format!("{:.0}%", self.settings.ui_scale * 100.0));
                    if ui.button("+").clicked() {
                        self.settings.ui_scale = (self.settings.ui_scale + 0.1).min(2.5);
                        self.settings.save();
                    }
                });
            });
            if let Some(tab) = self.tabs.get(self.active) {
                let root = tab.repo.root().to_path_buf();
                ui.menu_button("Repository", |ui| {
                    if ui.button("Open folder in file browser").clicked() {
                        ui.close();
                        crate::git::ops::open_path(&root);
                    }
                    if ui.button("Copy folder path").clicked() {
                        ui.close();
                        ui.ctx().copy_text(root.display().to_string());
                    }
                    if ui.button("Open terminal here").clicked() {
                        ui.close();
                        open_terminal(&root);
                    }
                });
            }
            ui.menu_button("Help", |ui| {
                if ui.button("g4v for Perforce users").clicked() {
                    ui.close();
                    self.dialog = Some(GlobalDialog::Help);
                }
                if ui.button("About g4v").clicked() {
                    ui.close();
                    self.dialog = Some(GlobalDialog::About);
                }
            });
        });
    }

    fn open_clone_dialog(&mut self) {
        let parent = self
            .settings
            .recent_repos
            .first()
            .and_then(|p| p.parent())
            .map(|p| p.display().to_string())
            .or_else(|| dirs::document_dir().map(|d| d.display().to_string()))
            .unwrap_or_default();
        self.dialog = Some(GlobalDialog::Clone { url: String::new(), parent, name: String::new(), name_edited: false });
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui) {
        let mut close = None;
        let mut select = None;
        ui.horizontal(|ui| {
            for (i, t) in self.tabs.iter().enumerate() {
                let active = i == self.active;
                let mut label = t.title.clone();
                if let Some(s) = &t.status {
                    if !s.files.is_empty() {
                        label.push_str(&format!("  • {}", s.files.len()));
                    }
                    if s.branch.behind > 0 {
                        label.push_str(&format!("  ⬇{}", s.branch.behind));
                    }
                }
                let text = if active { RichText::new(label).strong() } else { RichText::new(label) };
                let r = ui.selectable_label(active, text).on_hover_text(t.repo.root().display().to_string());
                if r.clicked() {
                    select = Some(i);
                }
                if r.middle_clicked() {
                    close = Some(i);
                }
                if ui.small_button("✖").on_hover_text("Close tab").clicked() {
                    close = Some(i);
                }
                ui.separator();
            }
            if ui.button("✚").on_hover_text("Open or clone a repository").clicked() {
                self.pick_and_open(ui.ctx());
            }
        });
        if let Some(i) = select {
            self.active = i;
            self.persist();
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.label(RichText::new("g4v").size(48.0).strong().color(ACCENT));
            ui.label(RichText::new("Version control for artists, designers and everyone else").size(16.0).color(MUTED));
            ui.add_space(30.0);
            if let Err(e) = &self.git {
                ui.label(RichText::new(e).color(RED));
                return;
            }
            let size = egui::vec2(260.0, 40.0);
            if ui.add(egui::Button::new(RichText::new("📂  Open a project folder…").size(16.0)).min_size(size)).clicked() {
                self.pick_and_open(ui.ctx());
            }
            ui.add_space(6.0);
            if ui.add(egui::Button::new(RichText::new("⬇  Get a project from a server (clone)…").size(16.0)).min_size(size)).clicked() {
                self.open_clone_dialog();
            }
            ui.add_space(6.0);
            if ui.add(egui::Button::new(RichText::new("✚  Start a new project…").size(16.0)).min_size(size)).clicked() {
                self.dialog = Some(GlobalDialog::Init { path: String::new() });
            }
            ui.add_space(30.0);
            if !self.settings.recent_repos.is_empty() {
                ui.label(theme::strong("Recent"));
                for r in self.settings.recent_repos.clone() {
                    if ui.link(r.display().to_string()).clicked() {
                        self.open_repo(ui.ctx(), r, None, true);
                    }
                }
            }
            ui.add_space(20.0);
            if let Ok(v) = &self.git {
                ui.label(theme::small(v.clone()));
            }
        });
    }

    fn global_dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.dialog.take() else { return };
        let mut keep = true;
        let mut open_after: Option<PathBuf> = None;
        // A fresh clone of a project that ships binaries gets its editor installed straight away.
        let mut after_clone = false;
        let mut next: Option<GlobalDialog> = None;
        let resp = egui::Modal::new(egui::Id::unique("g4v_global_dialog")).show(ctx, |ui| {
            ui.set_width(540.0);
            match &mut dlg {
                GlobalDialog::Clone { url, parent, name, name_edited } => {
                    ui.heading("⬇ Clone a repository");
                    ui.label("Repository URL");
                    if ui
                        .add(egui::TextEdit::singleline(url).hint_text("https://github.com/team/project.git").desired_width(f32::INFINITY))
                        .changed()
                        && !*name_edited
                    {
                        *name = url
                            .trim()
                            .trim_end_matches('/')
                            .rsplit(['/', ':'])
                            .next()
                            .unwrap_or_default()
                            .trim_end_matches(".git")
                            .to_string();
                    }
                    ui.label("Save in folder");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(parent).desired_width(ui.available_width() - 90.0));
                        if ui.button("Browse…").clicked() {
                            if let Some(d) = rfd::FileDialog::new().pick_folder() {
                                *parent = d.display().to_string();
                            }
                        }
                    });
                    ui.label("Project folder name");
                    if ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY)).changed() {
                        *name_edited = true;
                    }
                    let dest = PathBuf::from(parent.trim()).join(name.trim());
                    let exists = !name.trim().is_empty()
                        && dest.exists()
                        && std::fs::read_dir(&dest).map(|mut d| d.next().is_some()).unwrap_or(true);
                    if exists {
                        ui.colored_label(RED, "That folder already exists and isn't empty.");
                    } else if !name.trim().is_empty() {
                        ui.label(theme::small(format!("Will be created at {}", dest.display())));
                    }
                    let valid = !url.trim().is_empty() && !parent.trim().is_empty() && !name.trim().is_empty() && !exists;
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled(valid, egui::Button::new(RichText::new("Clone").strong())).clicked() {
                                let (tx, rx) = std::sync::mpsc::channel();
                                let u = url.trim().to_string();
                                let log = self.log.clone();
                                let ctx2 = ui.ctx().clone();
                                std::thread::spawn(move || {
                                    let res = crate::git::ops::clone(&u, &dest, &log).map(|_| dest.clone()).map_err(|e| e.to_string());
                                    let _ = tx.send(res);
                                    ctx2.request_repaint();
                                });
                                next = Some(GlobalDialog::Cloning { url: url.clone(), rx });
                            }
                            if ui.button("Cancel").clicked() {
                                keep = false;
                            }
                        });
                    });
                }
                GlobalDialog::Cloning { url, rx } => {
                    ui.heading("Cloning…");
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(format!("Downloading {url}. Large art projects can take a while."));
                    });
                    ui.label(theme::small("If your server asks you to sign in, a login window will appear."));
                    match rx.try_recv() {
                        Ok(Ok(p)) => {
                            open_after = Some(p);
                            after_clone = true;
                            keep = false;
                        }
                        Ok(Err(e)) => next = Some(GlobalDialog::Error(format!("Clone failed:\n{e}"))),
                        Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
                    }
                }
                GlobalDialog::Init { path } => {
                    ui.heading("✚ New repository");
                    ui.label("Choose an (empty or existing) folder to put under version control.");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(path).desired_width(ui.available_width() - 90.0));
                        if ui.button("Browse…").clicked() {
                            if let Some(d) = rfd::FileDialog::new().pick_folder() {
                                *path = d.display().to_string();
                            }
                        }
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled(!path.trim().is_empty(), egui::Button::new(RichText::new("Create").strong())).clicked() {
                                let p = PathBuf::from(path.trim());
                                if p.join(".git").exists() {
                                    open_after = Some(p);
                                    keep = false;
                                } else {
                                    match crate::git::ops::init(&p, &self.log) {
                                        Ok(()) => {
                                            open_after = Some(p);
                                            keep = false;
                                        }
                                        Err(e) => next = Some(GlobalDialog::Error(e.to_string())),
                                    }
                                }
                            }
                            if ui.button("Cancel").clicked() {
                                keep = false;
                            }
                        });
                    });
                }
                GlobalDialog::Settings(global) => {
                    ui.heading("Settings");
                    let before = self.settings.clone();
                    let s = &mut self.settings;
                    egui::Grid::new("settings_grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                        ui.label("Check the server every");
                        ui.horizontal(|ui| {
                            let mut mins = (s.fetch_interval_secs / 60) as u32;
                            if ui.add(egui::DragValue::new(&mut mins).range(0..=120).suffix(" min")).changed() {
                                s.fetch_interval_secs = mins as u64 * 60;
                            }
                            ui.label(theme::small("0 = only when you press Refresh"));
                        });
                        ui.end_row();
                        ui.label("Submit");
                        ui.checkbox(&mut s.push_on_submit, "Upload to the server right away (Perforce-style)");
                        ui.end_row();
                        ui.label("Locks");
                        ui.checkbox(&mut s.unlock_after_submit, "Release my locks after submitting");
                        ui.end_row();
                        ui.label("Get Latest with my own unpushed commits");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut s.pull_mode, crate::git::ops::PullMode::Rebase, "Rebase (linear history)");
                            ui.radio_value(&mut s.pull_mode, crate::git::ops::PullMode::Merge, "Merge");
                        });
                        ui.end_row();
                        ui.label("Warn about files larger than");
                        ui.add(egui::DragValue::new(&mut s.large_file_mb).range(1..=4096).suffix(" MB"));
                        ui.end_row();
                        ui.label("Keep automatic backups for");
                        ui.add(egui::DragValue::new(&mut s.backup_retention_days).range(0..=3650).suffix(" days"));
                        ui.end_row();
                        ui.label("Appearance");
                        ui.checkbox(&mut s.dark_mode, "Dark mode");
                        ui.end_row();
                    });
                    ui.separator();
                    let bundled = crate::git::bundled_git_dir().is_some();
                    let using = match crate::git::git_source() {
                        crate::git::GitSource::Bundled(_) => "the Git that came with g4v",
                        crate::git::GitSource::System => "the Git installed on this computer",
                        crate::git::GitSource::Env(_) => "the Git named by G4V_GIT",
                    };
                    ui.label(theme::small(format!("Using {using}: {}", self.git.as_deref().unwrap_or("not found"))));
                    if bundled
                        && ui
                            .checkbox(&mut self.settings.use_system_git, "Use the Git installed on this computer instead (restart g4v to apply)")
                            .on_hover_text("For programmers who keep their own Git. Artists should leave this off.")
                            .changed()
                    {
                        self.settings.save();
                    }
                    ui.label(theme::small(format!(
                        "Git identity: {} <{}>",
                        global.name.as_deref().unwrap_or("not set"),
                        global.email.as_deref().unwrap_or("not set")
                    )));
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(RichText::new("Done").strong()).clicked() {
                                keep = false;
                            }
                        });
                    });
                    if self.settings != before {
                        self.settings.save();
                    }
                }
                GlobalDialog::Account { accounts, pending } => {
                    ui.heading("GitHub account");
                    if let Some((_, rx)) = pending {
                        if let Ok(r) = rx.try_recv() {
                            *accounts = Some(r);
                            *pending = None;
                        }
                    }
                    let mut next: Option<GlobalDialog> = None;
                    match (&*pending, &*accounts) {
                        (Some((what, _)), _) => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(what.as_str());
                            });
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                        }
                        (None, Some(Err(e))) => {
                            ui.label(RichText::new("Git Credential Manager isn't available, so g4v can't manage sign-in.").color(RED));
                            ui.label(theme::small(e.clone()));
                        }
                        (None, Some(Ok(list))) => {
                            if list.is_empty() {
                                ui.label("You're not signed in to GitHub on this computer.");
                            } else {
                                ui.label("Signed in on this computer as:");
                                for a in list.clone() {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(&a).strong());
                                        if ui.button("Sign out").clicked() {
                                            next = Some(account_dialog("Signing out…", move |_| {
                                                crate::git::run_plain(&["credential-manager", "github", "logout", &a]).map(|_| ())
                                            }));
                                        }
                                    });
                                }
                            }
                            ui.add_space(6.0);
                            let label = if list.is_empty() { "Sign in to GitHub" } else { "Add another account" };
                            if ui.button(RichText::new(label).strong()).clicked() {
                                next = Some(account_dialog("Finish signing in in the window that opened…", |_| {
                                    crate::git::run_plain(&["credential-manager", "github", "login"]).map(|_| ())
                                }));
                            }
                            ui.label(theme::small(
                                "Sign-in is stored in Windows Credential Manager and used for cloning, Get Latest, Submit, locks and build downloads.",
                            ));
                        }
                        (None, None) => {}
                    }
                    ui.separator();
                    if ui.button("Close").clicked() {
                        keep = false;
                    }
                    if let Some(n) = next {
                        *self.dialog.as_mut().unwrap() = n;
                    }
                }
                GlobalDialog::About => {
                    ui.heading("g4v");
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.label("A P4V-style visual client for Git, with changelists, shelves, file locking and out-of-date indicators.");
                    if let Ok(v) = &self.git {
                        ui.label(theme::small(v.clone()));
                    }
                    if ui.button("Close").clicked() {
                        keep = false;
                    }
                }
                GlobalDialog::Help => {
                    ui.heading("g4v for Perforce users");
                    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                        egui::Grid::new("help").num_columns(2).striped(true).spacing([20.0, 6.0]).show(ui, |ui| {
                            for (p4, g4v) in HELP {
                                ui.label(RichText::new(*p4).strong());
                                ui.label(*g4v);
                                ui.end_row();
                            }
                        });
                    });
                    if ui.button("Close").clicked() {
                        keep = false;
                    }
                }
                GlobalDialog::Error(e) => {
                    ui.heading(RichText::new(format!("{} Something went wrong", icon::WARN)).color(RED));
                    ui.label(e.as_str());
                    ui.horizontal(|ui| {
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(e.clone());
                        }
                        if ui.button("Close").clicked() {
                            keep = false;
                        }
                    });
                }
            }
        });
        if let Some(n) = next {
            dlg = n;
        }
        let cloning = matches!(dlg, GlobalDialog::Cloning { .. });
        if resp.should_close() && !cloning && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            keep = false;
        }
        if keep {
            self.dialog = Some(dlg);
        }
        if let Some(p) = open_after {
            self.open_repo(ctx, p, None, true);
            if after_clone {
                let settings = self.settings.clone();
                if let Some(tab) = self.tabs.get_mut(self.active).filter(|t| t.bin.cfg.is_some()) {
                    tab.handle(tab::Action::GetLatest, &settings);
                }
            }
        }
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        if tab.toasts.is_empty() {
            return;
        }
        let mut dismiss = None;
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                for (i, t) in tab.toasts.iter().enumerate().rev().take(5) {
                    let color = if t.error { RED } else { ACCENT };
                    egui::Frame::new()
                        .fill(ui.visuals().window_fill)
                        .stroke(egui::Stroke::new(1.5, color))
                        .corner_radius(8)
                        .inner_margin(10)
                        .shadow(ui.visuals().popup_shadow)
                        .show(ui, |ui| {
                            ui.horizontal_top(|ui| {
                                ui.label(RichText::new(if t.error { icon::WARN } else { icon::INFO }).color(color).size(16.0));
                                ui.add(egui::Label::new(&t.text).wrap());
                                if ui.small_button("✖").clicked() {
                                    dismiss = Some(i);
                                }
                            });
                            if t.error && ui.small_button("Copy details").clicked() {
                                ui.ctx().copy_text(t.text.clone());
                            }
                        });
                    ui.add_space(6.0);
                }
            });
        if let Some(i) = dismiss {
            tab.toasts.remove(i);
        }
    }
}

const HELP: &[(&str, &str)] = &[
    ("Workspace / client", "The project folder on your computer (a clone)."),
    ("Depot", "The server repository (\"remote\", usually called origin)."),
    ("Get Latest (p4 sync)", "Get Latest — downloads new changes. Files with a yellow ⬇ have newer versions on the server."),
    ("Check Out (p4 edit)", "Check Out locks the file with Git LFS so nobody else can submit it. Only needed for lockable (binary) files; everything else you can just edit."),
    ("Mark for Add / Delete", "Automatic — new and deleted files simply appear in Pending."),
    ("Pending changelist", "Same idea: group files into changelists, then Submit or Shelve them."),
    ("Submit", "Submit = git commit + push. Unticking \"Upload\" only commits on your computer."),
    ("Shelve / Unshelve", "Same idea. Shelves can be shared with the team (pushed to the server)."),
    ("Revert", "Revert — throws away local edits. g4v always keeps a backup shelf first."),
    ("Resolve", "If the same file was changed by two people, choose Keep mine / Take theirs (or merge text files in a merge tool)."),
    ("Streams", "Branches. Switch with the branch selector in the toolbar."),
    ("Submitted changelists", "History tab."),
    ("p4 opened -a / locks", "Locks tab — who has what checked out."),
    ("Changelist number", "Each submitted change has an id (a long hash); the first 8 characters are enough."),
];

fn open_terminal(dir: &std::path::Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd").args(["/C", "start", "cmd"]).current_dir(dir).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").args(["-a", "Terminal"]).arg(dir).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("x-terminal-emulator").current_dir(dir).spawn();
    }
}

impl eframe::App for G4vApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let want = (self.settings.dark_mode, self.settings.ui_scale);
        if self.applied_theme != Some(want) {
            theme::apply(&ctx, want.0, want.1);
            self.applied_theme = Some(want);
        }
        // Background updates for every open tab (so tab badges stay live).
        for t in &mut self.tabs {
            t.tick(&ctx, &self.settings);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.tabs.iter().any(RepoTab::has_pending_work) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.dialog = Some(GlobalDialog::Error("An operation is still running. Wait for it to finish before exiting g4v.".into()));
        }
        // Requests from tabs (e.g. open a submodule).
        let mut requests = Vec::new();
        for t in &mut self.tabs {
            requests.append(&mut t.app_requests);
        }
        for r in requests {
            match r {
                AppRequest::OpenRepo(p, parent) => self.open_repo(&ctx, p, parent, true),
            }
        }

        egui::Panel::top("menu").show(ui, |ui| {
            self.menu_bar(ui);
            if !self.tabs.is_empty() {
                ui.add_space(2.0);
                self.tab_strip(ui);
            }
        });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            if self.tabs.is_empty() {
                egui::CentralPanel::default_margins().show(ui, |ui| self.welcome(ui));
            } else {
                let log = self.log.clone();
                let settings = self.settings.clone();
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.show(ui, &settings, &log);
                }
            }
        });
        self.toasts(&ctx);
        self.global_dialogs(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist();
        // Housekeeping: prune old automatic backups.
        if self.settings.backup_retention_days > 0 {
            for t in &self.tabs {
                let _ = t.repo.prune_backups(self.settings.backup_retention_days);
            }
        }
    }
}

/// The account dialog with `action` running in the background (shown as `what`), then the account list.
fn account_dialog(what: &str, action: impl FnOnce(()) -> Result<(), String> + Send + 'static) -> GlobalDialog {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = action(()).and_then(|_| crate::git::github_accounts());
        let _ = tx.send(r);
    });
    GlobalDialog::Account { accounts: None, pending: Some((what.to_string(), rx)) }
}
