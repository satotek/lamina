//! File history: the commits that touched one path, with that path's diff per
//! commit.
//!
//! The list comes from `Session::path_history`, which walks history on the
//! session's query worker; the diff comes from `DiffRequest::Commit`. Both are
//! keyed by ticket, so a late answer can never replace the view for a newer
//! selection. The worker only answers once the whole walk is done, so the list
//! shows a spinner instead of pretending to stream.

use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement, SharedString,
    Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};
use gpui_component::{ActiveTheme as _, Theme, spinner::Spinner};
use strata_core::session::{DiffRequest, DiffTicket, DiffView, QueryTicket};

use crate::data;
use crate::diff_view::{self, DiffColors, Row as DiffRow};
use crate::repo_view::RepoView;
use crate::scrollbar::WithScrollbar as _;
use crate::search::{QuerySlot, Ready};
use crate::settings::Settings;
use crate::style::{self, Design};

const LIST_WIDTH: f32 = 520.0;

/// State of the file-history view. Rendering reads it; [`RepoView`] updates it
/// from session events and input.
pub(crate) struct FileHistory {
    pub(crate) path: String,
    slot: QuerySlot,
    /// Snapshot row indices of the commits that touched `path`, newest first.
    rows: Option<Arc<Vec<usize>>>,
    /// Snapshot row index of the commit whose diff is shown.
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,

    diff_ticket: Option<DiffTicket>,
    view: Option<Arc<DiffView>>,
    diff_rows: Rc<Vec<DiffRow>>,
    diff_error: Option<String>,

    error: Option<String>,
}

impl FileHistory {
    fn new(path: String) -> Self {
        Self {
            path,
            slot: QuerySlot::default(),
            rows: None,
            selected: None,
            scroll: UniformListScrollHandle::default(),
            diff_scroll: UniformListScrollHandle::default(),
            diff_ticket: None,
            view: None,
            diff_rows: Rc::new(Vec::new()),
            diff_error: None,
            error: None,
        }
    }

    fn rows(&self) -> Option<&Arc<Vec<usize>>> {
        self.rows.as_ref()
    }
}

/// Keeps the current selection when it is still in the list, otherwise takes
/// the newest match.
fn choose_selection(rows: &[usize], current: Option<usize>) -> Option<usize> {
    match current {
        Some(current) if rows.contains(&current) => Some(current),
        _ => rows.first().copied(),
    }
}

// ---------------------------------------------------------------------------
// RepoView glue.
// ---------------------------------------------------------------------------

impl RepoView {
    /// Opens the file-history view for a changed file. Only a real session can
    /// answer it; the synthetic benchmark has no file list.
    pub(crate) fn open_file_history(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        self.cancel_file_history();
        self.file_history = Some(FileHistory::new(path));
        self.focus_root(window, cx);
        self.start_file_history(cx);
    }

    pub(crate) fn start_file_history(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.file_history.as_ref().map(|it| it.path.clone()) else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let ticket = session.path_history(path);
        let Some(history) = self.file_history.as_mut() else {
            return;
        };
        if let Some(previous) = history.slot.begin(ticket) {
            session.cancel_query(previous);
        }
        history.error = None;
        cx.notify();
    }

    /// Returns to the commit list. True when a file history was open.
    pub(crate) fn close_file_history(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.file_history.is_none() {
            return false;
        }
        self.cancel_file_history();
        self.file_history = None;
        self.focus_root(window, cx);
        cx.notify();
        true
    }

    /// Cancels the outstanding query and diff without touching focus.
    pub(crate) fn cancel_file_history(&mut self) {
        let query = self.file_history.as_mut().and_then(|it| it.slot.cancel());
        let diff = self
            .file_history
            .as_mut()
            .and_then(|it| it.diff_ticket.take());
        if let Some(session) = self.session.as_ref() {
            if let Some(ticket) = query {
                session.cancel_query(ticket);
            }
            if let Some(ticket) = diff {
                session.cancel_diff(ticket);
            }
        }
    }

    /// Drops the view and its tickets when another main view takes over.
    pub(crate) fn discard_file_history(&mut self) {
        if self.file_history.is_some() {
            self.cancel_file_history();
            self.file_history = None;
        }
    }

    pub(crate) fn file_history_ready(
        &mut self,
        ticket: QueryTicket,
        generation: u64,
        rows: &Arc<Vec<usize>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let current = self
            .session
            .as_ref()
            .map_or(0, |session| session.snapshot().generation);
        let Some(history) = self.file_history.as_mut() else {
            return false;
        };
        match history.slot.accept(ticket, generation, current) {
            Ready::Ignored => false,
            Ready::Stale => {
                self.start_file_history(cx);
                true
            }
            Ready::Applied => {
                history.rows = Some(Arc::clone(rows));
                history.error = None;
                let previous = history.selected;
                history.selected = choose_selection(rows, previous);
                let moved = history.selected != previous;
                if moved {
                    history.view = None;
                    history.diff_rows = Rc::new(Vec::new());
                    history.diff_error = None;
                }
                cx.notify();
                if moved {
                    self.request_history_diff(cx);
                }
                true
            }
        }
    }

    pub(crate) fn file_history_failed(
        &mut self,
        ticket: QueryTicket,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(history) = self.file_history.as_mut() else {
            return false;
        };
        if !history.slot.fail(ticket) {
            return false;
        }
        history.error = Some(error.to_owned());
        history.rows = Some(Arc::new(Vec::new()));
        cx.notify();
        true
    }

    pub(crate) fn file_history_diff_ready(
        &mut self,
        ticket: DiffTicket,
        view: &Arc<DiffView>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(history) = self.file_history.as_mut() else {
            return false;
        };
        if history.diff_ticket != Some(ticket) {
            return false;
        }
        history.diff_rows = Rc::new(diff_view::flatten(&view.diff));
        history.view = Some(Arc::clone(view));
        history.diff_error = None;
        cx.notify();
        true
    }

    pub(crate) fn file_history_diff_failed(
        &mut self,
        ticket: DiffTicket,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(history) = self.file_history.as_mut() else {
            return false;
        };
        if history.diff_ticket != Some(ticket) {
            return false;
        }
        history.diff_ticket = None;
        history.view = None;
        history.diff_rows = Rc::new(Vec::new());
        history.diff_error = Some(error.to_owned());
        cx.notify();
        true
    }

    /// File history is a query over loaded commits too, so a new chunk means
    /// the walk has to be repeated to include it.
    pub(crate) fn file_history_on_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(history) = self.file_history.as_ref() else {
            return;
        };
        if self.mode != crate::worktree_panel::MainMode::History {
            return;
        }
        let current = self
            .session
            .as_ref()
            .map_or(0, |session| session.snapshot().generation);
        if history.slot.needs_refresh(current) {
            self.start_file_history(cx);
        }
    }

    /// Selects a commit and asks for that commit's diff of the path.
    pub(crate) fn select_history_commit(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.file_history.as_ref().and_then(|it| it.selected) == Some(index) {
            return;
        }
        if let Some(history) = self.file_history.as_mut() {
            history.selected = Some(index);
            history.view = None;
            history.diff_rows = Rc::new(Vec::new());
            history.diff_error = None;
        }
        self.request_history_diff(cx);
    }

    pub(crate) fn step_file_history(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(history) = self.file_history.as_ref() else {
            return;
        };
        let Some(rows) = history.rows() else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let position = history
            .selected
            .and_then(|index| rows.iter().position(|row| *row == index))
            .map_or(0, |position| {
                (position as isize + delta).clamp(0, rows.len() as isize - 1) as usize
            });
        let Some(index) = rows.get(position).copied() else {
            return;
        };
        history
            .scroll
            .scroll_to_item(position, gpui::ScrollStrategy::Center);
        self.select_history_commit(index, cx);
    }

    fn request_history_diff(&mut self, cx: &mut Context<Self>) {
        let Some((index, path)) = self
            .file_history
            .as_ref()
            .map(|history| (history.selected, history.path.clone()))
        else {
            return;
        };
        let Some(index) = index else {
            return;
        };
        let Some(commit) = self.source().and_then(|source| source.commit_id(index)) else {
            return;
        };
        let (Some(session), Some(history)) = (self.session.as_ref(), self.file_history.as_mut())
        else {
            return;
        };
        let ticket = session.request_diff(DiffRequest::Commit {
            commit,
            path: path.clone(),
        });
        if let Some(previous) = history.diff_ticket.replace(ticket) {
            session.cancel_diff(previous);
        }
        history.view = None;
        history.diff_rows = Rc::new(Vec::new());
        history.diff_error = None;
        cx.notify();
    }

    /// The whole main area while a file history is open.
    pub(crate) fn file_history_view(
        &self,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(history) = self.file_history.as_ref() else {
            return div().into_any_element();
        };
        let colors = Colors::from_theme(cx.theme(), Settings::get(cx));

        let back = {
            let entity = entity.clone();
            crate::ui::button("file-history-back", "‹ History")
                .size(crate::ui::ButtonSize::Tiny)
                .ghost()
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.close_file_history(window, cx);
                    });
                })
        };
        let count = history.rows().map_or_else(
            || "walking history…".to_owned(),
            |rows| {
                format!(
                    "{} commit{}",
                    rows.len(),
                    if rows.len() == 1 { "" } else { "s" }
                )
            },
        );

        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .h(px(40.0))
            .flex_none()
            .px_3p5()
            .bg(colors.background)
            .border_b_1()
            .border_color(colors.border)
            .child(back)
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("File history"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(colors.mono.clone())
                    .text_size(px(11.5))
                    .text_color(colors.accent)
                    .child(history.path.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(count),
            );

        let diff_colors = DiffColors::from_theme(cx.theme());
        let list = self.file_history_list(entity.clone(), &colors);
        let diff = self.file_history_diff(&colors, &diff_colors, cx);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(colors.background)
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(list)
                    .child(diff),
            )
            .into_any_element()
    }

    fn file_history_list(&self, entity: Entity<Self>, colors: &Colors) -> AnyElement {
        let Some(history) = self.file_history.as_ref() else {
            return div().into_any_element();
        };
        let Some(source) = self.source() else {
            return div().into_any_element();
        };
        if let Some(error) = &history.error {
            return list_message(error, colors.danger, colors);
        }
        let Some(rows) = history.rows() else {
            // The query worker answers once, at the end of the walk; there is
            // nothing to stream until then.
            return div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .w(px(LIST_WIDTH))
                .flex_none()
                .border_r_1()
                .border_color(colors.border)
                .child(Spinner::new().color(colors.muted_foreground))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(colors.muted_foreground)
                        .child("walking history…"),
                )
                .into_any_element();
        };
        if rows.is_empty() {
            return list_message(
                "No loaded commit touches this path",
                colors.muted_foreground,
                colors,
            );
        }

        let selected = history.selected;
        let list_colors = colors.clone();
        let rows = Arc::clone(rows);
        uniform_list(
            "file-history-commits",
            rows.len(),
            move |range, _window, _cx| {
                range
                    .map(|position| {
                        let index = rows[position];
                        let Some(row) = source.row(index) else {
                            return div().into_any_element();
                        };
                        let date = style::date_label(
                            row.time,
                            style::now_epoch(),
                            list_colors.design.date_format,
                        );
                        let element = crate::ui::list_row(
                            ("file-history-row", position),
                            crate::ui::RowState::NORMAL.selected(selected == Some(index)),
                            &list_colors.design,
                        )
                        .gap_3()
                        .mx(px(0.0))
                        .w_full()
                        .px_3()
                        .text_size(px(list_colors.design.small()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(row.summary.to_string()),
                        )
                        .child(
                            div()
                                .w(px(100.0))
                                .flex_none()
                                .truncate()
                                .text_color(list_colors.muted_foreground)
                                .child(row.author.to_string()),
                        )
                        .child(
                            div()
                                .w(px(104.0))
                                .flex_none()
                                .text_color(list_colors.muted_foreground)
                                .child(date),
                        )
                        .child(
                            div()
                                .w(px(58.0))
                                .flex_none()
                                .font_family(list_colors.mono.clone())
                                .text_color(list_colors.muted_foreground)
                                .child(row.id.to_string()),
                        );
                        let entity = entity.clone();
                        element
                            .cursor_pointer()
                            .on_click(move |_event, _window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.select_history_commit(index, cx);
                                });
                            })
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&history.scroll)
        .with_scrollbar(&history.scroll)
        .w(px(LIST_WIDTH))
        .flex_none()
        .border_r_1()
        .border_color(colors.border)
        .into_any_element()
    }

    fn file_history_diff(
        &self,
        colors: &Colors,
        diff_colors: &DiffColors,
        cx: &mut App,
    ) -> AnyElement {
        let Some(history) = self.file_history.as_ref() else {
            return div().into_any_element();
        };
        let commit = history
            .selected
            .and_then(|index| self.source().and_then(|source| source.commit_id(index)));

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .h(px(36.0))
            .flex_none()
            .px_3p5()
            .bg(colors.title_bar)
            .border_b_1()
            .border_color(colors.border)
            .text_size(px(11.0))
            .child(
                div()
                    .truncate()
                    .font_family(colors.mono.clone())
                    .child(history.path.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(colors.muted_foreground)
                    .child(match commit {
                        Some(commit) => format!(
                            "{} · {} rows",
                            data::short_oid(commit),
                            history.diff_rows.len()
                        ),
                        None => "no commit selected".to_owned(),
                    }),
            );

        let body = if let Some(error) = &history.diff_error {
            message(error, colors.danger, colors)
        } else if history.selected.is_none() {
            message("Select a commit", colors.muted_foreground, colors)
        } else if let Some(view) = &history.view {
            if view.diff.binary {
                message("Binary file", colors.muted_foreground, colors)
            } else if history.diff_rows.is_empty() {
                message("No textual changes", colors.muted_foreground, colors)
            } else {
                self.history_diff_rows(colors, diff_colors, cx)
            }
        } else {
            message("computing diff…", colors.muted_foreground, colors)
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn history_diff_rows(
        &self,
        colors: &Colors,
        diff_colors: &DiffColors,
        cx: &mut App,
    ) -> AnyElement {
        let Some(history) = self.file_history.as_ref() else {
            return div().into_any_element();
        };
        diff_view::diff_list(
            "file-history-diff",
            Rc::clone(&history.diff_rows),
            history.view.clone(),
            *diff_colors,
            colors.mono.clone(),
            &history.diff_scroll,
            cx,
        )
    }
}

fn list_message(text: &str, color: Hsla, colors: &Colors) -> AnyElement {
    div()
        .flex()
        .w(px(LIST_WIDTH))
        .flex_none()
        .border_r_1()
        .border_color(colors.border)
        .child(crate::ui::message(text, color, &colors.design))
        .into_any_element()
}

fn message(text: &str, color: Hsla, colors: &Colors) -> AnyElement {
    crate::ui::message(text, color, &colors.design).into_any_element()
}

#[derive(Clone)]
struct Colors {
    design: Design,
    background: Hsla,
    muted_foreground: Hsla,
    border: Hsla,
    title_bar: Hsla,
    accent: Hsla,
    danger: Hsla,
    mono: SharedString,
}

impl Colors {
    fn from_theme(theme: &Theme, settings: &Settings) -> Self {
        let design = Design::new(theme, settings);
        Self {
            background: design.background,
            muted_foreground: design.muted,
            border: design.border,
            title_bar: design.chrome,
            accent: design.accent,
            danger: design.remove_text,
            mono: design.mono.clone(),
            design,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::choose_selection;

    #[test]
    fn selection_keeps_a_listed_commit_and_otherwise_takes_the_newest() {
        let rows = [4, 7, 9];
        assert_eq!(choose_selection(&rows, Some(7)), Some(7));
        assert_eq!(
            choose_selection(&rows, Some(8)),
            Some(4),
            "a commit that left the list falls back to the newest match"
        );
        assert_eq!(choose_selection(&rows, None), Some(4));
        assert_eq!(choose_selection(&[], Some(7)), None);
        assert_eq!(choose_selection(&[], None), None);
    }
}
