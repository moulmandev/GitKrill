//! The commit graph: ref labels, lanes and commit messages, fully virtualised
//! so only the visible rows are laid out and painted.

pub mod drag;
mod ghost;
pub mod reach;
pub mod search;

use std::sync::Arc;

use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, Key, Painter, Popup, PopupCloseBehavior, Pos2,
    Rangef, Rect, Response, ScrollArea, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::git::graph::SegKind;
use crate::git::ops::{self, ResetMode};
use crate::git::{CommitInfo, RefKind, RefLabel, RepoSnapshot, RowKind, short};
use crate::prefs::graph::Description;
use crate::state::{AppAction, Dialog, RepoTab, Settings, Shared, format_time};
use crate::theme::{self, icons};
use crate::ui::sidebar;
use crate::ui::widgets::{self, elided, elided_pair, menu_item};

use self::drag::{Branch, Target};

pub const ROW_H: f32 = 28.0;
const HEADER_H: f32 = 24.0;
const LANE_W: f32 = 22.0;
const NODE_R: f32 = 10.0;
const LINE_W: f32 = 2.0;
/// Space between the graph column's left edge and the centre of lane 0.
const LANE_PAD: f32 = 16.0;

/// Where each column sits. Hidden columns (Preferences > Commit Graph) are
/// `None`, or for the two on the left, empty ranges.
pub struct Columns {
    refs: Rangef,
    graph: Rangef,
    message: Option<Rangef>,
    author: Option<Rangef>,
    date: Option<Rangef>,
    sha: Option<Rangef>,
    /// Branch and tag labels are shown.
    show_refs: bool,
    /// Lanes and commit nodes are shown.
    show_tree: bool,
}

impl Columns {
    fn new(left: f32, right: f32, refs_w: f32, graph_w: f32, settings: &Settings) -> Self {
        let prefs = &settings.graph;
        let mut end = right;
        let mut take = |on: bool, width: f32| {
            on.then(|| {
                let range = Rangef::new(end - width, end);
                end -= width;
                range
            })
        };
        let sha = take(settings.show_sha, 76.0);
        let date = take(settings.show_date, 136.0);
        let author = take(settings.show_author, 150.0);

        let refs = Rangef::new(left, left + if prefs.show_refs { refs_w } else { 0.0 });
        let graph = Rangef::new(refs.max, refs.max + if prefs.show_tree { graph_w } else { 0.0 });
        let message = prefs.show_message.then(|| Rangef::new(graph.max, end.max(graph.max + 60.0)));
        let mut cols = Columns { refs, graph, message, author, date, sha, show_refs: prefs.show_refs, show_tree: prefs.show_tree };
        if message.is_none() {
            // Without the message, the other columns follow the graph.
            let shift = (end - graph.max).max(0.0);
            for range in [&mut cols.author, &mut cols.date, &mut cols.sha].into_iter().flatten() {
                *range = Rangef::new(range.min - shift, range.max - shift);
            }
        }
        cols
    }
}

fn auto_graph_width(max_lane: u16) -> f32 {
    LANE_PAD * 2.0 + (max_lane.min(9) as f32) * LANE_W
}

pub fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared) {
    drag::overlay(ui, tab, shared);
    let Some(snap) = tab.snap.clone() else {
        match &tab.error {
            Some(e) => widgets::placeholder(ui, e),
            None => widgets::placeholder(ui, "Loading repository…"),
        }
        return;
    };
    let history = snap.history.clone();
    let rows = history.commits.len();
    let prefs = shared.settings.graph;
    tab.set_initial_limit(prefs.limit());

    ui.spacing_mut().item_spacing.y = 0.0;
    crate::ui::bisect::banner(ui, tab, shared, &snap);
    crate::ui::solo::bar(ui, tab, shared, &snap);
    if tab.search_open {
        search::bar(ui, tab, &snap);
    }

    let avail = ui.available_rect_before_wrap();
    let bar = ui.spacing().scroll.bar_width;
    let auto_w = auto_graph_width(history.graph.max_lane);
    let refs_w = shared.settings.refs_width.clamp(70.0, 420.0);
    let graph_w = shared
        .settings
        .graph_width
        .unwrap_or(auto_w)
        .clamp(LANE_PAD * 2.0, (avail.width() * 0.6).max(LANE_PAD * 2.0));
    let cols = Columns::new(avail.left(), avail.right() - bar, refs_w, graph_w, &shared.settings);

    let header_rect = Rect::from_min_size(avail.min, vec2(avail.width(), HEADER_H));
    ui.allocate_rect(header_rect, Sense::hover());
    header(ui, header_rect, &cols, shared, refs_w, graph_w);

    if rows == 0 {
        widgets::placeholder(ui, "No commits yet. Stage some files and make the first one.");
        return;
    }

    // Arrow keys walk the graph unless a text field has the keyboard.
    if shared.dialog.is_none() && !ui.ctx().egui_wants_keyboard_input() {
        let step = ui.input(|i| {
            i.key_pressed(Key::ArrowDown) as i64 - i.key_pressed(Key::ArrowUp) as i64
        });
        if step != 0 {
            let current = tab.selected_row().unwrap_or(0) as i64;
            let next = (current + step).clamp(0, rows as i64 - 1) as usize;
            // Shift extends the selection (`ui::multi`).
            crate::ui::multi::arrow(ui, tab, next);
        }
    }

    let query = search::is_active(tab);
    let selected_row = tab.selected_row();
    let file_count = snap.status.file_count();
    // A branch being dragged, from here or from the sidebar.
    let session = drag::session(ui.ctx());

    ScrollArea::vertical().id_salt("commit-graph").auto_shrink([false, false]).show_viewport(
        ui,
        |ui, viewport| {
            let extra = if history.has_more { ROW_H } else { 0.0 };
            let (content, _) = ui.allocate_exact_size(
                vec2(ui.available_width(), rows as f32 * ROW_H + extra),
                Sense::hover(),
            );
            let row_rect = |i: usize| {
                Rect::from_min_size(
                    pos2(content.left(), content.top() + i as f32 * ROW_H),
                    vec2(content.width(), ROW_H),
                )
            };

            if let Some(row) = tab.scroll_to.take() {
                let top = row as f32 * ROW_H;
                let visible = top >= viewport.min.y && top + ROW_H <= viewport.max.y;
                if !visible {
                    // Step by a row when adjacent, centre when jumping.
                    let near = top + ROW_H * 4.0 > viewport.min.y && top - ROW_H * 3.0 < viewport.max.y;
                    ui.scroll_to_rect(row_rect(row), (!near).then_some(Align::Center));
                }
            }

            let first = (viewport.min.y / ROW_H).floor().max(0.0) as usize;
            let last = ((viewport.max.y / ROW_H).ceil() as usize + 1).min(rows);
            let painter = ui.painter().clone();

            if session.is_some() {
                scroll_near_edges(ui);
            }

            // The commits of the branch under the pointer, if one is: a label
            // here (found before any row is painted) or a sidebar row.
            let highlight = prefs.highlight_branch && session.is_none();
            let label = if highlight { hovered_branch_label(ui, &painter, &snap, &cols, row_rect, first..last) } else { None };
            let reach = tab.ref_hover.reached(ui.ctx(), &history, label, highlight);

            for i in first..last {
                let commit = &history.commits[i];
                let rect = row_rect(i);
                let response = ui.interact(rect, ui.id().with(("row", commit.oid)), Sense::click());

                let missed = query && tab.search_hits.binary_search(&(i as u32)).is_err();
                let reached = reach.as_ref().map(|rows| rows.contains(i));
                let labels: &[RefLabel] = match commit.kind {
                    RowKind::Commit if cols.show_refs => snap.refs_by_oid.get(&commit.oid).map_or(&[], Vec::as_slice),
                    _ => &[],
                };
                let pills = layout_pills(&painter, rect, &cols, labels);

                // Every label is a widget of its own on top of the row, so it
                // can be clicked, dragged and dropped on by itself.
                let refs_clip = Rect::from_x_y_ranges(cols.refs, rect.y_range());
                let pill_responses: Vec<Response> = pills
                    .shown
                    .iter()
                    .map(|pill| {
                        let sense = match labels[pill.label].kind {
                            RefKind::Local | RefKind::Remote => Sense::click_and_drag(),
                            RefKind::Tag | RefKind::Head => Sense::click(),
                        };
                        let id = ui.id().with(("pill", commit.oid, pill.label));
                        ui.interact(pill.rect.intersect(refs_clip), id, sense)
                    })
                    .collect();

                // While a branch is dragged: is this row, or one of its
                // labels, where it would land?
                let mut landing = Landing::None;
                let mut lifted = None;
                if let Some(session) = &session {
                    lifted = pills.shown.iter().position(|pill| session.branch().is_label(&labels[pill.label]));
                    let over_pill = pill_responses.iter().position(Response::contains_pointer);
                    if over_pill.is_some() || response.contains_pointer() {
                        let on_branch = over_pill
                            .and_then(|p| Some((p, Branch::of_label(&snap, &labels[pills.shown[p].label])?)))
                            .map(|(p, branch)| (Landing::Pill(p), Target::Branch(branch)));
                        // A label that cannot take the branch (a tag, the
                        // branch itself) still sits on a commit that might.
                        let on_commit = (Landing::Row, Target::Commit(commit.oid));
                        let accepted = [on_branch, Some(on_commit)]
                            .into_iter()
                            .flatten()
                            .find(|(_, target)| session.accepts(&snap, target));
                        if let Some((place, target)) = accepted {
                            landing = place;
                            session.over(ui.ctx(), tab, &snap, target);
                        }
                    }
                }

                // The faded "branch" and "tag" offers, on the row under the
                // pointer and on the selected one.
                let pointer_in = session.is_none() && ui.rect_contains_pointer(rect);
                let selected = selected_row == Some(i);
                let offer = prefs.ghost_refs && commit.kind == RowKind::Commit && session.is_none() && (selected || pointer_in);
                let ghosts = if offer && cols.show_refs { ghost::layout(&painter, rect, &cols, &pills) } else { [None, None] };
                let ghost_responses = ghosts.each_ref().map(|ghost| {
                    ghost.as_ref().map(|ghost| ui.interact(ghost.rect, ui.id().with(("ghost", commit.oid, ghost.tag)), Sense::click()))
                });

                // Merge commits are drawn as a dot, without an avatar.
                let picture = match commit.kind {
                    RowKind::Commit if commit.parents.len() <= 1 => {
                        shared.avatars.get(&commit.email, shared.settings.fetch_avatars)
                    }
                    _ => None,
                };
                let look = RowLook {
                    selected: selected || tab.multi.contains(commit.oid),
                    // Nothing but the landing place reacts during a drag.
                    hovered: session.is_none()
                        && (response.hovered()
                            || pill_responses.iter().any(Response::hovered)
                            || ghost_responses.iter().flatten().any(Response::hovered)),
                    dim: missed || reached == Some(false),
                    bright: reached == Some(true),
                    description: prefs.shows_description(pointer_in),
                    file_count,
                    picture,
                    landing,
                    lifted,
                    bisect: crate::ui::bisect::badge(tab, &snap, commit.oid),
                };
                paint_row(&painter, rect, &cols, &snap, i, &pills, &look);

                let color = theme::lane_color(history.graph.row(i).color);
                let ghost_hovered = ghost_responses.each_ref().map(|r| r.as_ref().is_some_and(Response::hovered));
                ghost::paint(&painter, rect, &cols, &ghosts, ghost_hovered, color);
                for (ghost, ghost_response) in ghosts.iter().flatten().zip(ghost_responses.into_iter().flatten()) {
                    let what = if ghost.tag { "tag" } else { "branch" };
                    let ghost_response = ghost_response
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .on_hover_text(format!("Create a {what} at {}", short(commit.oid)));
                    if ghost_response.clicked() {
                        shared.show(ghost::dialog(commit.oid, ghost.tag));
                    }
                }

                if response.double_clicked() {
                    checkout_row(tab, &snap, commit);
                } else if response.clicked() {
                    crate::ui::multi::click(ui, tab, i);
                }
                response.context_menu(|ui| row_menu(ui, tab, shared, &snap, i));

                // Labels that did not fit are listed on hover.
                let hidden = labels.len() > pills.shown.len();
                if hidden {
                    let over_labels = response
                        .hover_pos()
                        .is_some_and(|pos| pos.x < cols.refs.max);
                    if over_labels {
                        response.on_hover_ui_at_pointer(|ui| list_labels(ui, labels));
                    }
                }

                // A label acts for its own ref: double-click checks it out,
                // right-click opens its menu, dragging carries the branch.
                for (pill, pill_response) in pills.shown.iter().zip(pill_responses) {
                    let label = &labels[pill.label];
                    if pill_response.double_clicked() {
                        checkout_label(tab, &snap, commit, label);
                    } else if pill_response.clicked() {
                        crate::ui::multi::click(ui, tab, i);
                    }
                    drag::source(&pill_response, || Branch::of_label(&snap, label), color);
                    pill_response.context_menu(|ui| label_menu(ui, tab, shared, &snap, i, label));
                    if hidden {
                        pill_response.on_hover_ui_at_pointer(|ui| list_labels(ui, labels));
                    }
                }
            }

            if history.has_more {
                let rect = row_rect(rows);
                if prefs.lazy_load || tab.loading_more() {
                    painter.text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "Loading older commits…",
                        theme::font(12.0),
                        theme::text_faint(),
                    );
                    if prefs.lazy_load && last >= rows {
                        tab.load_more();
                    }
                } else {
                    // Lazy loading is off: more history only when asked.
                    let response = ui
                        .interact(rect, ui.id().with("load-more"), Sense::click())
                        .on_hover_cursor(CursorIcon::PointingHand);
                    let color = if response.hovered() { theme::text_bright() } else { theme::accent() };
                    let text = format!("{}  Load more commits", icons::ARROW_DOWN);
                    painter.text(rect.center(), Align2::CENTER_CENTER, text, theme::font(12.5), color);
                    if response.clicked() {
                        tab.load_more();
                    }
                }
            }
        },
    );
}

/// The commit whose branch label (local or remote) is under the pointer,
/// among the visible rows. Only the row under the pointer has its labels laid
/// out, an extra time.
fn hovered_branch_label(
    ui: &Ui,
    painter: &Painter,
    snap: &RepoSnapshot,
    cols: &Columns,
    row_rect: impl Fn(usize) -> Rect,
    visible: std::ops::Range<usize>,
) -> Option<crate::git::Oid> {
    if !cols.show_refs {
        return None;
    }
    let pos = ui.ctx().pointer_hover_pos().filter(|pos| cols.refs.contains(pos.x))?;
    let top = row_rect(0).top();
    let row = ((pos.y - top) / ROW_H).floor();
    let row = (row >= 0.0).then_some(row as usize).filter(|row| visible.contains(row))?;
    let commit = &snap.history.commits[row];
    let labels = snap.refs_by_oid.get(&commit.oid).filter(|_| commit.kind == RowKind::Commit)?;
    let pills = layout_pills(painter, row_rect(row), cols, labels);
    let on_branch = pills.shown.iter().any(|pill| {
        matches!(labels[pill.label].kind, RefKind::Local | RefKind::Remote)
            && ui.rect_contains_pointer(pill.rect.intersect(Rect::from_x_y_ranges(cols.refs, pill.rect.y_range())))
    });
    // The label sits on its branch's tip.
    on_branch.then_some(commit.oid)
}

/// The tooltip for a row whose labels do not all fit.
fn list_labels(ui: &mut Ui, labels: &[RefLabel]) {
    for label in labels {
        let icon = match label.kind {
            RefKind::Tag => icons::TAG,
            RefKind::Remote => icons::CLOUD,
            RefKind::Local | RefKind::Head => icons::GIT_BRANCH,
        };
        ui.label(format!("{icon}  {}", label.name));
    }
}

/// Scrolls the graph while something is dragged close to its top or bottom
/// edge, so a drop target outside the view can be reached.
fn scroll_near_edges(ui: &Ui) {
    const EDGE: f32 = 32.0;
    /// Points per second at the very edge.
    const SPEED: f32 = 900.0;
    let view = ui.clip_rect();
    let Some(pos) = ui.ctx().pointer_latest_pos().filter(|pos| view.contains(*pos)) else { return };
    // Positive moves the content down, which brings earlier rows into view.
    let push = if pos.y < view.top() + EDGE {
        (view.top() + EDGE - pos.y) / EDGE
    } else if pos.y > view.bottom() - EDGE {
        -(pos.y - (view.bottom() - EDGE)) / EDGE
    } else {
        return;
    };
    let dt = ui.input(|i| i.stable_dt).min(0.05);
    ui.scroll_with_delta_animation(vec2(0.0, push * SPEED * dt), egui::style::ScrollAnimation::none());
    ui.ctx().request_repaint();
}

fn header(ui: &mut Ui, rect: Rect, cols: &Columns, shared: &mut Shared, refs_w: f32, graph_w: f32) {
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, theme::bg_header());
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));

    let label = |x: f32, text: &str| {
        painter.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, text, theme::font(10.5), theme::text_dim());
    };
    if cols.show_refs {
        label(cols.refs.min + 10.0, "BRANCH  /  TAG");
    }
    // A single lane leaves no room for the word.
    if cols.show_tree && cols.graph.span() >= 56.0 {
        label(cols.graph.min + 8.0, "GRAPH");
    }
    if let Some(range) = cols.message {
        label(range.min + 12.0, "COMMIT MESSAGE");
    }
    if let Some(range) = cols.author {
        label(range.min + 8.0, "AUTHOR");
    }
    if let Some(range) = cols.date {
        label(range.min + 8.0, "DATE");
    }
    if let Some(range) = cols.sha {
        label(range.min + 8.0, "SHA");
    }

    // Draggable column edges. Double-click the graph edge to auto-size again.
    let settings = &mut shared.settings;
    let edges = [(cols.refs.max, false), (cols.graph.max, true)];
    for (x, is_graph) in edges.into_iter().filter(|(_, is_graph)| if *is_graph { cols.show_tree } else { cols.show_refs }) {
        let handle = Rect::from_center_size(pos2(x, rect.center().y), vec2(9.0, rect.height()));
        let response = ui
            .interact(handle, ui.id().with(("column-edge", is_graph)), Sense::click_and_drag())
            .on_hover_and_drag_cursor(CursorIcon::ResizeHorizontal);
        if response.dragged() {
            let delta = response.drag_delta().x;
            if is_graph {
                settings.graph_width = Some((graph_w + delta).max(LANE_PAD * 2.0));
            } else {
                settings.refs_width = (refs_w + delta).clamp(70.0, 420.0);
            }
        }
        if response.double_clicked() && is_graph {
            settings.graph_width = None;
        }
        let active = response.hovered() || response.dragged();
        painter.vline(
            x,
            rect.y_range(),
            Stroke::new(1.0, if active { theme::accent() } else { theme::border_light() }),
        );
    }

    let gear = Rect::from_min_max(pos2(rect.right() - 26.0, rect.top()), rect.max);
    let response = ui.interact(gear, ui.id().with("graph-columns"), Sense::click());
    painter.text(
        gear.center(),
        Align2::CENTER_CENTER,
        icons::GEAR,
        theme::font(14.0),
        if response.hovered() { theme::text_bright() } else { theme::text_dim() },
    );
    Popup::menu(&response).close_behavior(PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        ui.set_min_width(170.0);
        let caption = |ui: &mut Ui, text: &str| ui.label(egui::RichText::new(text).color(theme::text_dim()).size(11.0));
        caption(ui, "Columns");
        ui.checkbox(&mut settings.graph.show_refs, "Branch / Tag");
        ui.checkbox(&mut settings.graph.show_tree, "Graph");
        ui.checkbox(&mut settings.graph.show_message, "Commit message");
        ui.checkbox(&mut settings.show_author, "Author");
        ui.checkbox(&mut settings.show_date, "Date");
        ui.checkbox(&mut settings.show_sha, "SHA");
        ui.separator();
        caption(ui, "Description");
        ui.add_enabled_ui(settings.graph.show_message, |ui| {
            ui.horizontal(|ui| {
                for (value, text) in Description::CHOICES {
                    ui.selectable_value(&mut settings.graph.description, value, text);
                }
            });
        });
        ui.separator();
        if ui.button("Reset column widths").clicked() {
            settings.graph_width = None;
            settings.refs_width = Settings::default().refs_width;
        }
        if ui.button("Commit graph preferences…").clicked() {
            shared.actions.push(AppAction::OpenPreferences(crate::ui::prefs::Section::Graph));
            ui.close();
        }
    });
}

/// Where on a row a dragged branch would land.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Landing {
    None,
    /// On the commit itself.
    Row,
    /// On the shown label with this index.
    Pill(usize),
}

/// How a row looks this frame, beyond what the snapshot says about it.
struct RowLook {
    selected: bool,
    hovered: bool,
    /// Faded: not a match of the current search, or not on the branch the
    /// pointer is on.
    dim: bool,
    /// On the branch the pointer is on.
    bright: bool,
    /// The commit's description follows its summary.
    description: bool,
    /// Changed files, for the WIP row.
    file_count: usize,
    /// The author's picture, once it is loaded.
    picture: Option<egui::TextureId>,
    landing: Landing,
    /// The shown label that is being dragged away.
    lifted: Option<usize>,
    /// What the commit is to a bisect in progress (good, bad, testing...).
    bisect: Option<crate::ui::bisect::Badge>,
}

/// The COMMIT MESSAGE column: the summary, then the description when the
/// row shows it.
fn paint_message(painter: &Painter, rect: Rect, cols: &Columns, commit: &CommitInfo, look: &RowLook, color: Color32) {
    let Some(message) = cols.message else { return };
    let cy = rect.center().y;
    let x0 = message.min;
    painter.rect_filled(
        Rect::from_min_max(pos2(x0, rect.top() + 2.0), pos2(x0 + 2.0, rect.bottom() - 2.0)),
        0.0,
        color,
    );
    let mut text_x = x0 + 12.0;
    let mut max_w = message.max - text_x - 8.0;
    let mp = painter.with_clip_rect(
        Rect::from_x_y_ranges(message, rect.y_range()).intersect(painter.clip_rect()),
    );

    match commit.kind {
        RowKind::Wip => {
            let galley = mp.layout_no_wrap("// WIP".to_owned(), theme::font(13.0), theme::text_dim());
            let field = Rect::from_min_size(
                pos2(text_x - 4.0, cy - 10.0),
                vec2((galley.size().x + 60.0).min(max_w), 20.0),
            );
            mp.rect_filled(field, 3.0, theme::well());
            widgets::galley_left(&mp, rect, text_x + 4.0, galley);
            let x = field.right() + 12.0;
            let w = widgets::text_left(&mp, rect, x, icons::PENCIL_SIMPLE, theme::font(14.0), theme::yellow());
            widgets::text_left(&mp, rect, x + w + 5.0, &look.file_count.to_string(), theme::font(13.0), theme::text());
        }
        RowKind::Stash => {
            let galley = elided(&mp, &commit.summary, theme::font(13.0), theme::text_dim(), max_w);
            widgets::galley_left(&mp, rect, text_x, galley);
        }
        RowKind::Commit => {
            if let Some(badge) = &look.bisect {
                let used = crate::ui::bisect::paint_badge(&mp, rect, text_x, badge) + 6.0;
                text_x += used;
                max_w -= used;
            }
            let summary_color = if look.selected || look.bright { theme::text_bright() } else { theme::text() };
            let body = if look.description { commit.body_preview.as_str() } else { "" };
            let galley = elided_pair(
                &mp,
                (&commit.summary, theme::font(13.0), summary_color),
                (body, theme::font(13.0), theme::text_faint()),
                max_w,
            );
            widgets::galley_left(&mp, rect, text_x, galley);
        }
    }
}

fn paint_row(
    base: &Painter,
    rect: Rect,
    cols: &Columns,
    snap: &RepoSnapshot,
    index: usize,
    pills: &Pills,
    look: &RowLook,
) {
    let RowLook { selected, hovered, dim, picture, landing, lifted, .. } = *look;
    let history = &snap.history;
    let commit = &history.commits[index];
    let row = history.graph.row(index);
    let (lane, segs) = (row.lane, row.segs);
    let color = theme::lane_color(row.color);

    widgets::row_background(base, rect, selected, hovered);

    let mut painter = base.clone();
    if dim {
        painter.set_opacity(0.3);
    }

    let cy = rect.center().y;
    let lane_x = |l: u16| (cols.graph.min + LANE_PAD + l as f32 * LANE_W).round();
    let node = pos2(lane_x(lane), cy);

    let graph_rect = Rect::from_x_y_ranges(cols.graph, rect.y_range());
    let gp = painter.with_clip_rect(graph_rect.intersect(painter.clip_rect()));

    // Tinted band from the node to the message, in the lane's colour.
    gp.rect_filled(
        Rect::from_min_max(pos2(node.x, rect.top() + 1.0), pos2(cols.graph.max, rect.bottom() - 1.0)),
        0.0,
        theme::with_alpha(color, if selected { 70 } else { 34 }),
    );

    let label_end =
        (!pills.shown.is_empty()).then(|| paint_pills(&painter, rect, cols, pills, color, landing, lifted));
    if let Some(end) = label_end.filter(|_| cols.show_tree) {
        // Connector from the label to its node, underneath the lanes.
        painter
            .with_clip_rect(
                Rect::from_x_y_ranges(Rangef::new(cols.refs.min, cols.graph.max), rect.y_range())
                    .intersect(painter.clip_rect()),
            )
            .line_segment([pos2(end, cy), node], Stroke::new(1.5, color));
    }

    for seg in segs {
        let x = lane_x(seg.lane);
        let stroke = Stroke::new(LINE_W, theme::lane_color(seg.color));
        match seg.kind {
            SegKind::Through => {
                gp.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], stroke);
            }
            SegKind::In if seg.lane == lane => {
                gp.line_segment([pos2(x, rect.top()), node], stroke);
            }
            SegKind::Out if seg.lane == lane => {
                gp.line_segment([node, pos2(x, rect.bottom())], stroke);
            }
            SegKind::In => {
                gp.add(Shape::line(elbow(x, rect.top(), node, true), stroke));
            }
            SegKind::Out => {
                gp.add(Shape::line(elbow(x, rect.bottom(), node, false), stroke));
            }
        }
    }

    match commit.kind {
        RowKind::Wip => {
            gp.circle_filled(node, NODE_R, theme::bg_app());
            gp.extend(dashed_circle(node, NODE_R - 1.0, Stroke::new(1.5, color)));
        }
        RowKind::Stash => {
            let tile = Rect::from_center_size(node, vec2(NODE_R * 2.0 - 2.0, NODE_R * 2.0 - 2.0));
            gp.rect(tile, 4.0, theme::bg_app(), Stroke::new(1.5, color), StrokeKind::Inside);
            gp.text(node, Align2::CENTER_CENTER, icons::ARCHIVE, theme::font(12.0), color);
        }
        RowKind::Commit if commit.parents.len() > 1 => {
            gp.circle_filled(node, 5.0, color);
        }
        RowKind::Commit => {
            widgets::avatar_picture(&gp, picture, node, NODE_R, &commit.author, &commit.email, Some(color));
        }
    }

    paint_message(&painter, rect, cols, commit, look, color);

    // ---- optional columns ----
    if commit.kind != RowKind::Wip {
        let cell = |range: Option<Rangef>, text: &str, font: egui::FontId| {
            let Some(range) = range else { return };
            let galley = elided(&painter, text, font, theme::text_dim(), range.span() - 14.0);
            widgets::galley_left(&painter, rect, range.min + 8.0, galley);
        };
        cell(cols.author, &commit.author, theme::font(12.5));
        if cols.date.is_some() {
            cell(cols.date, &format_time(commit.time), theme::font(12.0));
        }
        if cols.sha.is_some() {
            cell(cols.sha, &short(commit.oid), theme::mono(12.0));
        }
    }

    if landing == Landing::Row {
        drag::highlight_row(base, rect);
    }
}

/// Points for a rounded corner joining lane `x` (at the row edge `edge_y`) to
/// the node. `from_top` is true for lines arriving from the row above.
fn elbow(x: f32, edge_y: f32, node: Pos2, from_top: bool) -> Vec<Pos2> {
    let dir = (node.x - x).signum();
    let r = ((node.x - x).abs()).min(ROW_H / 2.0 - 2.0).min(LANE_W / 2.0);
    let vertical = if from_top { -1.0 } else { 1.0 };
    // Corner sits on the lane at the node's height, rounded with radius r.
    let center = pos2(x + dir * r, node.y + vertical * r);

    let mut points = Vec::with_capacity(10);
    points.push(pos2(x, edge_y));
    const STEPS: usize = 6;
    for step in 0..=STEPS {
        let angle = std::f32::consts::FRAC_PI_2 * step as f32 / STEPS as f32;
        points.push(pos2(
            center.x - dir * r * angle.cos(),
            center.y - vertical * r * angle.sin(),
        ));
    }
    points.push(node);
    points
}

fn dashed_circle(center: Pos2, radius: f32, stroke: Stroke) -> Vec<Shape> {
    const DASHES: usize = 10;
    let step = std::f32::consts::TAU / DASHES as f32;
    (0..DASHES)
        .map(|i| {
            let a0 = i as f32 * step;
            let arc: Vec<Pos2> = (0..=3)
                .map(|k| {
                    let a = a0 + step * 0.55 * k as f32 / 3.0;
                    center + radius * vec2(a.cos(), a.sin())
                })
                .collect();
            Shape::line(arc, stroke)
        })
        .collect()
}

/// One ref label laid out in the BRANCH / TAG column.
pub struct Pill {
    /// Index into the row's label list.
    pub label: usize,
    pub rect: Rect,
    style: PillStyle,
    name: std::sync::Arc<egui::Galley>,
    glyphs: std::sync::Arc<egui::Galley>,
}

/// The labels of one row that fit, plus how many did not.
pub struct Pills {
    pub shown: Vec<Pill>,
    hidden: usize,
}

const PILL_PAD: f32 = 7.0;
const PILL_GAP: f32 = 5.0;
/// Room kept for the "+2" overflow counter.
const MORE_W: f32 = 26.0;

/// Places as many labels as fit, left to right. The first one is always
/// shown (elided if need be); the rest only when they fit whole.
pub fn layout_pills(painter: &Painter, rect: Rect, cols: &Columns, labels: &[RefLabel]) -> Pills {
    let mut shown = Vec::new();
    let cy = rect.center().y;
    let mut x = cols.refs.min + 8.0;
    let limit = cols.refs.max - 10.0;

    for (index, label) in labels.iter().enumerate() {
        let mut glyphs = String::new();
        match label.kind {
            RefKind::Local => {
                glyphs.push_str(icons::LAPTOP);
                if label.synced_remote {
                    glyphs.push(' ');
                    glyphs.push_str(icons::CLOUD);
                }
            }
            RefKind::Remote => glyphs.push_str(icons::CLOUD),
            RefKind::Tag => glyphs.push_str(icons::TAG),
            RefKind::Head => {}
        }
        let (text_color, glyph_color) = pill_text_colors(label);
        let glyphs = painter.layout_no_wrap(glyphs, theme::font(13.0), glyph_color);
        let glyph_w = if glyphs.is_empty() { 0.0 } else { glyphs.size().x + 6.0 };

        let text = if label.is_head { format!("{} {}", icons::CHECK, label.name) } else { label.name.clone() };
        let font = if label.is_head { theme::bold(12.5) } else { theme::font(12.5) };
        let later = labels.len() - index - 1;
        let reserve = if later > 0 { MORE_W } else { 0.0 };
        let budget = limit - x - reserve - glyph_w - PILL_PAD * 2.0;

        let name = if index == 0 {
            elided(painter, &text, font, text_color, budget.max(24.0))
        } else {
            let whole = painter.layout_no_wrap(text, font, text_color);
            if whole.size().x > budget {
                break;
            }
            whole
        };

        let width = PILL_PAD * 2.0 + name.size().x + glyph_w;
        let pill = Rect::from_min_size(pos2(x, cy - 10.0), vec2(width, 20.0));
        x = pill.right() + PILL_GAP;
        let style = match label.kind {
            _ if label.is_head => PillStyle::Head,
            RefKind::Local | RefKind::Head => PillStyle::Local,
            RefKind::Remote | RefKind::Tag => PillStyle::Outline,
        };
        shown.push(Pill { label: index, rect: pill, style, name, glyphs });
    }

    let hidden = labels.len() - shown.len();
    Pills { shown, hidden }
}

fn pill_text_colors(label: &RefLabel) -> (Color32, Color32) {
    match label.kind {
        _ if label.is_head => (theme::on_accent(), theme::on_accent()),
        RefKind::Local | RefKind::Head => (theme::text_bright(), theme::text()),
        RefKind::Remote => (theme::text(), theme::text_dim()),
        RefKind::Tag => (theme::text(), theme::yellow()),
    }
}

/// Paints a row's ref pills and returns the x where they end.
///
/// The kinds are told apart by shape as well as colour: the checked-out
/// branch is a solid, bright pill with a tick; other local branches are a
/// muted fill; remote branches and tags are outlines.
///
/// `landing` and `lifted` are for a branch drag in progress: the label the
/// branch would land on is outlined, the one being dragged away is faded.
fn paint_pills(
    painter: &Painter,
    rect: Rect,
    cols: &Columns,
    pills: &Pills,
    color: Color32,
    landing: Landing,
    lifted: Option<usize>,
) -> f32 {
    let clip = Rect::from_x_y_ranges(cols.refs, rect.y_range()).intersect(painter.clip_rect());
    let whole = painter.with_clip_rect(clip);
    let cy = rect.center().y;
    let mut end = cols.refs.min;

    for (index, pill) in pills.shown.iter().enumerate() {
        let mut p = whole.clone();
        if lifted == Some(index) {
            p.multiply_opacity(0.35);
        }
        match pill.style {
            PillStyle::Head => {
                p.rect_filled(pill.rect.expand(1.5), 4.5, theme::with_alpha(color, 90));
                p.rect_filled(pill.rect, 3.0, theme::deep(color));
            }
            PillStyle::Local => {
                p.rect_filled(pill.rect, 3.0, theme::mix(theme::bg_app(), color, 0.34));
            }
            PillStyle::Outline => {
                p.rect(
                    pill.rect,
                    3.0,
                    theme::mix(theme::bg_app(), color, 0.08),
                    Stroke::new(1.0, theme::mix(theme::bg_app(), color, 0.75)),
                    StrokeKind::Inside,
                );
            }
        }
        let name_w = widgets::galley_left(&p, pill.rect, pill.rect.left() + PILL_PAD, pill.name.clone());
        if !pill.glyphs.is_empty() {
            widgets::galley_left(&p, pill.rect, pill.rect.left() + PILL_PAD + name_w + 6.0, pill.glyphs.clone());
        }
        if landing == Landing::Pill(index) {
            drag::highlight_pill(&whole, pill.rect);
        }
        end = pill.rect.right();
    }

    if pills.hidden > 0 {
        let used = whole.text(
            pos2(end + 5.0, cy),
            Align2::LEFT_CENTER,
            format!("+{}", pills.hidden),
            theme::font(11.0),
            theme::text_dim(),
        );
        end = used.right() + 4.0;
    }
    end.min(cols.refs.max)
}

#[derive(Clone, Copy)]
enum PillStyle {
    Head,
    Local,
    Outline,
}

/// Double-click: check out whatever branch sits on the row.
fn checkout_row(tab: &mut RepoTab, snap: &RepoSnapshot, commit: &CommitInfo) {
    if commit.kind != RowKind::Commit {
        return;
    }
    let Some(labels) = snap.refs_by_oid.get(&commit.oid) else { return };
    if let Some(local) = labels.iter().find(|l| l.kind == RefKind::Local && !l.is_head) {
        tab.run(ops::checkout(&local.name));
    } else if labels.iter().any(|l| l.is_head) {
        // Already here.
    } else if let Some(remote) = labels.iter().find(|l| l.kind == RefKind::Remote)
        && let Some(branch) = snap.remote_branches.iter().find(|b| b.full == remote.name) {
            tab.run(sidebar::checkout_remote(snap, branch));
        }
}

/// Double-click on a label: check out that very ref.
fn checkout_label(tab: &mut RepoTab, snap: &RepoSnapshot, commit: &CommitInfo, label: &RefLabel) {
    match label.kind {
        RefKind::Local if label.is_head => {}
        RefKind::Local => tab.run(ops::checkout(&label.name)),
        RefKind::Remote => {
            if let Some(branch) = snap.remote_branches.iter().find(|b| b.full == label.name) {
                tab.run(sidebar::checkout_remote(snap, branch));
            }
        }
        // Not branches: do what the row does.
        RefKind::Tag | RefKind::Head => checkout_row(tab, snap, commit),
    }
}

/// Right-click on a label: the menu of that ref alone.
fn label_menu(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    snap: &Arc<RepoSnapshot>,
    index: usize,
    label: &RefLabel,
) {
    match label.kind {
        RefKind::Local => {
            if let Some(branch) = snap.branches.iter().find(|b| b.name == label.name) {
                return sidebar::local_branch_menu(ui, tab, shared, snap, branch);
            }
        }
        RefKind::Remote => {
            if let Some(branch) = snap.remote_branches.iter().find(|b| b.full == label.name) {
                return sidebar::remote_branch_menu(ui, tab, shared, snap, branch);
            }
        }
        RefKind::Tag => return tag_menu(ui, tab, shared, snap, &label.name),
        RefKind::Head => {}
    }
    row_menu(ui, tab, shared, snap, index);
}

fn tag_menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, name: &str) {
    ui.set_min_width(180.0);
    if let Some(remote) = snap.default_remote()
        && menu_item(ui, icons::ARROW_LINE_UP, &format!("Push to {remote}")) {
            tab.run(ops::push_tag(remote, name));
        }
    let tip = snap.tags.iter().find(|t| t.name == name).map(|t| t.oid);
    crate::ui::multi::compare_item(ui, tab, shared, snap, tip);
    if menu_item(ui, icons::TRASH, "Delete tag") {
        shared.show(Dialog::confirm(
            "Delete tag",
            format!("Delete the tag \"{name}\" from this repository?"),
            "Delete",
            ops::delete_tag(name),
        ));
    }
    ui.separator();
    let path = crate::git::hidden::tag(name);
    let is_hidden = tab.hidden.hides(&path);
    if sidebar::hide_item(ui, is_hidden, "in graph") {
        tab.set_hidden(&mut shared.settings, &path, !is_hidden);
    }
}

fn row_menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>, index: usize) {
    ui.set_min_width(230.0);
    let commit = &snap.history.commits[index];

    match commit.kind {
        RowKind::Wip => {
            if menu_item(ui, icons::TRAY_ARROW_DOWN, "Stash all changes") {
                crate::ui::stash::open(shared);
            }
            if menu_item(ui, icons::PLUS, "Stage all changes") {
                tab.run(ops::stage_all());
            }
            ui.separator();
            if menu_item(ui, icons::TRASH, "Discard all changes") {
                shared.show(discard_all_dialog(snap));
            }
            return;
        }
        RowKind::Stash => {
            if let Some(stash) = snap.stashes.iter().find(|s| s.oid == commit.oid) {
                sidebar::stash_menu(ui, tab, shared, stash.index);
            }
            return;
        }
        RowKind::Commit => {}
    }
    // One of several selected commits: the menu for all of them.
    if crate::ui::multi::menu(ui, tab, snap, commit.oid) {
        return;
    }

    let sha = commit.oid.to_string();
    let abbrev = short(commit.oid);
    let is_head = snap.head.oid == Some(commit.oid);
    let is_merge = commit.parents.len() > 1;
    let labels: &[RefLabel] = snap.refs_by_oid.get(&commit.oid).map(Vec::as_slice).unwrap_or(&[]);

    // Branches and tags sitting on this commit get their own submenus.
    let mut any_ref = false;
    for label in labels {
        match label.kind {
            RefKind::Local => {
                if let Some(branch) = snap.branches.iter().find(|b| b.name == label.name) {
                    any_ref = true;
                    ui.menu_button(format!("{}  {}", icons::GIT_BRANCH, label.name), |ui| {
                        sidebar::local_branch_menu(ui, tab, shared, snap, branch);
                    });
                }
            }
            RefKind::Remote => {
                if let Some(branch) = snap.remote_branches.iter().find(|b| b.full == label.name) {
                    any_ref = true;
                    ui.menu_button(format!("{}  {}", icons::CLOUD, label.name), |ui| {
                        sidebar::remote_branch_menu(ui, tab, shared, snap, branch);
                    });
                }
            }
            RefKind::Tag => {
                any_ref = true;
                ui.menu_button(format!("{}  {}", icons::TAG, label.name), |ui| {
                    tag_menu(ui, tab, shared, snap, &label.name);
                });
            }
            RefKind::Head => {}
        }
    }
    if any_ref {
        ui.separator();
    }

    if !is_head && menu_item(ui, icons::GIT_COMMIT, "Checkout this commit") {
        tab.run(ops::checkout(&sha));
    }
    if menu_item(ui, icons::GIT_BRANCH, "Create branch here") {
        shared.show(Dialog::CreateBranch {
            at: Some(sha.clone()),
            at_label: abbrev.clone(),
            name: String::new(),
            checkout: true,
        });
    }
    if menu_item(ui, icons::TAG, "Create tag here") {
        shared.show(Dialog::CreateTag {
            at: sha.clone(),
            at_label: abbrev.clone(),
            name: String::new(),
            message: String::new(),
        });
    }
    ui.separator();

    if !is_head && menu_item(ui, icons::GIT_COMMIT, "Cherry pick commit") {
        tab.run(ops::cherry_pick(commit.oid, is_merge));
    }
    if menu_item(ui, icons::ARROW_COUNTER_CLOCKWISE, "Revert commit") {
        tab.run(ops::revert(commit.oid, is_merge));
    }
    crate::ui::multi::compare_with_head(ui, tab, shared, snap, commit.oid);
    // Reword, squash, reorder or drop everything after this commit.
    if !is_head
        && let Some(branch) = &snap.head.branch
        && menu_item(ui, icons::LIST_NUMBERS, &format!("Interactive rebase {branch} from here"))
    {
        tab.overlay = Some(crate::state::Overlay::Rebase(crate::ui::rebase::RebaseView::new(commit.oid)));
    }
    if !is_head {
        let target = snap.head_label();
        ui.menu_button(format!("{}  Reset {target} to this commit", icons::ARROW_LINE_DOWN), |ui| {
            ui.set_min_width(260.0);
            if ui.button("Soft - keep all changes staged").clicked() {
                tab.run(ops::reset(&sha, ResetMode::Soft));
            }
            if ui.button("Mixed - keep changes in the working tree").clicked() {
                tab.run(ops::reset(&sha, ResetMode::Mixed));
            }
            if ui.button("Hard - discard all changes").clicked() {
                shared.show(Dialog::confirm(
                    "Hard reset",
                    format!(
                        "Reset {target} to {abbrev} and discard every uncommitted change? \
                         This cannot be undone."
                    ),
                    "Reset",
                    ops::reset(&sha, ResetMode::Hard),
                ));
            }
        });
    }
    crate::ui::bisect::menu_items(ui, tab, snap, commit.oid);
    ui.separator();

    if menu_item(ui, icons::COPY, "Copy commit SHA") {
        ui.ctx().copy_text(sha.clone());
    }
    if menu_item(ui, icons::COPY, "Copy commit message") {
        ui.ctx().copy_text(commit.summary.clone());
    }
}

pub fn discard_all_dialog(snap: &RepoSnapshot) -> Dialog {
    Dialog::confirm(
        "Discard all changes",
        format!(
            "Discard all changes to {} file(s), including untracked files? This cannot be undone.",
            snap.status.file_count()
        ),
        "Discard all",
        ops::discard_all(),
    )
}

/// Opens the search bar above the graph with the cursor in it (Ctrl+F).
pub fn open_search(tab: &mut RepoTab) {
    tab.search_open = true;
    tab.search_focus = true;
    tab.diff = None;
}
