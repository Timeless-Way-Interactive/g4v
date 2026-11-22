#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;

/// Apps started from Finder or the Dock get a minimal PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), so Git and Git LFS
/// installed with Homebrew or MacPorts would not be found. Add their usual locations, ahead of the system ones so the
/// same `git` runs as in the user's terminal.
#[cfg(target_os = "macos")]
fn extend_macos_path() {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::env::split_paths(&current).collect();
    let extra = ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"];
    let mut front: Vec<PathBuf> = extra.iter().map(PathBuf::from).filter(|p| p.is_dir() && !paths.contains(p)).collect();
    if front.is_empty() {
        return;
    }
    front.append(&mut paths);
    if let Ok(joined) = std::env::join_paths(front) {
        std::env::set_var("PATH", joined);
    }
}

fn main() -> eframe::Result<()> {
    #[cfg(target_os = "macos")]
    extend_macos_path();
    // Before anything runs git: the installer's own Git unless the user chose the one installed on the computer.
    g4v::git::select_git(g4v::settings::Settings::load().use_system_git);
    // `g4v [folder ...]` opens the given repositories as tabs. (Older macOS passes `-psn_…` to apps opened from
    // Finder; that isn't a folder.)
    let open: Vec<PathBuf> = std::env::args_os().skip(1).filter(|a| !a.to_string_lossy().starts_with("-psn_")).map(PathBuf::from).collect();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("g4v")
            .with_inner_size([1400.0, 880.0])
            .with_min_inner_size([900.0, 560.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/g4v.png")).unwrap_or_default()),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native("g4v", options, Box::new(move |cc| Ok(Box::new(g4v::ui::G4vApp::new(cc, open)))))
}
