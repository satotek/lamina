//! File history: the commits that touched one path, with that path's diff per
//! commit.
//!
//! The list comes from `Session::path_history_streaming`, which walks history on
//! the session's query worker and reports rows in batches, so a long history
//! fills in while the walk runs. The diff comes from `DiffRequest::Commit`.
//! Both are keyed by ticket, so a late answer can never replace the view for a
//! newer selection.
//!
//! Renames are followed by the session, but its batches carry row indices only.
//! The name a commit knew the file under therefore comes from a second,
//! rename-following walk run on the background executor, once per path: a row
//! whose commit used another name says so, and its diff is opened under that
//! name rather than the one the view was opened with.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement, SharedString,
    Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};
use gpui_component::{ActiveTheme as _, Theme, spinner::Spinner};
use strata_core::Oid;
use strata_core::session::{DiffRequest, DiffTicket, DiffView, QueryTicket};

use crate::data;
use crate::diff_view::{self, DiffColors, Row as DiffRow};
use crate::repo_view::{RepoView, history_diff_options};
use crate::scrollbar::WithScrollbar as _;
use crate::search::{QuerySlot, Ready};
use crate::settings::Settings;
use crate::style::{self, Design};

const LIST_WIDTH: f32 = 520.0;

/// Bound on the rename map: the walk runs to the end of history, but the names
/// kept are only useful for commits the list can show, and a repository with
/// millions of commits must not turn a file view into a memory leak.
const NAME_LIMIT: usize = 100_000;

/// State of the file-history view. Rendering reads it; [`RepoView`] updates it
/// from session events and input.
pub(crate) struct FileHistory {
    pub(crate) path: String,
    slot: QuerySlot,
    /// Rows gathered from the streaming walk, newest first.
    walk: BatchRows,
    /// Commit -> path the file had at that commit, once the rename walk
    /// answers. Shared so the list closure can read it without copying.
    names: Arc<HashMap<Oid, String>>,
    /// Stops the rename walk when the view closes or the path changes.
    names_stop: Arc<AtomicBool>,
    /// True while the rename walk runs; it is started once per path.
    names_walking: bool,
    /// Snapshot row index of the commit whose diff is shown.
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,

    diff_ticket: Option<DiffTicket>,
    /// Path the visible diff was requested with, so a name that arrives late
    /// re-requests the same commit under the name it had then.
    diff_path: Option<String>,
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
            walk: BatchRows::default(),
            names: Arc::new(HashMap::new()),
            names_stop: Arc::new(AtomicBool::new(false)),
            names_walking: false,
            selected: None,
            scroll: UniformListScrollHandle::default(),
            diff_scroll: UniformListScrollHandle::default(),
            diff_ticket: None,
            diff_path: None,
            view: None,
            diff_rows: Rc::new(Vec::new()),
            diff_error: None,
            error: None,
        }
    }

    fn rows(&self) -> &[usize] {
        &self.walk.rows
    }

    fn rows_shared(&self) -> Arc<Vec<usize>> {
        Arc::clone(&self.walk.rows)
    }

    fn walking(&self) -> bool {
        self.walk.ticket.is_some()
    }

    /// The path `index`'s commit knew the file under, falling back to the path
    /// the view was opened with until the rename walk has answered.
    fn path_at(&self, index: usize, source: Option<&data::Source>) -> String {
        let named = source
            .and_then(|source| source.commit_id(index))
            .and_then(|commit| self.names.get(&commit).cloned());
        named.unwrap_or_else(|| self.path.clone())
    }
}

/// Rows accumulated from one streaming walk, and the ticket that owns them.
///
/// The query worker emits batches and a done event, and a ticket can be
/// cancelled or replaced by a newer selection at any point in between; keeping
/// the ticket next to the rows makes "is this batch still mine?" a single
/// comparison instead of a race. Rows also belong to a snapshot generation, so
/// a batch for a replaced snapshot is dropped rather than shown as the wrong
/// commits.
#[derive(Debug, Default)]
struct BatchRows {
    ticket: Option<QueryTicket>,
    /// Shared with the list closure, which appends nothing but must not copy
    /// the whole history on every frame.
    rows: Arc<Vec<usize>>,
}

/// What one batch meant to the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Batch {
    /// The rows were appended.
    Appended(usize),
    /// A batch for a ticket that is no longer the one being walked.
    Ignored,
    /// The current ticket, but its rows point into a replaced snapshot.
    Stale,
}

impl BatchRows {
    /// Starts a walk, replacing whatever the previous one had gathered.
    fn begin(&mut self, ticket: QueryTicket) {
        self.ticket = Some(ticket);
        self.rows = Arc::new(Vec::new());
    }

    /// Replaces the rows with a complete answer, for the non-streaming query.
    fn replace(&mut self, rows: &[usize]) {
        self.ticket = None;
        self.rows = Arc::new(rows.to_vec());
    }

    fn append(
        &mut self,
        ticket: QueryTicket,
        generation: u64,
        current_generation: u64,
        entries: &[usize],
    ) -> Batch {
        if self.ticket != Some(ticket) {
            return Batch::Ignored;
        }
        if generation != current_generation {
            return Batch::Stale;
        }
        Arc::make_mut(&mut self.rows).extend_from_slice(entries);
        Batch::Appended(entries.len())
    }

    /// True when `ticket` is the walk's; the ticket is released either way.
    fn done(&mut self, ticket: QueryTicket) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        self.ticket = None;
        true
    }

    fn cancel(&mut self) -> Option<QueryTicket> {
        self.ticket.take()
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

/// `"renamed from <old path>"` for a commit that knew the file under another
/// name, or `None` when it had the requested one.
fn rename_label(requested: &str, path: Option<&str>) -> Option<String> {
    let path = path?;
    (path != requested).then(|| format!("renamed from {path}"))
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
        let ticket = session.path_history_streaming(path.clone());
        let repo = session.repo().clone();
        let Some(history) = self.file_history.as_mut() else {
            return;
        };
        if let Some(previous) = history.slot.begin(ticket) {
            session.cancel_query(previous);
        }
        history.walk.begin(ticket);
        history.error = None;
        // The batches name rows, not paths, so the names come from a walk of
        // their own. It is keyed by commit and kept across snapshot refreshes,
        // which is why it only starts once per opened path.
        if !history.names_walking && history.names.is_empty() {
            history.names_walking = true;
            let stop = Arc::clone(&history.names_stop);
            // The answer carries the view's own stop flag, so a walk started
            // for an earlier view of the same path cannot land in this one.
            let token = Arc::clone(&history.names_stop);
            let walk_path = path.clone();
            let answer_path = path;
            cx.spawn(async move |this, cx| {
                let names = cx
                    .background_executor()
                    .spawn(async move {
                        let mut names: HashMap<Oid, String> = HashMap::new();
                        let Ok(entries) = repo.path_history_entries(&walk_path) else {
                            return names;
                        };
                        for entry in entries {
                            if stop.load(Ordering::Relaxed) || names.len() >= NAME_LIMIT {
                                break;
                            }
                            let Ok(entry) = entry else { break };
                            names.insert(entry.commit, entry.path);
                        }
                        names
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.file_history_names(&answer_path, &token, names, cx);
                });
            })
            .detach();
        }
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

    /// Cancels the outstanding query, rename walk and diff without touching
    /// focus.
    pub(crate) fn cancel_file_history(&mut self) {
        let Some(history) = self.file_history.as_mut() else {
            return;
        };
        history.names_stop.store(true, Ordering::Relaxed);
        let query = history.slot.cancel();
        let stream = history.walk.cancel();
        let diff = history.diff_ticket.take();
        if let Some(session) = self.session.as_ref() {
            // `slot` and the walk hold the same ticket; cancelling it twice is
            // harmless and keeps the close path from depending on that.
            if let Some(ticket) = query.or(stream) {
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

    /// One batch of the streaming walk.
    pub(crate) fn file_history_batch(
        &mut self,
        ticket: QueryTicket,
        generation: u64,
        entries: &[usize],
        cx: &mut Context<Self>,
    ) -> bool {
        let current = self.current_generation();
        let Some(history) = self.file_history.as_mut() else {
            return false;
        };
        match history.walk.append(ticket, generation, current, entries) {
            Batch::Ignored => false,
            Batch::Appended(_) => {
                cx.notify();
                true
            }
            Batch::Stale => {
                // The rows index a snapshot that has been replaced; walking
                // again is the only way to get rows that select anything.
                self.start_file_history(cx);
                true
            }
        }
    }

    /// The walk reached the end of history.
    pub(crate) fn file_history_done(
        &mut self,
        ticket: QueryTicket,
        generation: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let current = self.current_generation();
        let Some(history) = self.file_history.as_mut() else {
            return false;
        };
        if !history.walk.done(ticket) {
            return false;
        }
        match history.slot.accept(ticket, generation, current) {
            Ready::Ignored => false,
            Ready::Stale => {
                self.start_file_history(cx);
                true
            }
            Ready::Applied => {
                let previous = history.selected;
                history.selected = choose_selection(history.rows(), previous);
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

    /// The rename walk answered. It is keyed by path, so an answer for a path
    /// the view has since left is dropped.
    pub(crate) fn file_history_names(
        &mut self,
        path: &str,
        token: &Arc<AtomicBool>,
        names: HashMap<Oid, String>,
        cx: &mut Context<Self>,
    ) {
        let selected = {
            let Some(history) = self.file_history.as_mut() else {
                return;
            };
            if history.path != path || !Arc::ptr_eq(&history.names_stop, token) {
                return;
            }
            history.names_walking = false;
            if names.is_empty() {
                // Cancelled or unreadable: the rows fall back to the path the
                // view was opened with, which is right for every commit before
                // the first rename.
                return;
            }
            history.names = Arc::new(names);
            history.selected
        };
        cx.notify();
        // A commit selected before the names arrived may have been asked for
        // under the wrong name.
        let Some(index) = selected else {
            return;
        };
        let resolved = self
            .file_history
            .as_ref()
            .map(|history| history.path_at(index, self.source().as_ref()))
            .unwrap_or_default();
        let requested = self
            .file_history
            .as_ref()
            .and_then(|it| it.diff_path.clone());
        if requested.as_deref() != Some(resolved.as_str()) {
            self.request_history_diff(cx);
        }
    }

    /// The non-streaming `path_history` answer, kept for the query events the
    /// view still receives from other searches.
    pub(crate) fn file_history_ready(
        &mut self,
        ticket: QueryTicket,
        generation: u64,
        rows: &Arc<Vec<usize>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let current = self.current_generation();
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
                history.walk.replace(rows);
                history.error = None;
                let previous = history.selected;
                history.selected = choose_selection(history.rows(), previous);
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
        history.walk.cancel();
        history.error = Some(error.to_owned());
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
        if history.slot.needs_refresh(self.current_generation()) {
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
        let rows = history.rows();
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

    /// Requests the selected commit's diff of the path with the settings'
    /// comparison options, under the name the file had in that commit.
    pub(crate) fn request_history_diff(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.file_history.as_ref().and_then(|it| it.selected) else {
            return;
        };
        let Some(commit) = self.source().and_then(|source| source.commit_id(index)) else {
            return;
        };
        let path = self
            .file_history
            .as_ref()
            .map(|history| history.path_at(index, self.source().as_ref()))
            .unwrap_or_default();
        let (Some(session), Some(history)) = (self.session.as_ref(), self.file_history.as_mut())
        else {
            return;
        };
        let ticket = session.request_diff_with(
            DiffRequest::Commit {
                commit,
                path: path.clone(),
            },
            history_diff_options(cx),
        );
        if let Some(previous) = history.diff_ticket.replace(ticket) {
            session.cancel_diff(previous);
        }
        history.diff_path = Some(path);
        history.view = None;
        history.diff_rows = Rc::new(Vec::new());
        history.diff_error = None;
        cx.notify();
    }

    /// The snapshot generation the rows must belong to.
    fn current_generation(&self) -> u64 {
        self.session
            .as_ref()
            .map_or(0, |session| session.snapshot().generation)
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
        let count = {
            let rows = history.rows().len();
            let plural = if rows == 1 { "" } else { "s" };
            if history.walking() {
                format!("{rows} commit{plural} · loading…")
            } else {
                format!("{rows} commit{plural}")
            }
        };

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
        let walking = history.walking();
        if history.rows().is_empty() && walking {
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
        }
        if history.rows().is_empty() {
            return list_message(
                "No loaded commit touches this path",
                colors.muted_foreground,
                colors,
            );
        }

        let selected = history.selected;
        let list_colors = colors.clone();
        let rows = history.rows_shared();
        let names = Arc::clone(&history.names);
        let requested = history.path.clone();
        let count = rows.len() + usize::from(walking);
        uniform_list("file-history-commits", count, move |range, _window, _cx| {
            range
                .map(|position| {
                    if position >= rows.len() {
                        return loading_row(&list_colors);
                    }
                    let index = rows[position];
                    let Some(row) = source.row(index) else {
                        return div().into_any_element();
                    };
                    let date = style::date_label(
                        row.time,
                        style::now_epoch(),
                        list_colors.design.date_format,
                    );
                    let rename = rename_label(
                        &requested,
                        source
                            .commit_id(index)
                            .and_then(|commit| names.get(&commit))
                            .map(String::as_str),
                    );
                    let element = crate::ui::list_row(
                        ("file-history-row", position),
                        crate::ui::RowState::NORMAL.selected(selected == Some(index)),
                        &list_colors.design,
                    )
                    // Two lines: the summary, and the name the file had there
                    // when a rename makes the row older than the requested
                    // path. Every row keeps the taller height so the list stays
                    // uniform.
                    .h(px(38.0))
                    .gap_3()
                    .mx(px(0.0))
                    .w_full()
                    .px_3()
                    .text_size(px(list_colors.design.small()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(div().min_w_0().truncate().child(row.summary.to_string()))
                            .children(rename.map(|rename| {
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(10.0))
                                    .text_color(list_colors.muted_foreground)
                                    .child(rename)
                            })),
                    )
                    .child(
                        div()
                            .w(px(92.0))
                            .flex_none()
                            .truncate()
                            .text_color(list_colors.muted_foreground)
                            .child(row.author.to_string()),
                    )
                    .child(
                        div()
                            .w(px(96.0))
                            .flex_none()
                            .truncate()
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
        })
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
        let path = history.selected.map_or_else(
            || history.path.clone(),
            |index| history.path_at(index, self.source().as_ref()),
        );

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
                    .child(path),
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

/// The list's last row while the walk runs.
fn loading_row(colors: &Colors) -> AnyElement {
    crate::ui::list_row(
        "file-history-loading",
        crate::ui::RowState::NORMAL,
        &colors.design,
    )
    .gap_2()
    .h(px(38.0))
    .mx(px(0.0))
    .w_full()
    .px_3()
    .text_size(px(colors.design.small()))
    .text_color(colors.muted_foreground)
    .child(Spinner::new().color(colors.muted_foreground))
    .child("Loading…")
    .into_any_element()
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
    use super::*;

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

    #[test]
    fn batches_append_to_the_walk_that_owns_them() {
        let mut walk = BatchRows::default();
        walk.begin(QueryTicket(1));
        assert_eq!(
            walk.append(QueryTicket(1), 3, 3, &[10, 11]),
            Batch::Appended(2)
        );
        assert_eq!(walk.append(QueryTicket(1), 3, 3, &[12]), Batch::Appended(1));
        assert_eq!(*walk.rows, vec![10, 11, 12]);
        assert!(walk.done(QueryTicket(1)));
        assert_eq!(walk.ticket, None);
        assert!(!walk.rows.is_empty());
    }

    #[test]
    fn batches_from_a_replaced_ticket_are_dropped() {
        let mut walk = BatchRows::default();
        walk.begin(QueryTicket(1));
        walk.append(QueryTicket(1), 3, 3, &[10]);
        walk.begin(QueryTicket(2));
        assert_eq!(
            walk.append(QueryTicket(1), 3, 3, &[11]),
            Batch::Ignored,
            "a late batch for the previous selection must not append"
        );
        assert!(walk.rows.is_empty());
        assert!(!walk.done(QueryTicket(1)), "and it cannot finish the walk");
        assert_eq!(walk.append(QueryTicket(2), 3, 3, &[20]), Batch::Appended(1));
    }

    #[test]
    fn batches_for_a_replaced_snapshot_are_stale() {
        let mut walk = BatchRows::default();
        walk.begin(QueryTicket(4));
        assert_eq!(walk.append(QueryTicket(4), 2, 3, &[7]), Batch::Stale);
        assert!(
            walk.rows.is_empty(),
            "rows of another generation select the wrong commits"
        );
        assert_eq!(walk.append(QueryTicket(4), 3, 3, &[8]), Batch::Appended(1));
    }

    #[test]
    fn a_complete_answer_replaces_the_streamed_rows() {
        let mut walk = BatchRows::default();
        walk.begin(QueryTicket(5));
        walk.append(QueryTicket(5), 1, 1, &[1, 2]);
        walk.replace(&[3, 4]);
        assert_eq!(*walk.rows, vec![3, 4]);
        assert!(!walk.done(QueryTicket(5)));
    }

    #[test]
    fn renaming_is_labelled_against_the_requested_path() {
        assert_eq!(
            rename_label("src/new.rs", Some("src/old.rs")).as_deref(),
            Some("renamed from src/old.rs")
        );
        assert_eq!(rename_label("src/new.rs", Some("src/new.rs")), None);
        assert_eq!(rename_label("src/new.rs", None), None);
    }
}
