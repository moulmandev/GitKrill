//! Every file of a commit, in the commit details panel: "All files" beside
//! the changed ones above its file list. Folders open and close as in the
//! changed-file tree (Path / Tree applies here too), a filter finds a file by
//! part of its path, and only the rows in view are painted, so trees of a
//! hundred thousand files scroll as smoothly as small ones. Files the commit
//! changed keep their change icon.
//!
//! Clicking a file shows it as it is in that commit (`DiffSource::At`, see
//! `ui::file_view`); its right-click menu opens, saves or restores that
//! version (`ui::versions`), and starts its history or blame there. The
//! tree is read on the reader thread (`git::versions::tree`) once per
//! commit; which folders are open is kept from one commit to the next.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use eframe::egui::{self, Align2, ScrollArea, Sense, TextEdit, Ui, pos2, vec2};

use crate::git::versions::{EntryKind, Tree};
use crate::git::{ChangeKind, DiffSource, DiffTarget, FileChange, Oid, short};
use crate::state::{Overlay, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::files::{ROW_H, paint_path};
use crate::ui::history::HistoryView;
use crate::ui::versions::{self, Version};
use crate::ui::widgets::{self, elided, kind_icon, menu_item};

const TREE_JOB: &str = "browse:tree";
const INDENT: f32 = 14.0;

/// What `rows` was worked out from.
#[derive(Clone, PartialEq, Eq)]
struct RowsKey {
    commit: Oid,
    generation: u64,
    filter: String,
    flat: bool,
}

/// The browser of one tab (`RepoTab::browse`).
#[derive(Default)]
pub struct Browse {
    /// The details list every file of the commit, not the changed ones.
    pub on: bool,
    /// The commit whose tree was asked for, and the answer once it came.
    asked: Option<Oid>,
    tree: Option<Result<Arc<Tree>, String>>,
    /// Folders that are open, by path.
    open: HashSet<String>,
    /// Bumped whenever `open` changes.
    generation: u64,
    filter: String,
    /// Indices into the tree's entries of the rows shown.
    rows: Option<(RowsKey, Arc<Vec<u32>>)>,
}

impl Browse {
    fn toggle(&mut self, path: &str) {
        if !self.open.remove(path) {
            self.open.insert(path.to_owned());
        }
        self.generation += 1;
    }

    /// Opens (or closes) the folder at `index` and every folder inside it.
    fn set_all(&mut self, tree: &Tree, index: usize, open: bool) {
        let EntryKind::Dir { end } = tree.entries[index].kind else { return };
        for entry in &tree.entries[index..end as usize] {
            if entry.is_dir() {
                if open {
                    self.open.insert(entry.path.clone());
                } else {
                    self.open.remove(&entry.path);
                }
            }
        }
        self.generation += 1;
    }
}

/// The rows to show: the files whose path contains `filter` (any case),
/// every file by path when `flat`, otherwise the tree with closed folders'
/// contents left out.
fn rows(tree: &Tree, open: &HashSet<String>, filter: &str, flat: bool) -> Vec<u32> {
    if !filter.is_empty() {
        let filter = filter.to_lowercase();
        return tree
            .by_path
            .iter()
            .copied()
            .filter(|i| tree.entries[*i as usize].path.to_lowercase().contains(&filter))
            .collect();
    }
    if flat {
        return tree.by_path.clone();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < tree.entries.len() {
        out.push(i as u32);
        i = match tree.entries[i].kind {
            EntryKind::Dir { end } if !open.contains(&tree.entries[i].path) => end as usize,
            _ => i + 1,
        };
    }
    out
}

/// Something clicked in the list, done once it is drawn.
enum Act {
    Toggle(String),
    SetAll(usize, bool),
    Open(String),
    Overlay(Overlay),
    Version(versions::Act),
}

/// The list of every file of `commit`, filling `height`. `changed` are the
/// files the commit changed, marked in the list.
pub fn list(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, commit: Oid, changed: &[FileChange], height: f32) {
    load(tab, commit);
    let top = ui.available_rect_before_wrap().top();

    // The filter, and how many files there are.
    let mut filter = std::mem::take(&mut tab.browse.filter);
    egui::Frame::new().inner_margin(egui::Margin { left: 10, right: 10, top: 2, bottom: 6 }).show(ui, |ui| {
        ui.add(
            TextEdit::singleline(&mut filter)
                .hint_text(format!("{}  Filter files of {}", icons::MAGNIFYING_GLASS, short(commit)))
                .desired_width(f32::INFINITY),
        );
    });
    tab.browse.filter = filter;
    let height = height - (ui.available_rect_before_wrap().top() - top);

    let tree = match &tab.browse.tree {
        Some(Ok(tree)) => tree.clone(),
        Some(Err(e)) => return widgets::placeholder(ui, e),
        None => {
            let rect = ui.available_rect_before_wrap();
            widgets::spinner(ui.painter(), pos2(rect.center().x, rect.top() + 30.0), 8.0, theme::text_dim());
            ui.ctx().request_repaint();
            return;
        }
    };

    let flat = !shared.settings.tree_view;
    let key = RowsKey {
        commit,
        generation: tab.browse.generation,
        filter: tab.browse.filter.trim().to_owned(),
        flat,
    };
    let shown = match &tab.browse.rows {
        Some((at, rows)) if *at == key => rows.clone(),
        _ => {
            let rows = Arc::new(rows(&tree, &tab.browse.open, &key.filter, flat));
            tab.browse.rows = Some((key.clone(), rows.clone()));
            rows
        }
    };
    if shown.is_empty() {
        let text = if key.filter.is_empty() { "This commit has no files." } else { "No file matches the filter." };
        return widgets::placeholder(ui, text);
    }
    // A filtered list shows paths, like the flat one.
    let flat = flat || !key.filter.is_empty();

    let kinds: HashMap<&str, ChangeKind> = changed.iter().map(|file| (file.path.as_str(), file.kind)).collect();
    let open_path = tab
        .diff
        .as_ref()
        .filter(|view| matches!(view.target.source, DiffSource::At(oid) | DiffSource::Commit(oid) if oid == commit))
        .map(|view| view.target.path.clone());
    let mut act = None;

    ui.spacing_mut().item_spacing.y = 0.0;
    ScrollArea::vertical()
        .id_salt("browse-files")
        .max_height(height.max(ROW_H))
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, shown.len(), |ui, range| {
            for index in shown[range].iter().map(|i| *i as usize) {
                let entry = &tree.entries[index];
                let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                let painter = ui.painter().clone();
                let y = rect.center().y;
                let depth = if flat { 0.0 } else { entry.depth as f32 };
                let x = rect.left() + 10.0 + depth * INDENT;

                if let EntryKind::Dir { .. } = entry.kind {
                    let open = tab.browse.open.contains(&entry.path);
                    widgets::row_background(&painter, rect, false, response.hovered());
                    let caret = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
                    painter.text(pos2(x, y), Align2::LEFT_CENTER, caret, theme::font(10.0), theme::text_dim());
                    let icon = if open { icons::FOLDER_OPEN } else { icons::FOLDER };
                    painter.text(pos2(x + 14.0, y), Align2::LEFT_CENTER, icon, theme::font(14.0), theme::text_dim());
                    let galley = elided(&painter, entry.name(), theme::font(13.0), theme::text_dim(), rect.right() - x - 44.0);
                    widgets::galley_left(&painter, rect, x + 34.0, galley);
                    if response.clicked() {
                        act = Some(Act::Toggle(entry.path.clone()));
                    }
                    response.context_menu(|ui| {
                        ui.set_min_width(190.0);
                        if menu_item(ui, icons::CARET_DOWN, "Open all folders inside") {
                            act = Some(Act::SetAll(index, true));
                        }
                        if menu_item(ui, icons::CARET_RIGHT, "Close all folders inside") {
                            act = Some(Act::SetAll(index, false));
                        }
                        if menu_item(ui, icons::COPY, "Copy folder path") {
                            ui.ctx().copy_text(entry.path.clone());
                        }
                    });
                    continue;
                }

                let selected = open_path.as_deref() == Some(entry.path.as_str());
                widgets::row_background(&painter, rect, selected, response.hovered());
                let x = x + if flat { 0.0 } else { 14.0 };
                let kind = kinds.get(entry.path.as_str()).copied();
                let (icon, color) = match (entry.kind, kind) {
                    (EntryKind::Submodule(_), _) => (icons::CUBE, theme::text_dim()),
                    (_, Some(kind)) => kind_icon(kind),
                    (EntryKind::Link, None) => (icons::LINK_SIMPLE, theme::text_faint()),
                    _ => (icons::FILE, theme::text_faint()),
                };
                painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), color);
                let dir = if flat { &entry.path[..entry.name_at] } else { "" };
                paint_path(&painter, rect, x + 22.0, rect.right() - 8.0, dir, entry.name(), selected);

                let hover = match (entry.kind, kind) {
                    (EntryKind::Submodule(at), _) => format!("{}: a submodule, at commit {}", entry.path, short(at)),
                    (_, Some(kind)) => format!("{} ({} in this commit)", entry.path, widgets::kind_name(kind)),
                    (EntryKind::Link, None) => format!("{} (a symbolic link)", entry.path),
                    _ => entry.path.clone(),
                };
                let submodule = matches!(entry.kind, EntryKind::Submodule(_));
                if response.clicked() && !submodule {
                    act = Some(Act::Open(entry.path.clone()));
                }
                response.on_hover_text(hover).context_menu(|ui| {
                    ui.set_min_width(200.0);
                    if !submodule {
                        let version = Version::new(commit, &entry.path);
                        if let Some(chosen) = versions::menu_items(ui, shared, &version, "this version") {
                            act = Some(Act::Version(chosen));
                        }
                        ui.separator();
                        if menu_item(ui, icons::CLOCK_COUNTER_CLOCKWISE, "File history") {
                            let view = HistoryView::history(&entry.path).starting_at(Some(commit));
                            act = Some(Act::Overlay(Overlay::History(view)));
                        }
                        if menu_item(ui, icons::USER_LIST, "Blame") {
                            let view = HistoryView::blame(&entry.path).starting_at(Some(commit));
                            act = Some(Act::Overlay(Overlay::History(view)));
                        }
                    }
                    if menu_item(ui, icons::COPY, "Copy file path") {
                        ui.ctx().copy_text(entry.path.clone());
                    }
                });
            }
        });

    match act {
        Some(Act::Toggle(path)) => tab.browse.toggle(&path),
        Some(Act::SetAll(index, open)) => tab.browse.set_all(&tree, index, open),
        Some(Act::Open(path)) => tab.open_diff(DiffTarget { source: DiffSource::At(commit), path, old_path: None }),
        Some(Act::Overlay(overlay)) => tab.overlay = Some(overlay),
        Some(Act::Version(chosen)) => versions::apply(tab, shared, chosen),
        None => {}
    }
}

/// "Show all files of the commit" (or its changed files again), while one
/// commit's details are shown.
pub fn palette_entries(list: &mut Vec<crate::ui::palette::Entry>, tab: &RepoTab) {
    let crate::state::Selection::Commit(oid) = tab.selection else { return };
    if tab.multi.is_active() {
        return;
    }
    let (icon, label) = if tab.browse.on {
        (icons::GIT_DIFF, "Show the files this commit changed")
    } else {
        (icons::FILES, "Show all files of this commit")
    };
    list.push(
        crate::ui::palette::Entry::on_tab("browse-files", icon, label, |tab, _| tab.browse.on = !tab.browse.on)
            .detail(short(oid))
            .keywords("browse tree every file whole repository snapshot view"),
    );
}

/// The bar above a commit's files, `rect`: changed files or all of them on
/// the left, Path / Tree on the right. Labels get shorter when the panel is
/// narrow, so the two never run into each other.
pub fn header(ui: &mut Ui, rect: egui::Rect, tab: &mut RepoTab, shared: &mut Shared, commit: Oid, changed: usize) {
    let all = match count(tab, commit) {
        Some(n) if tab.browse.on => format!("All {n}"),
        _ => "All files".to_owned(),
    };
    // Measured as `widgets::segmented` sizes its halves.
    let width = |labels: [&str; 2]| {
        labels
            .iter()
            .map(|label| ui.painter().layout_no_wrap(format!("{} {label}", icons::FILES), theme::font(12.0), theme::text()).size().x + 18.0)
            .sum::<f32>()
    };
    let room = rect.width() - 16.0 - width(["Path", "Tree"]) - 8.0;
    let full = format!("{changed} changed");
    let count = changed.to_string();
    let choices = [[full.as_str(), all.as_str()], [full.as_str(), "All"], [count.as_str(), "All"]];
    let [first, second] = choices.into_iter().find(|labels| width(*labels) <= room).unwrap_or([&count, "All"]);
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| widgets::segmented(ui, &mut tab.browse.on, (icons::GIT_DIFF, first), (icons::FILES, second)),
    );
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 0.0))).layout(egui::Layout::right_to_left(egui::Align::Center)),
        |ui| widgets::segmented(ui, &mut shared.settings.tree_view, (icons::LIST, "Path"), (icons::TREE_VIEW, "Tree")),
    );
}

/// How many files the commit has, once its tree is read.
pub fn count(tab: &RepoTab, commit: Oid) -> Option<usize> {
    match &tab.browse.tree {
        Some(Ok(tree)) if tree.commit == commit => Some(tree.by_path.len()),
        _ => None,
    }
}

/// Asks for the tree of `commit` unless it is the one shown or asked for,
/// and collects the answer.
fn load(tab: &mut RepoTab, commit: Oid) {
    if let Some((oid, result)) = tab.take_result::<(Oid, Result<Tree, String>)>(TREE_JOB)
        && tab.browse.asked == Some(oid)
    {
        tab.browse.tree = Some(result.map(Arc::new));
    }
    if tab.browse.asked != Some(commit) {
        tab.browse.asked = Some(commit);
        tab.browse.tree = None;
        tab.spawn_job(TREE_JOB, move |loader| (commit, crate::git::versions::tree(loader.repo(), commit)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::versions::Entry;

    fn tree(paths: &[(&str, u16, Option<u32>)]) -> Tree {
        let entries: Vec<Entry> = paths
            .iter()
            .map(|(path, depth, end)| Entry {
                path: (*path).to_owned(),
                name_at: path.rfind('/').map_or(0, |i| i + 1),
                depth: *depth,
                kind: match end {
                    Some(end) => EntryKind::Dir { end: *end },
                    None => EntryKind::File,
                },
            })
            .collect();
        let mut by_path: Vec<u32> = (0..entries.len() as u32).filter(|i| !entries[*i as usize].is_dir()).collect();
        by_path.sort_by(|a, b| entries[*a as usize].path.cmp(&entries[*b as usize].path));
        Tree { commit: Oid::from_bytes(&[0; 20]).unwrap(), entries, by_path }
    }

    #[test]
    fn closed_folders_hide_what_is_inside_them() {
        let tree = tree(&[
            ("src", 0, Some(4)),
            ("src/ui", 1, Some(3)),
            ("src/ui/a.rs", 2, None),
            ("src/main.rs", 1, None),
            ("README.md", 0, None),
        ]);
        let shown = |open: &[&str]| rows(&tree, &open.iter().map(|s| (*s).to_owned()).collect(), "", false);
        assert_eq!(shown(&[]), [0, 4]);
        assert_eq!(shown(&["src"]), [0, 1, 3, 4]);
        assert_eq!(shown(&["src", "src/ui"]), [0, 1, 2, 3, 4]);
        // An open folder inside a closed one stays hidden.
        assert_eq!(shown(&["src/ui"]), [0, 4]);
        // Flat: every file by path; filtered: the paths that match, any case.
        assert_eq!(rows(&tree, &HashSet::new(), "", true), [4, 3, 2]);
        assert_eq!(rows(&tree, &HashSet::new(), "UI/", false), [2]);
    }
}
