//! Modal dialogs of a repository tab.

use super::tab::{first_line, Action, Dialog, RepoTab, SwitchTarget};
use super::theme::{self, icon, ACCENT, MUTED, ORANGE, RED, YELLOW};
use crate::changelists::DEFAULT_ID;
use eframe::egui::{self, RichText, Ui};

enum Outcome {
    Keep,
    Close,
}

fn buttons(ui: &mut Ui, ok_label: &str, ok_enabled: bool, danger: bool) -> (bool, bool) {
    let mut ok = false;
    let mut cancel = false;
    ui.add_space(6.0);
    ui.separator();
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let text = RichText::new(ok_label).strong();
            let text = if danger { text.color(RED) } else { text };
            if ui.add_enabled(ok_enabled, egui::Button::new(text).min_size(egui::vec2(90.0, 28.0))).clicked() {
                ok = true;
            }
            if ui.add(egui::Button::new("Cancel").min_size(egui::vec2(80.0, 28.0))).clicked() {
                cancel = true;
            }
        });
    });
    (ok, cancel)
}

fn file_checklist(ui: &mut Ui, files: &mut [(String, bool)], id: &str, annotate: impl Fn(&str) -> Option<(String, egui::Color32)>) {
    ui.horizontal(|ui| {
        let all = files.iter().all(|(_, on)| *on);
        let mut toggle = all;
        if ui.checkbox(&mut toggle, theme::small(format!("{} of {} selected", files.iter().filter(|f| f.1).count(), files.len()))).changed()
        {
            for f in files.iter_mut() {
                f.1 = toggle;
            }
        }
    });
    // Only the rows in view are built, so a changelist with thousands of files opens and scrolls instantly.
    let row_h = list_row_height(ui);
    egui::ScrollArea::vertical().id_salt(id).max_height(220.0).auto_shrink([false, true]).show_rows(ui, row_h, files.len(), |ui, range| {
        for (path, on) in files[range].iter_mut() {
            ui.horizontal(|ui| {
                ui.checkbox(on, path.as_str());
                if let Some((note, color)) = annotate(path) {
                    ui.label(RichText::new(note).small().color(color));
                }
            });
        }
    });
}

/// Height of one checkbox row (without the spacing between rows).
fn list_row_height(ui: &Ui) -> f32 {
    ui.spacing().interact_size.y.max(ui.text_style_height(&egui::TextStyle::Body))
}

/// "a, b, c and 12 more".
fn some_of(items: impl ExactSizeIterator<Item = String>, n: usize) -> String {
    let total = items.len();
    let mut s = items.take(n).collect::<Vec<_>>().join(", ");
    if total > n {
        s.push_str(&format!(" and {} more", total - n));
    }
    s
}

impl RepoTab {
    pub fn dialogs(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(mut dlg) = self.dialog.take() else { return };
        let mut outcome = Outcome::Keep;
        let modal = egui::Modal::new(egui::Id::unique(("g4v_dialog", self.repo.root().to_string_lossy().to_string())));
        let resp = modal.show(ctx, |ui| {
            ui.set_width(560.0);
            outcome = self.dialog_body(ui, &mut dlg, actions);
        });
        if resp.should_close() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            outcome = Outcome::Close;
        }
        if let (Outcome::Close, Dialog::Submit(d)) = (&outcome, &dlg) {
            // However the dialog closes (Cancel, Escape, or a submit that then fails), the description that was typed
            // is kept and offered again next time. It is dropped once the change is actually committed.
            self.submit_draft = Some((d.cl_id, d.message.clone()));
        }
        if matches!(outcome, Outcome::Keep) && self.dialog.is_none() {
            self.dialog = Some(dlg);
        }
    }

    fn dialog_body(&mut self, ui: &mut Ui, dlg: &mut Dialog, actions: &mut Vec<Action>) -> Outcome {
        match dlg {
            Dialog::Submit(d) => {
                let title = if d.cl_id == DEFAULT_ID { "Submit changelist".to_string() } else { format!("Submit changelist #{}", d.cl_id) };
                ui.heading(format!("{} {title}", icon::SUBMIT));
                ui.add_space(4.0);
                ui.label("Description");
                let te = egui::TextEdit::multiline(&mut d.message)
                    .hint_text("What did you change and why? (e.g. \"Hero: new run cycle, fixed foot sliding\")")
                    .desired_rows(4)
                    .desired_width(f32::INFINITY);
                ui.add(te);
                ui.add_space(4.0);
                let incoming = &self.incoming;
                let locks = &self.locks_by_path;
                let all_locks = &self.locks;
                file_checklist(ui, &mut d.files, "submit_files", |p| {
                    if let Some(l) = locks.get(p).and_then(|&i| all_locks.get(i)).filter(|l| !l.ours) {
                        return Some((format!("{} locked by {}", icon::LOCK, l.owner), RED));
                    }
                    if incoming.contains_key(p) {
                        return Some((format!("{} newer version on server", icon::WARN), ORANGE));
                    }
                    None
                });
                let chosen: Vec<String> = d.files.iter().filter(|f| f.1).map(|f| f.0.clone()).collect();

                // ---- safety checks
                let blocked = self.locked_by_others(&chosen);
                let large: Vec<(&String, u64)> = if d.large.is_empty() {
                    Vec::new()
                } else {
                    chosen.iter().filter(|p| !self.lfs_paths.contains(*p)).filter_map(|p| d.large.get(p).map(|s| (p, *s))).collect()
                };
                let unlocked = self.lockable_not_locked(&chosen);
                let outdated: Vec<&String> = chosen.iter().filter(|p| self.incoming.contains_key(*p)).collect();
                let empty_msg = d.message.trim().is_empty();
                if !blocked.is_empty() {
                    theme::banner(ui, RED, |ui| {
                        ui.label(format!(
                            "{} {} file(s) are checked out by someone else. Untick them or ask the owner to release the lock: {}",
                            icon::LOCK,
                            blocked.len(),
                            some_of(blocked.iter().map(|(p, o)| format!("{p} ({o})")), 5)
                        ));
                    });
                }
                if !large.is_empty() {
                    theme::banner(ui, ORANGE, |ui| {
                        ui.label(format!(
                            "{} Large file(s) not stored with Git LFS will make the repository slow for everyone:",
                            icon::WARN
                        ));
                        for (p, s) in large.iter().take(6) {
                            ui.label(format!("  {p} ({})", theme::format_size(*s)));
                        }
                        if large.len() > 6 {
                            ui.label(format!("  …and {} more", large.len() - 6));
                        }
                        // One button per file type, however many files there are.
                        let mut exts: Vec<String> =
                            large.iter().filter_map(|(p, _)| p.rsplit_once('.').map(|(_, e)| e.to_lowercase())).collect();
                        exts.sort();
                        exts.dedup();
                        ui.horizontal_wrapped(|ui| {
                            for ext in exts.iter().take(8) {
                                if ui.small_button(format!("Store *.{ext} with LFS")).clicked() {
                                    actions.push(Action::TrackLfs(format!("*.{ext}")));
                                }
                            }
                        });
                        ui.label(theme::small("After enabling LFS, cancel and submit again so .gitattributes is included."));
                    });
                }
                if !unlocked.is_empty() {
                    theme::banner(ui, YELLOW, |ui| {
                        ui.label(format!(
                            "{} {} lockable file(s) aren't checked out by you; someone else may be editing them too.",
                            icon::LOCK,
                            unlocked.len()
                        ));
                        if ui.small_button("Check them out now").clicked() {
                            actions.push(Action::Lock(unlocked.clone()));
                        }
                    });
                }
                if self.locks_err.is_some() && chosen.iter().any(|p| self.lockable.contains(p)) {
                    theme::banner(ui, YELLOW, |ui| {
                        ui.label(format!(
                            "{} g4v couldn't verify file locks. Reconnect before submitting, or untick Upload to save a local commit. Locks are checked again when submitting.",
                            icon::LOCK
                        ));
                    });
                }
                if !outdated.is_empty() {
                    theme::banner(ui, ORANGE, |ui| {
                        ui.label(format!(
                            "{} {} file(s) have newer versions on the server. g4v will get latest before uploading; if both versions changed the same lines you'll be asked to resolve.",
                            icon::WARN,
                            outdated.len()
                        ));
                    });
                }
                ui.add_space(4.0);
                let has_remote = self.remote.is_some();
                ui.add_enabled(has_remote, egui::Checkbox::new(&mut d.push, "Upload to server after committing (recommended)"))
                    .on_disabled_hover_text("This repository has no server configured");
                if self.uses_lfs {
                    ui.checkbox(&mut d.unlock, "Release my locks on these files after uploading");
                }
                let ok_enabled = !chosen.is_empty() && blocked.is_empty() && !empty_msg && !self.is_busy();
                if empty_msg {
                    ui.label(RichText::new("A description is required.").small().color(MUTED));
                }
                let submit_shortcut = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
                let (ok, cancel) = buttons(ui, if d.push { "Submit" } else { "Commit locally" }, ok_enabled, false);
                if (ok || (submit_shortcut && ok_enabled)) && ok_enabled {
                    let unlock: Vec<String> = if d.unlock {
                        chosen.iter().filter(|p| self.lock_of(p).is_some_and(|l| l.ours)).cloned().collect()
                    } else {
                        vec![]
                    };
                    // If only some files were submitted, keep the changelist (with the rest) around.
                    let all_files = chosen.len() == d.files.len();
                    let cl_id = if all_files { d.cl_id } else { DEFAULT_ID };
                    if !all_files && d.cl_id != DEFAULT_ID {
                        if let Some(cl) = self.changelists.get_mut(d.cl_id) {
                            cl.description = d.message.trim().to_string();
                        }
                    }
                    actions.push(Action::DoSubmit { paths: chosen, message: d.message.clone(), cl_id, push: d.push, unlock });
                    return Outcome::Close;
                }
                if cancel {
                    // Remember the description on the changelist so it isn't lost.
                    if d.cl_id != DEFAULT_ID && !d.message.trim().is_empty() {
                        if let Some(cl) = self.changelists.get_mut(d.cl_id) {
                            cl.description = d.message.trim().to_string();
                            let _ = self.changelists.save();
                        }
                    }
                    return Outcome::Close;
                }
            }
            Dialog::Shelve(d) => {
                ui.heading(format!("{} Shelve files", icon::SHELVE));
                ui.label(theme::small(
                    "A shelf saves the current version of files on the side. Your workspace is not changed unless you choose to revert.",
                ));
                ui.add_space(4.0);
                ui.label("Description");
                ui.add(
                    egui::TextEdit::multiline(&mut d.description)
                        .hint_text("e.g. Half-finished lighting pass")
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
                file_checklist(ui, &mut d.files, "shelve_files", |_| None);
                ui.checkbox(&mut d.revert, "Revert the files after shelving (clean workspace)");
                let can_share = self.remote.is_some();
                ui.add_enabled(can_share, egui::Checkbox::new(&mut d.share, "Share with the team (upload the shelf to the server)"));
                let chosen: Vec<String> = d.files.iter().filter(|f| f.1).map(|f| f.0.clone()).collect();
                let (ok, cancel) = buttons(ui, "Shelve", !chosen.is_empty() && !self.is_busy(), false);
                if ok {
                    let desc =
                        if d.description.trim().is_empty() { "Shelved changes".to_string() } else { d.description.trim().to_string() };
                    let all = chosen.len() == d.files.len();
                    actions.push(Action::DoShelve {
                        paths: chosen,
                        description: desc,
                        revert: d.revert,
                        share: d.share,
                        cl_id: if all { d.cl_id } else { None },
                    });
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
            Dialog::Unshelve(d) => {
                ui.heading(format!("{} Unshelve \"{}\"", icon::SHELVE, first_line(&d.shelf.display_name())));
                ui.label(theme::small(format!("by {} · {}", d.shelf.author, theme::format_time(d.shelf.time))));
                ui.add_space(4.0);
                let overwrite = d.files.iter().filter(|(f, on)| *on && self.file(&f.path).is_some()).count();
                let row_h = list_row_height(ui);
                let total = d.files.len();
                egui::ScrollArea::vertical().id_salt("unshelve_files").max_height(240.0).show_rows(ui, row_h, total, |ui, range| {
                    for (f, on) in d.files[range].iter_mut() {
                        ui.horizontal(|ui| {
                            let label = format!("{} {}", if f.status == 'D' { icon::DELETE } else { icon::EDIT }, f.path);
                            ui.checkbox(on, label);
                            if self.file(&f.path).is_some() {
                                ui.label(RichText::new("you have local edits").small().color(ORANGE));
                            }
                        });
                    }
                });
                if overwrite > 0 {
                    theme::banner(ui, ORANGE, |ui| {
                        ui.label(format!(
                            "{} {overwrite} file(s) you are editing will be replaced by the shelved version. g4v backs up your current versions to a shelf first.",
                            icon::WARN
                        ));
                    });
                }
                if d.shelf.base.is_some() && self.status.as_ref().and_then(|s| s.branch.oid.clone()) != d.shelf.base {
                    theme::banner(ui, YELLOW, |ui| {
                        ui.label(format!(
                            "{} This shelf was made from a different version of the project than you have now. The files come back exactly as they were shelved, so if a teammate has changed the same files since then, submitting them would undo that work. Look at the diff before you submit.",
                            icon::WARN
                        ));
                    });
                }
                ui.label(theme::small("The files will be put in a new changelist named after the shelf. The shelf itself is kept."));
                let chosen: Vec<String> = d.files.iter().filter(|f| f.1).map(|f| f.0.path.clone()).collect();
                let (ok, cancel) = buttons(ui, "Unshelve", !chosen.is_empty() && !self.is_busy(), false);
                if ok {
                    actions.push(Action::DoUnshelve { shelf: d.shelf.clone(), paths: chosen });
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
            Dialog::Revert(files) => {
                ui.heading(format!("{} Revert files", icon::REVERT));
                ui.label("Your local changes to these files will be thrown away and the latest submitted version restored. New files will be deleted.");
                file_checklist(ui, files, "revert_files", |_| None);
                theme::banner(ui, ACCENT, |ui| {
                    ui.label(format!(
                        "{} A backup is saved automatically (Shelves › Show automatic backups) in case you change your mind.",
                        icon::INFO
                    ));
                });
                let chosen: Vec<String> = files.iter().filter(|f| f.1).map(|f| f.0.clone()).collect();
                let (ok, cancel) = buttons(ui, &format!("Revert {} file(s)", chosen.len()), !chosen.is_empty() && !self.is_busy(), true);
                if ok {
                    actions.push(Action::DoRevert(chosen));
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
            Dialog::Changelist { id, description, move_files } => {
                ui.heading(if id.is_some() { "Edit changelist" } else { "New changelist" });
                ui.label("Description");
                let r = ui.add(
                    egui::TextEdit::multiline(description).hint_text("e.g. Hero textures").desired_rows(3).desired_width(f32::INFINITY),
                );
                if id.is_none() && description.is_empty() {
                    r.request_focus();
                }
                if !move_files.is_empty() {
                    ui.label(theme::small(format!("{} selected file(s) will be moved into it.", move_files.len())));
                }
                let (ok, cancel) = buttons(ui, "Save", !description.trim().is_empty(), false);
                if ok {
                    match id {
                        Some(i) => {
                            if let Some(c) = self.changelists.get_mut(*i) {
                                c.description = description.trim().to_string();
                            }
                        }
                        None => {
                            let new = self.changelists.create(description);
                            self.changelists.move_files(move_files, new);
                        }
                    }
                    if let Err(e) = self.changelists.save() {
                        self.error(format!("Could not save changelists: {e}"));
                    }
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
            Dialog::NewBranch { name, start, switch } => {
                ui.heading(format!("{} New branch", icon::BRANCH));
                ui.label(format!("Starting from: {}", start.clone().unwrap_or_else(|| format!("current ({})", self.branch_name()))));
                ui.label("Name");
                let r = ui.add(egui::TextEdit::singleline(name).hint_text("e.g. feature/new-level").desired_width(f32::INFINITY));
                r.request_focus();
                let problem = if name.trim().is_empty() { None } else { crate::git::branches::branch_name_problem_local(name.trim()) };
                let exists = self.refs.as_ref().is_some_and(|r| r.branches.iter().any(|b| b.name == name.trim()));
                if let Some(p) = &problem {
                    ui.colored_label(RED, p);
                } else if exists {
                    ui.colored_label(RED, "A branch with that name already exists.");
                }
                ui.checkbox(switch, "Switch to the new branch (your pending changes come along)");
                let (ok, cancel) = buttons(ui, "Create", !name.trim().is_empty() && problem.is_none() && !exists, false);
                if ok {
                    let n = name.trim().to_string();
                    let s = start.clone();
                    let sw = *switch;
                    self.run_simple(&format!("Creating branch {n}"), move |r| {
                        r.create_branch(&n, s.as_deref(), sw).map(|_| format!("Created branch {n}"))
                    });
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
            Dialog::SwitchDirty(target) => {
                let name = match target {
                    SwitchTarget::Local(b) => b.clone(),
                    SwitchTarget::Remote(b) => b.split_once('/').map(|(_, n)| n.to_string()).unwrap_or_else(|| b.clone()),
                };
                ui.heading(format!("{} Switch to {name}", icon::BRANCH));
                let n = self.files().iter().filter(|f| !f.submodule).count();
                ui.label(format!("You have {n} file(s) with uncommitted changes. What should happen to them?"));
                ui.add_space(6.0);
                let mut chosen = None;
                if ui
                    .add(
                        egui::Button::new(RichText::new("Shelve them here (recommended)").strong())
                            .min_size(egui::vec2(ui.available_width(), 32.0)),
                    )
                    .on_hover_text("Your changes stay with the current branch and are offered back when you return")
                    .clicked()
                {
                    chosen = Some(true);
                }
                ui.label(theme::small("   Changes stay with the current branch; g4v offers to restore them when you come back."));
                if ui.add(egui::Button::new("Bring them with me").min_size(egui::vec2(ui.available_width(), 32.0))).clicked() {
                    chosen = Some(false);
                }
                ui.label(theme::small(
                    "   Changes move to the new branch. Git refuses if they would clash with files there — nothing is lost.",
                ));
                ui.add_space(6.0);
                ui.separator();
                let cancel = ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.button("Cancel").clicked()).inner;
                if let Some(shelve) = chosen {
                    actions.push(Action::SwitchNow(target.clone(), shelve));
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
            Dialog::TrustBinaries(cfg) => {
                if RepoTab::bin_trust_dialog(ui, cfg, actions) {
                    return Outcome::Close;
                }
            }
            Dialog::SyncBlocked(overlap) => {
                ui.heading(format!("{} Get Latest needs your attention", icon::WARN));
                ui.label("These files have local edits AND newer versions on the server. Getting latest would overwrite your edits, so g4v stopped:");
                egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                    for p in overlap.iter() {
                        ui.label(format!("  {} {p}", icon::CONFLICT));
                    }
                });
                ui.add_space(6.0);
                ui.label(theme::strong("Options"));
                ui.label("• Shelve my edits and get latest: your edits are saved to a shelf; you can unshelve them later (the shelved version will replace the server's).");
                ui.label("• Submit first: submit your edits, then Get Latest will merge them (text files only).");
                ui.label("• Revert: throw away your edits to these files (backup kept).");
                ui.add_space(4.0);
                let mut close = false;
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Shelve my edits & get latest").strong()).clicked() {
                        actions.push(Action::ShelveOverlapAndSync(overlap.clone()));
                        close = true;
                    }
                    if ui.button("Revert these files…").clicked() {
                        actions.push(Action::OpenRevert(overlap.clone()));
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
                if close {
                    return Outcome::Close;
                }
            }
            Dialog::Confirm { title, body, action } => {
                ui.heading(title.as_str());
                ui.add_space(4.0);
                ui.label(body.as_str());
                let (ok, cancel) = buttons(ui, "OK", true, false);
                if ok {
                    actions.push(action.clone());
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
            Dialog::NewTag { rev, name, message, push } => {
                ui.heading(format!("{} New tag", icon::TAG));
                ui.label(format!("At change {}", &rev[..rev.len().min(10)]));
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(name).hint_text("e.g. v1.2 or milestone-3").desired_width(f32::INFINITY));
                ui.label("Message (optional)");
                ui.add(egui::TextEdit::singleline(message).desired_width(f32::INFINITY));
                ui.checkbox(push, "Upload the tag to the server");
                let invalid = name.contains(' ') || name.trim().is_empty();
                let (ok, cancel) = buttons(ui, "Create tag", !invalid, false);
                if ok {
                    let (n, r, m, p) = (name.trim().to_string(), rev.clone(), message.clone(), *push);
                    let remote = self.remote.clone().or_else(|| self.repo.default_remote());
                    self.run_simple("Creating tag", move |repo| {
                        repo.create_tag(&n, &r, &m)?;
                        if p {
                            if let Some(rm) = &remote {
                                repo.push_tag(rm, &n)?;
                                return Ok(format!("Created and uploaded tag {n}"));
                            }
                        }
                        Ok(format!("Created tag {n}"))
                    });
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
            Dialog::AddRemote { name, url } => {
                ui.heading(format!("{} Connect to a server", icon::CLOUD));
                ui.label("Paste the repository URL from GitHub / GitLab / your studio server.");
                ui.label("URL");
                ui.add(egui::TextEdit::singleline(url).hint_text("https://github.com/team/project.git").desired_width(f32::INFINITY));
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(name).desired_width(200.0));
                let (ok, cancel) = buttons(ui, "Connect", !url.trim().is_empty() && !name.trim().is_empty(), false);
                if ok {
                    match self.repo.add_remote(name.trim(), url.trim()) {
                        Ok(()) => {
                            self.toast("Connected. Press Push to upload your branch.");
                            actions.push(Action::Refresh);
                        }
                        Err(e) => self.error(e.to_string()),
                    }
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
            Dialog::Identity { name, email, global } => {
                ui.heading("Who are you?");
                ui.label("Git records a name and email on every change you submit.");
                egui::Grid::new("identity").num_columns(2).show(ui, |ui| {
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(name).hint_text("Jane Doe").desired_width(300.0));
                    ui.end_row();
                    ui.label("Email");
                    ui.add(egui::TextEdit::singleline(email).hint_text("jane@studio.com").desired_width(300.0));
                    ui.end_row();
                });
                ui.checkbox(global, "Use for all repositories on this computer");
                let valid = !name.trim().is_empty() && email.contains('@');
                let (ok, cancel) = buttons(ui, "Save", valid, false);
                if ok {
                    match self.repo.set_identity(name, email, *global) {
                        Ok(()) => {
                            self.identity = self.repo.identity();
                            self.toast("Saved. You're ready to submit.");
                        }
                        Err(e) => self.error(e.to_string()),
                    }
                    return Outcome::Close;
                }
                if cancel {
                    return Outcome::Close;
                }
            }
        }
        Outcome::Keep
    }
}
