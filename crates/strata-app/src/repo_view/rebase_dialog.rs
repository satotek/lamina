//! The "Edit history" dialog: an interactive rebase as a list of rows.
//!
//! The dialog lists `onto..HEAD` the way a `git rebase -i` todo does, oldest
//! commit on top, each row with an action selector, the short id and the
//! summary. Rows reorder by drag or by Alt/Option+↑/↓ on the focused row; the
//! footer line says what the result will be and a banner warns when the range
//! is already on a remote. All rules (moves, validity, the steps handed to
//! core, the summary) live in [`RebasePlan`]; this file only renders it and
//! forwards input.
//!
//! The reword editor is a nested dialog owned by this one. It mirrors
//! `edit_dialog` but returns the text to the plan instead of running a single
//! reword, which is why it is not reused as is.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable as _, IntoElement, KeyDownEvent, ParentElement,
    Render, ScrollHandle, SharedString, Styled, Subscription, WeakEntity, Window, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use strata_core::Oid;
use strata_core::session::{Operation, RepoReader};

use super::RepoView;
use super::history_ops::{HistoryAction, PendingSelection};
use super::rebase_plan::{ActionKind, PlanProblem, PlanRow, RebasePlan};
use crate::data::short_oid;
use crate::settings::Settings;
use crate::style::Design;
use crate::ui;

/// Tallest the commit list grows before it scrolls.
const LIST_MAX_HEIGHT: f32 = 320.0;

/// What the dialog is opened with, read once from the menu's edit context.
pub(super) struct RebaseRequest {
    /// The commit the range starts after: the clicked commit's parent.
    pub(super) onto: Oid,
    /// HEAD when the dialog opened, so the rewrite's selection can wait for
    /// the snapshot it produces.
    pub(super) head: Oid,
    /// `(id, summary)` of `onto..HEAD`, oldest first.
    pub(super) commits: Vec<(Oid, String)>,
    /// Remote-tracking branches that already contain part of the range.
    pub(super) pushed: Vec<String>,
    /// The branch's upstream when the edited commit is already on it, so
    /// publishing the rewrite needs a force push.
    pub(super) published: Option<String>,
    pub(super) repo: Arc<dyn RepoReader>,
}

/// Opens the dialog for `request`.
pub(super) fn open(
    view: &mut RepoView,
    request: RebaseRequest,
    window: &mut Window,
    cx: &mut Context<RepoView>,
) {
    // The context menu that offered this is closing; the dialog restores the
    // focus it was opened with, which must outlive the menu.
    view.focus_root(window, cx);
    let handle = cx.weak_entity();
    let dialog = cx.new(|cx| RebaseDialog::new(handle, request, cx));
    let focus = dialog.read(cx).focus.clone();
    let content = dialog.clone();
    let footer = dialog.clone();
    let confirm = dialog;
    window.open_dialog(cx, move |card, _, cx| {
        let zoom = Settings::get(cx).appearance.zoom();
        let content = content.clone();
        let footer = footer.clone();
        let confirm = confirm.clone();
        let blocked = confirm.read(cx).blocked_reason();
        let mut ok = ui::button("rebase-ok", t!("rebase_edit.confirm").to_string())
            .size(ui::ButtonSize::Medium)
            .disabled(blocked.is_some());
        if let Some(reason) = blocked {
            ok = ok.tooltip(reason);
        }
        card.title(t!("rebase_edit.title").to_string())
            .w(px(700.0 * zoom))
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                DialogFooter::new()
                    .child(
                        ui::button("rebase-cancel", t!("common.cancel").to_string())
                            .size(ui::ButtonSize::Medium)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(ok.on_click(move |_, window, cx| {
                        footer.update(cx, |dialog, cx| dialog.submit(window, cx));
                    })),
            )
            .on_ok(move |_, window, cx| {
                // Enter reaches the dialog rather than the focused button; a
                // plan git would refuse keeps it open instead of closing.
                if confirm.read(cx).blocked_reason().is_some() {
                    return false;
                }
                confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                false
            })
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

/// The value a row carries while it is dragged, and the floating label under
/// the pointer.
#[derive(Clone)]
struct DraggedRow {
    index: usize,
    label: SharedString,
}

impl Render for DraggedRow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        div()
            .px_2()
            .py_1()
            .rounded(design.px(6.0))
            .border_1()
            .border_color(design.accent)
            .bg(design.panel)
            .text_color(design.text)
            .text_size(px(design.small()))
            .child(self.label.clone())
    }
}

struct RebaseDialog {
    /// Weak: the dialog outlives a `RepoView` the workspace has replaced.
    view: WeakEntity<RepoView>,
    repo: Arc<dyn RepoReader>,
    onto: Oid,
    /// HEAD when the dialog opened; the rewrite replaces it.
    head: Oid,
    plan: RebasePlan,
    /// The row keyboard commands act on.
    cursor: usize,
    pushed: Vec<String>,
    published: Option<String>,
    /// A failed message read, shown until the next edit.
    error: Option<String>,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

impl RebaseDialog {
    fn new(view: WeakEntity<RepoView>, request: RebaseRequest, cx: &mut Context<Self>) -> Self {
        Self {
            view,
            repo: request.repo,
            onto: request.onto,
            head: request.head,
            plan: RebasePlan::new(request.commits),
            cursor: 0,
            pushed: request.pushed,
            published: request.published,
            error: None,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
        }
    }

    /// Why OK is disabled, or `None` when the plan can run.
    fn blocked_reason(&self) -> Option<String> {
        self.plan.problem().map(|problem| problem.reason())
    }

    fn select_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let index = index.min(self.plan.len().saturating_sub(1));
        self.cursor = index;
        self.scroll.scroll_to_item(index);
        cx.notify();
    }

    /// Moves the row at `from` to where the row at `to` is, and follows it.
    fn move_row(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if let Some(index) = self.plan.move_to(from, to) {
            self.select_row(index, cx);
        }
    }

    fn set_action(
        &mut self,
        index: usize,
        action: ActionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        self.cursor = index;
        // A selector click can leave focus on its button; the list owns the
        // keys, so an Alt+↑ right after the click still moves the row.
        window.focus(&self.focus, cx);
        if action == ActionKind::Reword {
            self.open_reword(index, window, cx);
        } else {
            self.plan.set_action(index, action);
        }
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let plain = !modifiers.alt
            && !modifiers.control
            && !modifiers.platform
            && !modifiers.shift
            && !modifiers.function;
        match key {
            "up" | "down" if modifiers.alt && !modifiers.control && !modifiers.platform => {
                let moved = if key == "up" {
                    self.plan.move_up(self.cursor)
                } else {
                    self.plan.move_down(self.cursor)
                };
                if let Some(index) = moved {
                    self.select_row(index, cx);
                }
            }
            "up" if plain => self.select_row(self.cursor.saturating_sub(1), cx),
            "down" if plain => self.select_row(self.cursor + 1, cx),
            _ => {
                let Some(action) = plain.then(|| ActionKind::from_shortcut(key)).flatten() else {
                    return;
                };
                self.set_action(self.cursor, action, window, cx);
            }
        }
        cx.stop_propagation();
    }

    /// Opens the message editor for a row, prefilled with the text it already
    /// has: the earlier reword, or the commit's own message.
    fn open_reword(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.plan.rows().get(index) else {
            return;
        };
        let message = match row.message() {
            Some(message) => message.to_owned(),
            None => match self.repo.commit_message(row.commit) {
                Ok(message) => message,
                Err(error) => {
                    self.error = Some(
                        t!(
                            "history_ops.read_commit_failed",
                            id = row.short,
                            error = error
                        )
                        .to_string(),
                    );
                    return;
                }
            },
        };
        let commit = row.commit;
        let short = row.short.clone();
        let owner = cx.weak_entity();
        let prompt = cx.new(|cx| RewordPrompt::new(owner, commit, short, message, window, cx));
        let focus = prompt.read(cx).message.focus_handle(cx);
        let content = prompt.clone();
        let footer = prompt.clone();
        let confirm = prompt;
        let zoom = Settings::get(cx).appearance.zoom();
        window.open_dialog(cx, move |card, _, cx| {
            let content = content.clone();
            let footer = footer.clone();
            let confirm = confirm.clone();
            let empty = confirm.read(cx).is_empty(cx);
            card.title(t!("edit_dialog.title").to_string())
                .w(px(520.0 * zoom))
                .content(move |body, _, _| body.child(content.clone()))
                .footer(
                    DialogFooter::new()
                        .child(
                            ui::button("rebase-reword-cancel", t!("common.cancel").to_string())
                                .size(ui::ButtonSize::Medium)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            ui::button(
                                "rebase-reword-ok",
                                t!("rebase_edit.reword_button").to_string(),
                            )
                            .size(ui::ButtonSize::Medium)
                            .disabled(empty)
                            .on_click(move |_, window, cx| {
                                footer.update(cx, |prompt, cx| prompt.submit(window, cx));
                            }),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    if confirm.read(cx).is_empty(cx) {
                        return false;
                    }
                    confirm.update(cx, |prompt, cx| prompt.submit(window, cx));
                    false
                })
        });
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    /// Stores a reword the prompt returned. The row is found by commit
    /// because the list may have been reordered while the prompt was open.
    fn apply_reword(&mut self, commit: Oid, message: String, cx: &mut Context<Self>) {
        let Some(index) = self.plan.rows().iter().position(|row| row.commit == commit) else {
            return;
        };
        self.plan.reword(index, message);
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut App) {
        let Ok(steps) = self.plan.steps() else {
            return;
        };
        if self.plan.problem().is_some() {
            return;
        }
        let onto = self.onto;
        let label = t!("rebase_edit.op", summary = self.plan.summary().text()).to_string();
        // The rewrite replaces every commit it replays, so the row the user
        // was on is found again by its distance from the new tip.
        let selection = self
            .plan
            .selection_distance(self.cursor)
            .map(|distance| PendingSelection::at(self.head, distance));
        let _ = self.view.update(cx, |view, cx| {
            // Another operation can have started while the dialog was open.
            // A dirty worktree is left to core, which refuses with a message.
            let state = view.history_menu_state();
            if let Some(reason) = state
                .as_ref()
                .and_then(|state| state.blocked(HistoryAction::Reset))
            {
                view.status_error =
                    Some(t!("rebase_edit.cannot_start", reason = reason).to_string());
                cx.notify();
                return;
            }
            view.run_operation(Operation::RebaseEdit { onto, steps }, label, window, cx);
            view.edit_selection = selection;
        });
        // Closing here would drop this dialog while it is being updated.
        window.defer(cx, |window, cx| window.close_dialog(cx));
    }

    /// One row: grip, action selector, short id and summary.
    fn render_row(
        &self,
        index: usize,
        row: &PlanRow,
        orphan: bool,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dropped = row.action == ActionKind::Drop;
        // An orphan has nothing above to be nested under, so it is not indented.
        let combined = matches!(row.action, ActionKind::Squash | ActionKind::Fixup) && !orphan;
        let selector = ActionKind::ALL.into_iter().fold(
            div().flex().flex_none().items_center().gap(design.px(2.0)),
            |selector, action| {
                let chosen = row.action == action;
                let kind = match (chosen, action) {
                    // A drop reads as destructive, and so does the squash or
                    // fixup the plan cannot honour.
                    (true, ActionKind::Drop) => ui::ButtonKind::Danger,
                    (true, _) if orphan => ui::ButtonKind::Danger,
                    (true, _) => ui::ButtonKind::Primary,
                    (false, _) => ui::ButtonKind::Ghost,
                };
                let button = ui::button(
                    SharedString::from(format!("rebase-{index}-{}", action.id())),
                    action.label(),
                )
                .size(ui::ButtonSize::Tiny)
                .kind(kind)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_action(index, action, window, cx);
                }));
                selector.child(button)
            },
        );

        let summary = row.display_summary().to_owned();
        let label = SharedString::from(format!("{} {}", row.short, summary));
        let drag = DraggedRow { index, label };
        let accent = design.accent_soft;
        let text_color = if dropped { design.muted } else { design.text };

        ui::list_row(
            SharedString::from(format!("rebase-row-{index}")),
            ui::RowState::NORMAL.selected(index == self.cursor),
            design,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.select_row(index, cx);
            window.focus(&this.focus, cx);
        }))
        .on_drag(drag, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
        .drag_over::<DraggedRow>(move |style, _, _, _| style.bg(accent))
        .on_drop(cx.listener(move |this, dragged: &DraggedRow, _, cx| {
            this.move_row(dragged.index, index, cx);
        }))
        .when(combined, |row| row.pl(design.px(24.0)))
        .child(
            div()
                .flex_none()
                .w(design.px(14.0))
                .text_color(design.muted)
                .child("⠿"),
        )
        .child(selector)
        .child(
            div()
                .flex_none()
                .w(design.px(58.0))
                .font_family(design.mono.clone())
                .text_size(px(design.code))
                .text_color(design.muted)
                .child(row.short.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(text_color)
                .when(dropped, |summary| summary.line_through())
                .child(summary),
        )
        .when(row.action == ActionKind::Reword, |row| {
            row.child(ui::status_badge(
                t!("rebase_edit.badge.reworded").to_string(),
                ui::BadgeTone::Accent,
                design,
            ))
        })
        .when(orphan, |row| {
            row.child(ui::status_badge(
                t!("rebase_edit.badge.nothing_above").to_string(),
                ui::BadgeTone::Danger,
                design,
            ))
        })
    }
}

impl Render for RebaseDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let orphan = self.plan.orphan_row();
        let problem = self.plan.problem();
        let summary = self.plan.summary().text();

        let mut rows = div().flex().flex_col().py_1();
        for (index, row) in self.plan.rows().iter().enumerate() {
            rows = rows.child(self.render_row(index, row, orphan == Some(index), &design, cx));
        }

        let list = div()
            .id("rebase-rows")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.handle_key(event, window, cx);
            }))
            .max_h(design.px(LIST_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .rounded(design.px(6.0))
            .border_1()
            .border_color(design.border)
            .bg(design.background)
            .child(rows);

        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child(t!("rebase_edit.description", short = short_oid(self.onto))),
            )
            .child(list)
            .when(!self.pushed.is_empty(), |body| {
                body.child(
                    ui::banner(
                        ui::BannerKind::Warning,
                        t!(
                            "rebase_edit.pushed_warning",
                            remotes = self.pushed.join(", ")
                        ),
                        &design,
                    )
                    .wrap()
                    .rounded(design.px(6.0)),
                )
            })
            .when_some(self.published.clone(), |body, upstream| {
                body.child(
                    ui::banner(
                        ui::BannerKind::Warning,
                        t!("history_ops.pushed_commit_warning", upstream = upstream),
                        &design,
                    )
                    .wrap()
                    .rounded(design.px(6.0)),
                )
            })
            .when_some(self.error.clone(), |body, error| {
                body.child(
                    ui::banner(ui::BannerKind::Danger, error, &design)
                        .wrap()
                        .rounded(design.px(6.0)),
                )
            })
            .when_some(problem.filter(PlanProblem::is_error), |body, problem| {
                body.child(
                    ui::banner(ui::BannerKind::Danger, problem.reason(), &design)
                        .rounded(design.px(6.0)),
                )
            })
            .child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.text)
                    .child(summary),
            )
    }
}

/// The nested message editor of a reword row.
struct RewordPrompt {
    owner: WeakEntity<RebaseDialog>,
    commit: Oid,
    short: String,
    message: Entity<TextareaState>,
    _events: Vec<Subscription>,
}

impl RewordPrompt {
    fn new(
        owner: WeakEntity<RebaseDialog>,
        commit: Oid,
        short: String,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextareaState::new(window, cx).default_value(message));
        let events = vec![
            cx.subscribe(&input, |_this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    // Repaints the footer so OK follows the editor between
                    // enabled and disabled while the text is being cleared.
                    cx.notify();
                }
            }),
        ];
        Self {
            owner,
            commit,
            short,
            message: input,
            _events: events,
        }
    }

    fn is_empty(&self, cx: &App) -> bool {
        self.message.read(cx).value().trim().is_empty()
    }

    fn submit(&mut self, window: &mut Window, cx: &mut App) {
        let message = self.message.read(cx).value().to_string();
        if message.trim().is_empty() {
            return;
        }
        let commit = self.commit;
        let _ = self
            .owner
            .update(cx, |dialog, cx| dialog.apply_reword(commit, message, cx));
        // Closing here would drop this prompt while it is being updated.
        window.defer(cx, |window, cx| window.close_dialog(cx));
    }
}

impl Render for RewordPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child(t!("edit_dialog.description", short = self.short)),
            )
            .child(
                Textarea::new(&self.message)
                    .h(design.px(220.0))
                    .w_full()
                    .text_size(px(design.code)),
            )
    }
}
