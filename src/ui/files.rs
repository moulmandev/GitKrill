//! The changed-files list used for unstaged files, staged files and the files
//! of a commit, as either a flat path list or a folder tree.

use eframe::egui::{self, Align2, Rect, ScrollArea, Sense, Ui, pos2, vec2};

use crate::external::{self, Compare};
use crate::git::{ChangeKind, DiffSource, DiffTarget, FileChange, ops};
use crate::platform;
use crate::state::{Dialog, Overlay, RepoTab, Shared};
use crate::ui::conflict::ConflictView;
use crate::ui::history::HistoryView;
use crate::theme::{self, icons};
use crate::ui::widgets::{self, elided, kind_icon, menu_item};

pub const ROW_H: f32 = 24.0;
const INDENT: f32 = 14.0;

enum Row<'a> {
    Dir { name: &'a str, path: String, depth: usize, collapsed: bool },
    File { file: &'a FileChange, depth: usize, flat: bool },
}

fn source_key(source: DiffSource) -> &'static str {
    match source {
        DiffSource::Unstaged => "unstaged",
        DiffSource::Staged => "staged",
        DiffSource::Commit(_) => "commit",
    }
}

fn build_rows<'a>(files: &'a [FileChange], tree: bool, tab: &RepoTab, source: DiffSource) -> Vec<Row<'a>> {
    if !tree {
        return files.iter().map(|file| Row::File { file, depth: 0, flat: true }).collect();
    }

    let mut sorted: Vec<&FileChange> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    let mut rows = Vec::with_capacity(sorted.len() + 8);
    let mut stack: Vec<&str> = Vec::new();
    let mut hidden_from: Option<usize> = None;

    for file in sorted {
        let mut parts: Vec<&str> = file.path.split('/').collect();
        parts.pop();

        let common = stack.iter().zip(&parts).take_while(|(a, b)| a == b).count();
        stack.truncate(common);
        if hidden_from.is_some_and(|d| d >= common) {
            hidden_from = None;
        }
        for (depth, part) in parts.iter().enumerate().skip(common) {
            stack.push(*part);
            if hidden_from.is_some() {
                continue;
            }
            let path = stack.join("/");
            let collapsed = tab.collapsed.contains(&format!("files:{}:{path}", source_key(source)));
            rows.push(Row::Dir { name: part, path, depth, collapsed });
            if collapsed {
                hidden_from = Some(depth);
            }
        }
        if hidden_from.is_none() {
            rows.push(Row::File { file, depth: parts.len(), flat: false });
        }
    }
    rows
}

/// Number of rows the list would need, for sharing space between lists.
pub fn row_count(files: &[FileChange], tree: bool, tab: &RepoTab, source: DiffSource) -> usize {
    if tree { build_rows(files, true, tab, source).len() } else { files.len() }
}

pub fn file_list(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    files: &[FileChange],
    source: DiffSource,
    height: f32,
) {
    let tree = shared.settings.tree_view;
    let rows = build_rows(files, tree, tab, source);
    let open_path = tab
        .diff
        .as_ref()
        .filter(|view| view.target.source == source)
        .map(|view| view.target.path.clone());

    // Mutations are collected and applied after the loop: the rows borrow the tab.
    let mut toggle_dir: Option<String> = None;
    let mut open: Option<DiffTarget> = None;
    let mut run: Option<ops::GitCmd> = None;
    let mut overlay: Option<Overlay> = None;
    let unborn = tab.snap.as_ref().is_some_and(|s| s.head.oid.is_none());
    let workdir = tab.path.clone();

    ui.spacing_mut().item_spacing.y = 0.0;
    ScrollArea::vertical()
        .id_salt(("file-list", source_key(source)))
        .max_height(height)
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for row in &rows[range] {
                let (rect, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                let painter = ui.painter().clone();
                let y = rect.center().y;

                match row {
                    Row::Dir { name, path, depth, collapsed } => {
                        widgets::row_background(&painter, rect, false, response.hovered());
                        let x = rect.left() + 10.0 + *depth as f32 * INDENT;
                        let caret = if *collapsed { icons::CARET_RIGHT } else { icons::CARET_DOWN };
                        painter.text(pos2(x, y), Align2::LEFT_CENTER, caret, theme::font(10.0), theme::text_dim());
                        let icon = if *collapsed { icons::FOLDER } else { icons::FOLDER_OPEN };
                        painter.text(pos2(x + 14.0, y), Align2::LEFT_CENTER, icon, theme::font(14.0), theme::text_dim());
                        let galley = elided(&painter, name, theme::font(13.0), theme::text_dim(), rect.right() - x - 44.0);
                        widgets::galley_left(&painter, rect, x + 34.0, galley);

                        if response.clicked() {
                            toggle_dir = Some(format!("files:{}:{path}", source_key(source)));
                        }
                        let inside = || -> Vec<String> {
                            let prefix = format!("{path}/");
                            files.iter().filter(|f| f.path.starts_with(&prefix)).map(|f| f.path.clone()).collect()
                        };
                        response.context_menu(|ui| match source {
                            DiffSource::Unstaged => {
                                if menu_item(ui, icons::PLUS, "Stage folder") {
                                    run = Some(ops::stage(&inside()));
                                }
                            }
                            DiffSource::Staged => {
                                if menu_item(ui, icons::MINUS, "Unstage folder") {
                                    run = Some(ops::unstage(&inside(), unborn));
                                }
                            }
                            DiffSource::Commit(_) => {
                                if menu_item(ui, icons::COPY, "Copy folder path") {
                                    ui.ctx().copy_text(path.clone());
                                }
                            }
                        });
                    }

                    Row::File { file, depth, flat } => {
                        let selected = open_path.as_deref() == Some(file.path.as_str());
                        widgets::row_background(&painter, rect, selected, response.hovered());

                        let x = rect.left() + 10.0 + *depth as f32 * INDENT + if *flat { 0.0 } else { 14.0 };
                        let (icon, color) = kind_icon(file.kind);
                        painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), color);

                        // The action button appears on hover, so reserve its room then.
                        let action = match source {
                            DiffSource::Unstaged => Some(("Stage File", theme::green())),
                            DiffSource::Staged => Some(("Unstage File", theme::red())),
                            DiffSource::Commit(_) => None,
                        };
                        let hovering = ui.rect_contains_pointer(rect);
                        let mut text_right = rect.right() - 8.0;
                        if let (Some((label, tint)), true) = (action, hovering) {
                            let size = widgets::tint_button_size(ui, label);
                            let button = Rect::from_min_size(
                                pos2(rect.right() - size.x - 6.0, y - size.y / 2.0),
                                size,
                            );
                            let pressed = widgets::tint_button_at(ui, button, label, tint).clicked();
                            if pressed {
                                run = Some(match source {
                                    DiffSource::Unstaged => ops::stage(std::slice::from_ref(&file.path)),
                                    _ => ops::unstage(std::slice::from_ref(&file.path), unborn),
                                });
                            }
                            text_right = button.left() - 8.0;
                        }

                        let text_x = x + 22.0;
                        let (dir, name) = match file.path.rfind('/') {
                            Some(i) if *flat => (&file.path[..=i], &file.path[i + 1..]),
                            Some(i) => ("", &file.path[i + 1..]),
                            None => ("", file.path.as_str()),
                        };
                        paint_path(&painter, rect, text_x, text_right, dir, name, selected);

                        if response.clicked() {
                            if file.kind == ChangeKind::Conflicted && source == DiffSource::Unstaged {
                                // Conflicts open in the resolver, not the diff.
                                overlay = Some(Overlay::Conflict(ConflictView::new(&file.path)));
                            } else {
                                open = Some(DiffTarget::new(source, file));
                            }
                        }
                        let hover_text = match &file.old_path {
                            Some(old) => format!("{old}  {}  {}", icons::ARROW_RIGHT, file.path),
                            None => format!("{} ({})", file.path, widgets::kind_name(file.kind)),
                        };
                        response.on_hover_text(hover_text).context_menu(|ui| {
                            ui.set_min_width(190.0);
                            let one = std::slice::from_ref(&file.path);
                            match source {
                                DiffSource::Unstaged => {
                                    let stage = if file.kind == ChangeKind::Conflicted {
                                        "Mark as resolved"
                                    } else {
                                        "Stage file"
                                    };
                                    if menu_item(ui, icons::PLUS, stage) {
                                        run = Some(ops::stage(one));
                                    }
                                    if file.kind == ChangeKind::Conflicted
                                        && menu_item(ui, icons::GIT_MERGE, "Open in merge tool")
                                    {
                                        run = external::merge(&shared.settings.tools, &file.path);
                                    }
                                    if file.kind != ChangeKind::Conflicted
                                        && menu_item(ui, icons::TRASH, "Discard changes")
                                    {
                                        let what = if file.kind == ChangeKind::Untracked {
                                            format!("Delete the untracked file \"{}\"?", file.path)
                                        } else {
                                            format!("Discard all unstaged changes to \"{}\"?", file.path)
                                        };
                                        shared.show(Dialog::confirm(
                                            "Discard changes",
                                            format!("{what} This cannot be undone."),
                                            "Discard",
                                            ops::discard(std::slice::from_ref(*file)),
                                        ));
                                    }
                                    ui.separator();
                                }
                                DiffSource::Staged => {
                                    if menu_item(ui, icons::MINUS, "Unstage file") {
                                        run = Some(ops::unstage(one, unborn));
                                    }
                                    ui.separator();
                                }
                                DiffSource::Commit(_) => {}
                            }
                            let full = workdir.join(&file.path);
                            let tools = &shared.settings.tools;
                            // Untracked files have nothing to compare with;
                            // conflicts go to the merge tool.
                            let comparable = !matches!(file.kind, ChangeKind::Untracked | ChangeKind::Conflicted);
                            if comparable && menu_item(ui, icons::GIT_DIFF, "Open in diff tool") {
                                let compare = compare_for(tab, source);
                                external::diff(tools, &workdir, compare, &file.path, file.old_path.as_deref());
                            }
                            if file.kind != ChangeKind::Deleted
                                && tools.has_editor()
                                && menu_item(ui, icons::CODE, "Open in editor")
                            {
                                external::edit(tools, &full);
                            }
                            if file.kind != ChangeKind::Deleted {
                                if menu_item(ui, icons::FILE_TEXT, "Open file") {
                                    platform::open(&full);
                                }
                                if menu_item(ui, icons::FOLDER_OPEN, "Show in folder") {
                                    platform::reveal(&full);
                                }
                            }
                            if menu_item(ui, icons::CLOCK_COUNTER_CLOCKWISE, "File history") {
                                overlay = Some(Overlay::History(HistoryView::history(&file.path)));
                            }
                            if file.kind != ChangeKind::Untracked
                                && menu_item(ui, icons::USER_LIST, "Blame")
                            {
                                overlay = Some(Overlay::History(HistoryView::blame(&file.path)));
                            }
                            if menu_item(ui, icons::COPY, "Copy file path") {
                                ui.ctx().copy_text(file.path.clone());
                            }
                        });
                    }
                }
            }
        });

    if let Some(key) = toggle_dir
        && !tab.collapsed.remove(&key) {
            tab.collapsed.insert(key);
        }
    if let Some(target) = open {
        tab.open_diff(target);
    }
    if let Some(cmd) = run {
        tab.run(cmd);
    }
    if let Some(overlay) = overlay {
        tab.overlay = Some(overlay);
    }
}

/// What "Open in diff tool" compares for the files of `source`: a commit
/// with its first parent.
fn compare_for(tab: &RepoTab, source: DiffSource) -> Compare {
    match source {
        DiffSource::Unstaged => Compare::Unstaged,
        DiffSource::Staged => Compare::Staged,
        DiffSource::Commit(oid) => {
            let details = tab.details.as_ref().and_then(|d| d.as_ref().ok()).filter(|d| d.oid == oid);
            let parent = match details {
                Some(details) => details.parents.first().map(|parent| parent.to_string()),
                None => Some(format!("{oid}^")),
            };
            Compare::Commit { commit: oid.to_string(), parent }
        }
    }
}

/// `dir/` dimmed followed by the file name. When the path does not fit, the
/// start of the directory is cut so the file name always stays visible.
fn paint_path(
    painter: &egui::Painter,
    rect: Rect,
    left: f32,
    right: f32,
    dir: &str,
    name: &str,
    selected: bool,
) {
    let name_color = if selected { theme::text_bright() } else { theme::text() };
    let width = (right - left).max(0.0);
    let clip = Rect::from_min_max(pos2(left, rect.top()), pos2(right, rect.bottom()));
    let p = painter.with_clip_rect(clip.intersect(painter.clip_rect()));

    let name_galley = p.layout_no_wrap(name.to_owned(), theme::font(13.0), name_color);
    if dir.is_empty() || name_galley.size().x >= width {
        let galley = elided(&p, name, theme::font(13.0), name_color, width);
        widgets::galley_left(&p, rect, left, galley);
        return;
    }

    let dir_galley = p.layout_no_wrap(dir.to_owned(), theme::font(13.0), theme::text_dim());
    let total = dir_galley.size().x + name_galley.size().x;
    if total <= width {
        let w = widgets::galley_left(&p, rect, left, dir_galley);
        widgets::galley_left(&p, rect, left + w, name_galley);
    } else {
        // Right-align and mark the cut with an ellipsis.
        let ellipsis = p.layout_no_wrap("…".to_owned(), theme::font(13.0), theme::text_dim());
        let start = right - total;
        let cut = Rect::from_min_max(
            pos2(left + ellipsis.size().x + 1.0, rect.top()),
            pos2(right, rect.bottom()),
        );
        let clipped = p.with_clip_rect(cut.intersect(p.clip_rect()));
        let w = widgets::galley_left(&clipped, rect, start, dir_galley);
        widgets::galley_left(&clipped, rect, start + w, name_galley);
        widgets::galley_left(&p, rect, left, ellipsis);
    }
}
