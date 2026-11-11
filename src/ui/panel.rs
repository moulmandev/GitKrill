//! Right panel: the staging area for the working tree, or the details of the
//! selected commit.

use std::sync::Arc;

use eframe::egui::{
    self, Align, Align2, Frame, Key, Label, Layout, Margin, Modifiers, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, pos2, vec2,
};

use crate::git::{DiffSource, Oid, RepoSnapshot, RepoState, RowKind, ops, short};
use crate::state::{RepoTab, Selection, Shared, format_time, relative_time};
use crate::theme::{self, icons};
use crate::ui::commit_button;
use crate::ui::files::{self, file_list};
use crate::ui::graph::discard_all_dialog;
use crate::ui::widgets::{self, icon_button, segmented, tint_button};

pub fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared) {
    commit_button::follow_up(ui.ctx(), tab, shared);
    crate::ui::merge::follow_up(ui.ctx(), tab);
    crate::ui::versions::poll(tab, shared);
    let Some(snap) = tab.snap.clone() else { return };
    // Several commits selected: see `ui::multi`.
    if crate::ui::multi::panel(ui, tab, shared, &snap) {
        return;
    }
    match tab.selection {
        Selection::Wip => working_tree(ui, tab, shared, &snap),
        Selection::Commit(oid) => commit(ui, tab, shared, &snap, oid),
        Selection::None => widgets::placeholder(ui, "Select a commit to see its details"),
    }
}

// ---- working tree -----------------------------------------------------------

fn working_tree(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>) {
    egui::Panel::bottom("commit-box")
        .resizable(false)
        .show_separator_line(false)
        .frame(Frame::new().fill(theme::bg_app()).inner_margin(Margin::same(10)))
        .show(ui, |ui| commit_box(ui, tab, shared, snap));

    egui::CentralPanel::default().frame(Frame::new().fill(theme::bg_app())).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;

        // "6 file changes on [main]"
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
        {
            let trash = egui::Rect::from_center_size(pos2(rect.left() + 20.0, rect.center().y), vec2(26.0, 26.0));
            let response = ui
                .interact(trash, ui.id().with("discard-all"), Sense::click())
                .on_hover_text("Discard all changes");
            let painter = ui.painter();
            painter.rect(
                trash,
                4.0,
                if response.hovered() { theme::red_dark() } else { theme::bg_app() },
                Stroke::new(1.0, theme::mix(theme::bg_app(), theme::red(), 0.6)),
                egui::StrokeKind::Inside,
            );
            painter.text(trash.center(), Align2::CENTER_CENTER, icons::TRASH, theme::font(14.0), theme::red());
            if response.clicked() {
                shared.show(discard_all_dialog(snap));
            }

            let count = snap.status.file_count();
            let text = format!("{count} file change{} on", if count == 1 { "" } else { "s" });
            let branch = snap.head_label();
            let text_galley = painter.layout_no_wrap(text, theme::bold(13.0), theme::text_bright());
            let pill_galley = widgets::elided(painter, &branch, theme::font(12.5), theme::on_accent(), rect.width() * 0.4);
            let pill_w = pill_galley.size().x + 12.0;
            let total = text_galley.size().x + 8.0 + pill_w;
            let x = (rect.center().x - total / 2.0).max(trash.right() + 10.0);
            let w = widgets::galley_left(painter, rect, x, text_galley);
            let pill = egui::Rect::from_min_size(pos2(x + w + 8.0, rect.center().y - 10.0), vec2(pill_w, 20.0));
            painter.rect_filled(pill, 3.0, theme::solid(theme::accent(), 0.6));
            widgets::galley_left(painter, pill, pill.left() + 6.0, pill_galley);
        }

        // A bisect has its own banner, above the graph (`ui::bisect`).
        if let Some(label) = snap.state.label().filter(|_| snap.state != RepoState::Bisect) {
            operation_banner(ui, tab, snap.state, label);
        }
        crate::ui::merge::banner(ui, tab, shared, snap);

        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(8.0);
                segmented(ui, &mut shared.settings.tree_view, (icons::LIST, "Path"), (icons::TREE_VIEW, "Tree"));
            });
        });
        ui.add_space(6.0);

        // Share the remaining height: a short list only takes what it needs.
        let tree = shared.settings.tree_view;
        let unstaged = &snap.status.unstaged;
        let staged = &snap.status.staged;
        let header_h = 32.0;
        let room = (ui.available_height() - header_h * 2.0).max(files::ROW_H * 2.0);
        let need = |n: usize| (n.max(1) as f32) * files::ROW_H;
        let need_unstaged = need(files::row_count(unstaged, tree, tab, DiffSource::Unstaged));
        let need_staged = need(files::row_count(staged, tree, tab, DiffSource::Staged));
        let half = room / 2.0;
        let (h_unstaged, h_staged) = if need_unstaged <= half {
            (need_unstaged, room - need_unstaged)
        } else if need_staged <= half {
            (room - need_staged, need_staged)
        } else {
            (half, half)
        };

        let unborn = snap.head.oid.is_none();
        list_header(ui, &format!("Unstaged Files ({})", unstaged.len()), |ui| {
            if !unstaged.is_empty() && tint_button(ui, "Stage all changes", theme::green()).clicked() {
                tab.run(ops::stage_all());
            }
        });
        file_list(ui, tab, shared, unstaged, DiffSource::Unstaged, h_unstaged);

        list_header(ui, &format!("Staged Files ({})", staged.len()), |ui| {
            if !staged.is_empty() && tint_button(ui, "Unstage all changes", theme::red()).clicked() {
                tab.run(ops::unstage_all(unborn));
            }
        });
        file_list(ui, tab, shared, staged, DiffSource::Staged, h_staged);
    });
}

fn list_header(ui: &mut Ui, title: &str, trailing: impl FnOnce(&mut Ui)) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, theme::border()));
    ui.painter().text(
        pos2(rect.left() + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        format!("{}  {title}", icons::CARET_DOWN),
        theme::font(13.0),
        theme::text(),
    );
    let inner = rect.shrink2(vec2(8.0, 0.0));
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)),
        trailing,
    );
}

/// Shown while a merge, rebase, cherry-pick or revert is waiting on the user.
fn operation_banner(ui: &mut Ui, tab: &mut RepoTab, state: RepoState, label: &str) {
    Frame::new()
        .fill(theme::mix(theme::bg_app(), theme::orange(), 0.18))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}  {label}", icons::WARNING)).color(theme::orange()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(cmd) = ops::abort(state)
                        && tint_button(ui, "Abort", theme::red()).clicked() {
                            tab.run(cmd);
                        }
                    if let Some(cmd) = ops::continue_op(state)
                        && tint_button(ui, "Continue", theme::green()).clicked() {
                            tab.run(cmd);
                        }
                });
            });
        });
    ui.add_space(4.0);
}

fn commit_box(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot) {
    ui.spacing_mut().item_spacing.y = 6.0;
    let prefs = crate::prefs::repo(&shared.settings, &tab.path).commit.clone();
    commit_button::prefill(ui.ctx(), tab, &prefs, snap.head.oid);

    ui.horizontal(|ui| {
        ui.label(RichText::new("Commit Message").color(theme::text()));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let can_amend = snap.head.oid.is_some();
            let before = tab.amend;
            ui.add_enabled(can_amend, egui::Checkbox::new(&mut tab.amend, "Amend"));
            crate::ui::commit_options::button(ui, tab, &prefs);
            let empty = tab.summary.is_empty() && tab.description.is_empty();
            if tab.amend && !before && (empty || commit_button::untouched_template(ui.ctx(), tab)) {
                // Start from the message being amended.
                let (summary, body) = snap.head_message.split_once('\n').unwrap_or((&snap.head_message, ""));
                tab.summary = summary.trim().to_owned();
                tab.description = body.trim().to_owned();
            }
        });
    });

    let summary = ui.add(
        TextEdit::singleline(&mut tab.summary)
            .hint_text("Summary")
            .desired_width(f32::INFINITY)
            .margin(Margin { left: 4, right: 34, top: 2, bottom: 2 }),
    );
    commit_button::summary_counter(ui, &summary, &tab.summary);
    let description = ui.add(
        TextEdit::multiline(&mut tab.description)
            .hint_text("Description")
            .desired_rows(4)
            .desired_width(f32::INFINITY),
    );
    crate::ui::commit_options::chips(ui, tab, &prefs);
    crate::ui::prefs::profiles::commit_author(ui, tab, &shared.settings);

    let focused = summary.has_focus() || description.has_focus();
    let shortcut = focused && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
    commit_button::show(ui, tab, shared, snap, &prefs, shortcut);
}

// ---- commit details ---------------------------------------------------------

fn commit(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>, oid: Oid) {
    let details = match &tab.details {
        Some(Ok(details)) if details.oid == oid => details.clone(),
        Some(Err(e)) => return widgets::placeholder(ui, e),
        _ => return widgets::placeholder(ui, "Loading commit…"),
    };
    let is_stash = snap
        .row_of(oid)
        .is_some_and(|row| snap.history.commits[row].kind == RowKind::Stash);
    let mut jump: Option<Oid> = None;

    Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new(if is_stash { "stash:" } else { "commit:" }).color(theme::text_dim()));
            ui.label(RichText::new(short(oid)).font(theme::mono(12.5)).color(theme::text()));
            if icon_button(ui, icons::COPY, 13.0, "Copy full SHA").clicked() {
                ui.ctx().copy_text(oid.to_string());
            }
        });

        ui.add(
            Label::new(RichText::new(&details.summary).font(theme::bold(15.0)).color(theme::text_bright()))
                .wrap(),
        );
        if !details.body.is_empty() {
            ScrollArea::vertical().id_salt("commit-body").max_height(150.0).show(ui, |ui| {
                ui.add(Label::new(RichText::new(&details.body).color(theme::text())).wrap());
            });
        }
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
            let picture = shared.avatars.get(&details.author_email, shared.settings.fetch_avatars);
            widgets::avatar_picture(
                ui.painter(),
                picture,
                rect.center(),
                16.0,
                &details.author,
                &details.author_email,
                None,
            );
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(RichText::new(&details.author).font(theme::bold(13.0)).color(theme::text_bright()));
                let mut when = format!("authored {}", format_time(details.author_time));
                let ago = relative_time(details.author_time);
                if !ago.is_empty() {
                    when.push_str(&format!("  ({ago})"));
                }
                ui.label(RichText::new(when).size(11.5).color(theme::text_dim()))
                    .on_hover_text(&details.author_email);
                if details.committer != details.author {
                    ui.label(
                        RichText::new(format!(
                            "committed by {} on {}",
                            details.committer,
                            format_time(details.committer_time)
                        ))
                        .size(11.5)
                        .color(theme::text_dim()),
                    );
                }
            });
        });

        if !details.parents.is_empty() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let word = if details.parents.len() == 1 { "parent:" } else { "parents:" };
                ui.label(RichText::new(word).color(theme::text_dim()));
                for parent in &details.parents {
                    let link = ui
                        .add(
                            Label::new(RichText::new(short(*parent)).font(theme::mono(12.5)).color(theme::accent()))
                                .sense(Sense::click()),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if link.clicked() {
                        jump = Some(*parent);
                    }
                }
            });
        }
    });

    ui.spacing_mut().item_spacing.y = 0.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, theme::border()));
    // The files the commit changed, or all of its files (`ui::browse`).
    crate::ui::browse::header(ui, rect, tab, shared, oid, details.files.len());

    let height = ui.available_height();
    if tab.browse.on {
        crate::ui::browse::list(ui, tab, shared, oid, &details.files, height);
    } else {
        file_list(ui, tab, shared, &details.files, DiffSource::Commit(oid), height);
    }

    if let Some(parent) = jump {
        tab.jump_to(parent);
    }
}
