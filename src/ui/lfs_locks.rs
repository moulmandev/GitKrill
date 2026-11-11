//! Git LFS file locks in the UI: lock and unlock from a file's right-click
//! menu, a lock on locked files in the changed-file lists, the list of
//! locks in Preferences > LFS, and the palette's entry (`git::lfs_lock`).
//!
//! What the server says is kept per repository here, filled in by a thread
//! of its own (it is network work, never the reader thread's). Whether a
//! repository uses LFS is looked at once, locally; the locks are asked for
//! with each automatic fetch interval while something shows them, when
//! Preferences > LFS opens, and after each lock or unlock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Label, Layout, RichText, Ui, vec2};

use crate::git::internal::Internal;
use crate::git::lfs_lock::{self, Lock, NO_SERVER};
use crate::git::ops::{GitCmd, Step};
use crate::git::{FileChange, RepoSnapshot};
use crate::state::{AppAction, Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::palette::Entry;
use crate::ui::prefs::form;
use crate::ui::widgets::menu_item;

/// What is known of one repository's locks.
#[derive(Clone, Default)]
pub struct Locks {
    /// It uses LFS and git-lfs is installed; `None` until looked at.
    pub uses_lfs: Option<bool>,
    /// The server's last answer; `None` before the first.
    pub list: Option<Result<Arc<Vec<Lock>>, String>>,
    /// A look is under way.
    pub loading: bool,
    /// When the last look started.
    asked: Option<Instant>,
    /// The locks were asked for at least once.
    listed: bool,
}

impl Locks {
    /// The lock on `path`, if the server holds one.
    pub fn on(&self, path: &str) -> Option<&Lock> {
        self.list.as_ref()?.as_ref().ok()?.iter().find(|lock| lock.path == path)
    }

    /// Locking can be offered: LFS is in use, and the remote is not one
    /// known to have no LFS server.
    fn offered(&self) -> bool {
        self.uses_lfs == Some(true) && !matches!(&self.list, Some(Err(e)) if e == NO_SERVER)
    }
}

static BOOK: LazyLock<Mutex<HashMap<PathBuf, Locks>>> = LazyLock::new(Default::default);

fn book() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Locks>> {
    BOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What is known of the locks of the repository at `workdir`, looking
/// again in the background when that is due: whether it uses LFS on first
/// sight, its locks every `minutes` (the automatic fetch interval; 0 asks
/// only on request).
pub fn get(ctx: &egui::Context, workdir: &Path, minutes: u32) -> Locks {
    let mut book = book();
    let entry = book.entry(workdir.to_path_buf()).or_default();
    // A repository found not to use LFS is looked at again only on request
    // (Preferences > LFS does, and is where LFS is set up).
    let due = match entry.asked {
        None => true,
        Some(at) => {
            minutes > 0 && entry.uses_lfs == Some(true) && at.elapsed() >= Duration::from_secs(u64::from(minutes) * 60)
        }
    };
    if due && !entry.loading {
        let list = minutes > 0 || entry.listed;
        start(ctx, workdir, entry, list);
    }
    entry.clone()
}

/// Asks the server again now (and whether the repository uses LFS).
pub fn reload(ctx: &egui::Context, workdir: &Path) {
    let mut book = book();
    let entry = book.entry(workdir.to_path_buf()).or_default();
    if !entry.loading {
        start(ctx, workdir, entry, true);
    }
}

fn start(ctx: &egui::Context, workdir: &Path, entry: &mut Locks, list: bool) {
    entry.loading = true;
    entry.asked = Some(Instant::now());
    entry.listed |= list;
    let (ctx, workdir) = (ctx.clone(), workdir.to_path_buf());
    std::thread::spawn(move || {
        let uses = crate::git::lfs::in_use(&workdir);
        let found = (uses && list).then(|| lfs_lock::list(&workdir).map(Arc::new));
        let mut book = book();
        let entry = book.entry(workdir).or_default();
        entry.uses_lfs = Some(uses);
        if found.is_some() || !uses {
            entry.list = found;
        }
        entry.loading = false;
        drop(book);
        ctx.request_repaint();
    });
}

/// `cmd`, then the locks listed again on the operator thread, so they are
/// up to date when the repository refreshes after it.
fn relisting(mut cmd: GitCmd, workdir: &Path) -> GitCmd {
    let key = workdir.to_path_buf();
    cmd.steps.push(Step::internal(Internal::new("list LFS locks", move |_| {
        let found = lfs_lock::list(&key).map(Arc::new);
        let mut book = book();
        let entry = book.entry(key.clone()).or_default();
        entry.list = Some(found);
        entry.listed = true;
        Ok(String::new())
    })));
    cmd
}

// ---- file lists ---------------------------------------------------------------

/// What the changed-file lists show of locks right now: `None` when the
/// repository does not use LFS or nothing is known yet.
pub fn for_lists(ctx: &egui::Context, tab: &RepoTab, minutes: u32) -> Option<Locks> {
    let locks = get(ctx, &tab.path, minutes);
    (locks.uses_lfs == Some(true)).then_some(locks)
}

/// A line for the tooltip of a locked file's row.
pub fn row_note(locks: Option<&Locks>, path: &str) -> Option<String> {
    let lock = locks?.on(path)?;
    let who = match lock.ours {
        Some(true) => "you".to_owned(),
        _ if lock.owner.is_empty() => "someone".to_owned(),
        _ => lock.owner.clone(),
    };
    Some(format!("{}  Locked in LFS by {who} since {}", icons::LOCK_SIMPLE, lock.when()))
}

/// The lock icon at the right end of a locked file's row, left of `right`;
/// returns where the text must stop.
pub fn paint_mark(painter: &egui::Painter, rect: egui::Rect, right: f32, locks: Option<&Locks>, path: &str) -> f32 {
    let Some(lock) = locks.and_then(|locks| locks.on(path)) else { return right };
    let color = if lock.ours == Some(true) { theme::accent() } else { theme::orange() };
    let used = painter.text(egui::pos2(right, rect.center().y), egui::Align2::RIGHT_CENTER, icons::LOCK_SIMPLE, theme::font(13.0), color);
    used.left() - 6.0
}

/// "Lock file" or "Unlock file" (and "Force unlock…" for someone else's
/// lock) for a file's right-click menu, in a repository that uses LFS.
pub fn file_items(ui: &mut Ui, tab: &RepoTab, shared: &mut Shared, file: &FileChange) -> Option<GitCmd> {
    let has_remote = tab.snap.as_ref().is_some_and(|snap| !snap.remotes.is_empty());
    let locks = get(ui.ctx(), &tab.path, shared.settings.auto_fetch_minutes);
    if !has_remote || !locks.offered() {
        return None;
    }
    let path = file.path.as_str();
    ui.separator();
    let mut run = None;
    match locks.on(path) {
        None => {
            if file.kind != crate::git::ChangeKind::Untracked && menu_item(ui, icons::LOCK_SIMPLE, "Lock file (LFS)") {
                run = Some(relisting(lfs_lock::lock(path), &tab.path));
            }
        }
        Some(lock) if lock.ours == Some(false) => {
            let owner = if lock.owner.is_empty() { "someone else" } else { lock.owner.as_str() };
            if menu_item(ui, icons::LOCK_SIMPLE_OPEN, &format!("Force unlock (locked by {owner})…")) {
                confirm_force(shared, tab, lock);
            }
        }
        Some(lock) => {
            // Ours, or the server could not say: it decides.
            let label = if lock.ours == Some(true) || lock.owner.is_empty() {
                "Unlock file (LFS)".to_owned()
            } else {
                format!("Unlock file (locked by {})", lock.owner)
            };
            if menu_item(ui, icons::LOCK_SIMPLE_OPEN, &label) {
                run = Some(relisting(lfs_lock::unlock(path, false), &tab.path));
            }
        }
    }
    run
}

fn confirm_force(shared: &mut Shared, tab: &RepoTab, lock: &Lock) {
    let owner = if lock.owner.is_empty() { "someone else".to_owned() } else { lock.owner.clone() };
    shared.show(Dialog::confirm(
        "Force unlock",
        format!(
            "\"{}\" is locked by {owner} since {}. Take their lock away? They may be changing the file and lose work \
             when they cannot push it.",
            lock.path,
            lock.when()
        ),
        "Force unlock",
        relisting(lfs_lock::unlock(&lock.path, true), &tab.path),
    ));
}

// ---- Preferences > LFS --------------------------------------------------------

/// The "Locked files" row of Preferences > LFS, shown once LFS is set up:
/// every lock on the server, each with its unlock button. Asks the server
/// when the page opens (`first`), and again when LFS was set up since the
/// repository was last looked at.
pub fn prefs_row(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, first: bool) {
    let mut locks = get(ui.ctx(), &tab.path, shared.settings.auto_fetch_minutes);
    let stale = locks.uses_lfs == Some(false)
        && !locks.loading
        && locks.asked.is_some_and(|at| at.elapsed() > Duration::from_secs(3));
    if first || stale {
        reload(ui.ctx(), &tab.path);
        locks = get(ui.ctx(), &tab.path, shared.settings.auto_fetch_minutes);
    }
    let mut run = None;
    form::row(ui, "Locked files", |ui| {
        match &locks.list {
            None => form::loading(ui),
            Some(Err(problem)) => form::error(ui, problem.trim()),
            Some(Ok(list)) if list.is_empty() => {
                ui.label(RichText::new("No file is locked.").size(13.0).color(theme::text_faint()));
            }
            Some(Ok(list)) => {
                for lock in list.iter() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let width = form::FIELD_WIDTH - 70.0;
                        ui.allocate_ui_with_layout(vec2(width, 24.0), Layout::left_to_right(Align::Center), |ui| {
                            ui.set_width(width);
                            let color = if lock.ours == Some(true) { theme::accent() } else { theme::orange() };
                            ui.label(RichText::new(icons::LOCK_SIMPLE).size(13.0).color(color));
                            let text = RichText::new(&lock.path).font(theme::mono(13.0)).color(theme::text_bright());
                            let who = match lock.ours {
                                Some(true) => format!("you, since {}", lock.when()),
                                _ => format!("{}, since {}", lock.owner, lock.when()),
                            };
                            ui.add(Label::new(text).truncate()).on_hover_text(format!("{}\nLocked by {who}", lock.path));
                        });
                        let (label, color) = if lock.ours == Some(false) { ("Force unlock", theme::red()) } else { ("Unlock", theme::accent()) };
                        if form::button(ui, label, color).clicked() {
                            if lock.ours == Some(false) {
                                confirm_force(shared, tab, lock);
                            } else {
                                run = Some(relisting(lfs_lock::unlock(&lock.path, false), &tab.path));
                            }
                        }
                    });
                    let owner = match lock.ours {
                        Some(true) => "By you".to_owned(),
                        _ if lock.owner.is_empty() => "By someone".to_owned(),
                        _ => format!("By {}", lock.owner),
                    };
                    ui.label(RichText::new(format!("{owner}, since {}", lock.when())).size(11.5).color(theme::text_faint()));
                }
            }
        }
        ui.add_space(4.0);
        ui.add_enabled_ui(!locks.loading, |ui| {
            if form::button(ui, "Refresh", theme::accent()).clicked() {
                reload(ui.ctx(), &tab.path);
            }
        });
        form::help(
            ui,
            "Locks are kept by the LFS server: a locked file can only be pushed by whoever holds its lock. Lock or unlock a file from its right-click menu.",
        );
    });
    if let Some(cmd) = run {
        tab.run(cmd);
    }
}

// ---- command palette ----------------------------------------------------------

pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    let known = book().get(&tab.path).cloned().unwrap_or_default();
    if known.uses_lfs != Some(true) || snap.remotes.is_empty() {
        return;
    }
    let count = known.list.as_ref().and_then(|list| list.as_ref().ok()).map(|list| list.len());
    let detail = match count {
        Some(1) => "1 file locked".to_owned(),
        Some(n) => format!("{n} files locked"),
        None => "Preferences > LFS".to_owned(),
    };
    list.push(
        Entry::new(crate::ui::palette::Kind::Action, "lfs-locks", icons::LOCK_SIMPLE, "Show LFS locks", |env| {
            env.shared.actions.push(AppAction::OpenPreferences(crate::ui::prefs::Section::Lfs))
        })
        .detail(detail)
        .keywords("lfs lock unlock locked files git-lfs"),
    );
}
