//! g4v Windows installer / uninstaller.
//!
//! A single self-contained executable that embeds `g4v.exe` and, appended to its end, a zip of the Git that g4v
//! uses (MinGit with Git Credential Manager, plus Git LFS; see installer/build.ps1). It installs per user
//! (no administrator rights needed) into `%LOCALAPPDATA%\Programs\g4v`, creates
//! Start Menu / desktop shortcuts, and registers in "Apps & features" so g4v can
//! be uninstalled normally. The installed copy of this program is the uninstaller.
//!
//! Command line:
//!   g4v-setup.exe                         interactive install
//!   g4v-setup.exe --silent [--dir D]      unattended install
//!       [--no-shortcuts] [--no-register]
//!   g4v-setup.exe --uninstall [--silent]  remove g4v
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, RichText};
use std::path::{Path, PathBuf};
use std::process::Command;

const PAYLOAD: &[u8] = include_bytes!(env!("G4V_PAYLOAD"));
const VERSION: &str = env!("G4V_VERSION");
const APP_EXE: &str = "g4v.exe";
const UNINSTALL_EXE: &str = "uninstall.exe";
const REG_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\g4v";
/// The folder, inside the install directory, that holds the bundled Git.
const GIT_DIR: &str = "git";
/// Present in a bundled Git folder; nothing without it is ever deleted.
const GIT_MARKER: &str = ".g4v-bundle";
const TRAILER_MAGIC: &[u8; 8] = b"G4VGIT01";

/// The Git bundle appended to this program: (bytes of the program without it, the zip).
fn read_self() -> Result<(Vec<u8>, Option<std::ops::Range<usize>>), String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let bytes = std::fs::read(&me).map_err(|e| format!("Could not read {}: {e}", me.display()))?;
    let n = bytes.len();
    if n >= 16 && &bytes[n - 8..] == TRAILER_MAGIC {
        let len = u64::from_le_bytes(bytes[n - 16..n - 8].try_into().unwrap()) as usize;
        if len <= n - 16 {
            let start = n - 16 - len;
            return Ok((bytes, Some(start..n - 16)));
        }
    }
    Ok((bytes, None))
}

/// Extracts the bundled Git to `<dir>\git` by way of `git.new`, so a failure never leaves a half-written Git.
fn install_git(dir: &Path, zip_bytes: &[u8]) -> Result<(), String> {
    let new = dir.join("git.new");
    if new.exists() {
        std::fs::remove_dir_all(&new).map_err(|e| format!("Could not clear {}: {e}", new.display()))?;
    }
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).map_err(|e| format!("The bundled Git is damaged: {e}"))?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
        // enclosed_name refuses absolute paths and `..`.
        let Some(rel) = f.enclosed_name() else { return Err(format!("Unsafe path in the bundled Git: {}", f.name())) };
        let out = new.join(rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut w = std::fs::File::create(&out).map_err(|e| format!("Could not write {}: {e}", out.display()))?;
        std::io::copy(&mut f, &mut w).map_err(|e| format!("Could not write {}: {e}", out.display()))?;
    }
    if !new.join(GIT_MARKER).exists() || !new.join(r"cmd\git.exe").exists() {
        return Err("The bundled Git is incomplete.".into());
    }
    let cur = dir.join(GIT_DIR);
    let old = dir.join("git.old");
    if old.exists() {
        let _ = std::fs::remove_dir_all(&old);
    }
    if cur.exists() {
        if !cur.join(GIT_MARKER).exists() {
            return Err(format!("{} exists and isn't g4v's Git; move it away and try again.", cur.display()));
        }
        std::fs::rename(&cur, &old)
            .map_err(|e| format!("Could not replace g4v's Git ({e}). Close g4v and any Git windows it opened, then try again."))?;
    }
    if let Err(e) = std::fs::rename(&new, &cur) {
        if old.exists() {
            let _ = std::fs::rename(&old, &cur);
        }
        return Err(format!("Could not install g4v's Git: {e}"));
    }
    // A file still in use only delays this to the next install.
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => dir_size(&e.path()),
                    _ => e.metadata().map(|m| m.len()).unwrap_or(0),
                })
                .sum()
        })
        .unwrap_or(0)
}

#[derive(Clone)]
struct Options {
    dir: PathBuf,
    start_menu: bool,
    desktop: bool,
    register: bool,
}

fn default_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\g4v"));
    base.join("Programs").join("g4v")
}

#[cfg(windows)]
fn hide(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000)
}
#[cfg(not(windows))]
fn hide(cmd: &mut Command) -> &mut Command {
    cmd
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let out = hide(Command::new(program).args(args)).output().map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn start_menu_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs"))
}

fn desktop_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|u| PathBuf::from(u).join("Desktop"))
}

/// Creates a .lnk shortcut using the Windows Script Host COM object.
fn create_shortcut(link: &Path, target: &Path, workdir: &Path) -> Result<(), String> {
    let q = |p: &Path| p.display().to_string().replace('\'', "''");
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.WorkingDirectory='{}';$s.IconLocation='{},0';$s.Description='g4v - visual Git client';$s.Save()",
        q(link),
        q(target),
        q(workdir),
        q(target)
    );
    run("powershell", &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
}

fn is_running(exe: &Path) -> bool {
    // A running exe cannot be opened for writing on Windows.
    exe.exists() && std::fs::OpenOptions::new().write(true).open(exe).is_err()
}

fn install(o: &Options) -> Result<PathBuf, String> {
    let app = o.dir.join(APP_EXE);
    if is_running(&app) {
        return Err("g4v is currently running. Close it and try again.".into());
    }
    std::fs::create_dir_all(&o.dir).map_err(|e| format!("Could not create {}: {e}", o.dir.display()))?;
    // Write to a temp name then rename, so a failed copy never leaves a broken g4v.exe.
    let tmp = o.dir.join("g4v.exe.new");
    std::fs::write(&tmp, PAYLOAD).map_err(|e| format!("Could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &app).map_err(|e| format!("Could not replace {}: {e}", app.display()))?;

    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let (self_bytes, git_zip) = read_self()?;
    if let Some(range) = &git_zip {
        install_git(&o.dir, &self_bytes[range.clone()])?;
    }
    let uninstaller = o.dir.join(UNINSTALL_EXE);
    if me != uninstaller {
        // The uninstaller doesn't need the Git bundle: copy this program without it.
        let stub = git_zip.as_ref().map(|r| &self_bytes[..r.start]).unwrap_or(&self_bytes);
        std::fs::write(&uninstaller, stub).map_err(|e| format!("Could not write the uninstaller: {e}"))?;
    }

    if o.start_menu {
        if let Some(d) = start_menu_dir() {
            create_shortcut(&d.join("g4v.lnk"), &app, &o.dir)?;
        }
    }
    if o.desktop {
        if let Some(d) = desktop_dir() {
            create_shortcut(&d.join("g4v.lnk"), &app, &o.dir)?;
        }
    }
    if o.register {
        let size_kb = (PAYLOAD.len() / 1024
            + std::fs::metadata(&uninstaller).map(|m| m.len() as usize / 1024).unwrap_or(0)
            + (dir_size(&o.dir.join(GIT_DIR)) / 1024) as usize)
            .to_string();
        let uninstall_cmd = format!("\"{}\" --uninstall", uninstaller.display());
        let quiet_cmd = format!("\"{}\" --uninstall --silent", uninstaller.display());
        let icon = app.display().to_string();
        let loc = o.dir.display().to_string();
        let values: [(&str, &str, &str); 10] = [
            ("DisplayName", "REG_SZ", "g4v"),
            ("DisplayVersion", "REG_SZ", VERSION),
            ("Publisher", "REG_SZ", "g4v contributors"),
            ("DisplayIcon", "REG_SZ", &icon),
            ("InstallLocation", "REG_SZ", &loc),
            ("UninstallString", "REG_SZ", &uninstall_cmd),
            ("QuietUninstallString", "REG_SZ", &quiet_cmd),
            ("EstimatedSize", "REG_DWORD", &size_kb),
            ("NoModify", "REG_DWORD", "1"),
            ("NoRepair", "REG_DWORD", "1"),
        ];
        for (name, kind, data) in values {
            run("reg", &["add", REG_KEY, "/v", name, "/t", kind, "/d", data, "/f"])?;
        }
    }
    Ok(app)
}

/// The install to remove: the folder this uninstaller runs from when it is an install (holds g4v.exe), else the one
/// recorded in the registry. Never the registry first: a second copy's uninstaller would remove the main install.
fn installed_dir() -> PathBuf {
    if let Some(here) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        if here.join(APP_EXE).is_file() {
            return here;
        }
    }
    if let Some(p) = registered_dir() {
        return p;
    }
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(default_dir)
}

/// The install folder recorded in "Apps & features", if any.
fn registered_dir() -> Option<PathBuf> {
    let o = hide(Command::new("reg").args(["query", REG_KEY, "/v", "InstallLocation"])).output().ok()?;
    let text = String::from_utf8_lossy(&o.stdout);
    let line = text.lines().find(|l| l.contains("InstallLocation"))?;
    let p = line[line.find("REG_SZ")? + 6..].trim();
    (!p.is_empty()).then(|| PathBuf::from(p))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

fn uninstall() -> Result<(), String> {
    let dir = installed_dir();
    let app = dir.join(APP_EXE);
    if is_running(&app) {
        return Err("g4v is currently running. Close it and try again.".into());
    }
    for d in [start_menu_dir(), desktop_dir()].into_iter().flatten() {
        let lnk = d.join("g4v.lnk");
        if lnk.exists() {
            let _ = std::fs::remove_file(lnk);
        }
    }
    // Only this install's registration: another copy's uninstaller must not orphan the main install.
    if registered_dir().is_some_and(|r| same_dir(&r, &dir)) {
        let _ = run("reg", &["delete", REG_KEY, "/f"]);
    }
    if app.exists() {
        std::fs::remove_file(&app).map_err(|e| format!("Could not remove {}: {e}", app.display()))?;
    }
    // g4v's own Git, only if it carries the marker the installer wrote.
    for name in [GIT_DIR, "git.new", "git.old"] {
        let g = dir.join(name);
        if g.join(GIT_MARKER).exists() {
            std::fs::remove_dir_all(&g).map_err(|e| format!("Could not remove {}: {e}", g.display()))?;
        }
    }
    // Only g4v's own files are removed. User settings (%APPDATA%\g4v) and repositories are untouched.
    // This program may be the uninstaller inside `dir`, which can't delete itself while running:
    // hand that off to a short-lived background command.
    let me = std::env::current_exe().ok();
    let uninstaller = dir.join(UNINSTALL_EXE);
    if me.as_deref() == Some(uninstaller.as_path()) {
        // Pass paths as data: cmd expands %variables% even inside quotes. Never recursively delete the directory.
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command",
            "Start-Sleep -Seconds 2; Remove-Item -LiteralPath $env:G4V_REMOVE_EXE -Force; try { [System.IO.Directory]::Delete($env:G4V_REMOVE_DIR, $false) } catch {}"])
            .env("G4V_REMOVE_EXE", &uninstaller)
            .env("G4V_REMOVE_DIR", &dir);
        let _ = hide(&mut c).spawn();
    } else {
        let _ = std::fs::remove_file(&uninstaller);
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(())
}

// ------------------------------------------------------------------ GUI

enum Page {
    Welcome,
    Done(PathBuf),
    UninstallConfirm,
    Uninstalled,
    Failed(String),
}

struct SetupApp {
    page: Page,
    opts: Options,
    dir_text: String,
    launch: bool,
    /// Whether this setup program carries Git (read once: the file is large).
    bundled_git: bool,
}

impl eframe::App for SetupApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default_margins().show(ui, |ui| {
            ui.add_space(6.0);
            ui.label(RichText::new("g4v").size(30.0).strong().color(egui::Color32::from_rgb(64, 156, 255)));
            ui.label(RichText::new(format!("Version {VERSION} · visual Git client for artists and teams")).weak());
            ui.add_space(14.0);
            let mut next: Option<Page> = None;
            match &self.page {
                Page::Welcome => {
                    ui.label("Install location");
                    ui.add(egui::TextEdit::singleline(&mut self.dir_text).desired_width(f32::INFINITY));
                    ui.label(RichText::new("Installs for your user account only — no administrator rights needed.").small().weak());
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.opts.start_menu, "Add to Start menu");
                    ui.checkbox(&mut self.opts.desktop, "Create a desktop shortcut");
                    ui.checkbox(&mut self.launch, "Start g4v when finished");
                    ui.add_space(6.0);
                    let note = if self.bundled_git {
                        "Includes Git, Git LFS and Git Credential Manager — nothing else to install."
                    } else {
                        "g4v needs Git (git-scm.com). Git LFS is recommended for art files."
                    };
                    ui.label(RichText::new(note).small().weak());
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("Install").strong()).min_size(egui::vec2(110.0, 30.0))).clicked() {
                            self.opts.dir = PathBuf::from(self.dir_text.trim());
                            next = Some(match install(&self.opts) {
                                Ok(app) => {
                                    if self.launch {
                                        let _ = Command::new(&app).current_dir(&self.opts.dir).spawn();
                                    }
                                    Page::Done(app)
                                }
                                Err(e) => Page::Failed(e),
                            });
                        }
                        if ui.button("Cancel").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                Page::Done(app) => {
                    ui.label(RichText::new("✔ g4v is installed.").size(16.0).color(egui::Color32::from_rgb(80, 190, 110)));
                    ui.label(format!("Location: {}", app.display()));
                    ui.label("You can uninstall it any time from Settings › Apps.");
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if !self.launch && ui.button("Start g4v").clicked() {
                            let _ = Command::new(app).spawn();
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if ui.button("Close").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                Page::UninstallConfirm => {
                    ui.label("Remove g4v from this computer?");
                    ui.label(RichText::new("Your repositories and g4v settings are not touched.").small().weak());
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("Uninstall").strong()).min_size(egui::vec2(110.0, 30.0))).clicked() {
                            next = Some(match uninstall() {
                                Ok(()) => Page::Uninstalled,
                                Err(e) => Page::Failed(e),
                            });
                        }
                        if ui.button("Cancel").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                Page::Uninstalled => {
                    ui.label("g4v was removed.");
                    if ui.button("Close").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Page::Failed(e) => {
                    ui.label(RichText::new("Something went wrong").size(16.0).color(egui::Color32::from_rgb(235, 87, 87)));
                    ui.label(e.as_str());
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            next = Some(Page::Welcome);
                        }
                        if ui.button("Close").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
            }
            if let Some(p) = next {
                self.page = p;
            }
        });
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let uninstall_mode = has("--uninstall");
    let mut opts = Options {
        dir: value("--dir").map(PathBuf::from).unwrap_or_else(default_dir),
        start_menu: !has("--no-shortcuts"),
        desktop: false,
        register: !has("--no-register"),
    };

    if has("--silent") {
        let res = if uninstall_mode { uninstall() } else { install(&opts).map(|_| ()) };
        if let Err(e) = res {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }

    opts.desktop = true;
    let app = SetupApp {
        page: if uninstall_mode { Page::UninstallConfirm } else { Page::Welcome },
        dir_text: opts.dir.display().to_string(),
        opts,
        launch: true,
        bundled_git: read_self().map(|(_, z)| z.is_some()).unwrap_or(false),
    };
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(if uninstall_mode { "Uninstall g4v" } else { "g4v Setup" })
            .with_inner_size([520.0, 380.0])
            .with_resizable(false)
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../../assets/g4v.png")).unwrap_or_default()),
        ..Default::default()
    };
    let _ = eframe::run_native("g4v-setup", native, Box::new(|_| Ok(Box::new(app))));
}
