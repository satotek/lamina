//! File history: the commits that touched one path, with that path's diff per
//! commit.
//!
//! The list comes from `Session::path_history_streaming`, which walks history on
//! the session's query worker and reports commits (along with the path each
//! commit knew the file under) in batches, independently of graph snapshots, so
//! a long history fills in while both walks run. The diff comes from
//! `DiffRequest::Commit`. Both are keyed by ticket, so a late answer can never
//! replace the view for a newer selection.

use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement, SharedString,
    Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};
use gpui_component::{ActiveTheme as _, Selectable as _, Theme, spinner::Spinner};
use strata_core::session::{
    DiffRequest, DiffTicket, DiffView, PathHistoryCommit, QueryTicket, RepoSnapshot,
};
use strata_core::{HeadInfo, Oid, RefInfo, StashInfo};

use crate::data;
use crate::diff_view::{self, DiffColors, DiffContext, Row as DiffRow};
use crate::repo_view::{RepoView, RowsTarget, history_diff_options};
use crate::scrollbar::WithScrollbar as _;
use crate::settings::Settings;
use crate::style::{self, Design};

const LIST_WIDTH: f32 = 520.0;

/// Head and ref pointers observed when a file-history walk started, used to
/// decide whether a `SnapshotUpdated` event reflects moved refs rather than an
/// append-only chunk from the initial history walk.
pub(crate) struct SnapshotHeads {
    head: Option<HeadInfo>,
    refs: Arc<Vec<RefInfo>>,
    stashes: Arc<Vec<StashInfo>>,
}

impl SnapshotHeads {
    pub(crate) fn from_snapshot(snapshot: &RepoSnapshot) -> Self {
        Self {
            head: snapshot.head.clone(),
            refs: Arc::clone(&snapshot.refs),
            stashes: Arc::clone(&snapshot.stashes),
        }
    }

    pub(crate) fn changed(&self, snapshot: &RepoSnapshot) -> bool {
        if self.head != snapshot.head {
            return true;
        }
        if !Arc::ptr_eq(&self.refs, &snapshot.refs) && *self.refs != *snapshot.refs {
            return true;
        }
        if !Arc::ptr_eq(&self.stashes, &snapshot.stashes) && *self.stashes != *snapshot.stashes {
            return true;
        }
        false
    }
}

/// State of the file-history view. Rendering reads it; [`RepoView`] updates it
/// from session events and input.
pub(crate) struct FileHistory {
    pub(crate) path: String,
    /// Rows gathered from the streaming walk, newest first.
    walk: BatchRows,
    /// Heads and refs active when the current walk started.
    heads: Option<SnapshotHeads>,
    /// Commit whose diff is shown.
    pub(crate) selected: Option<Oid>,
    scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,

    diff_ticket: Option<DiffTicket>,
    view: Option<Arc<DiffView>>,
    pub(crate) diff_rows: Rc<Vec<DiffRow>>,
    /// Which hidden lines of the selected commit's diff are revealed.
    pub(crate) context: DiffContext,
    diff_error: Option<String>,

    error: Option<String>,
}

impl FileHistory {
    fn new(path: String) -> Self {
        Self {
            path,
            walk: BatchRows::default(),
            heads: None,
            selected: None,
            scroll: UniformListScrollHandle::default(),
            diff_scroll: UniformListScrollHandle::default(),
            diff_ticket: None,
            view: None,
            diff_rows: Rc::new(Vec::new()),
            context: DiffContext::default(),
            diff_error: None,
            error: None,
        }
    }

    pub(crate) fn rows(&self) -> &[PathHistoryCommit] {
        &self.walk.rows
    }

    fn rows_shared(&self) -> Arc<Vec<PathHistoryCommit>> {
        Arc::clone(&self.walk.rows)
    }

    fn walking(&self) -> bool {
        self.walk.ticket.is_some()
    }

    fn entry_for(&self, commit: Oid) -> Option<&PathHistoryCommit> {
        self.rows().iter().find(|entry| entry.commit.id == commit)
    }

    pub(crate) fn path_for(&self, commit: Oid) -> String {
        self.entry_for(commit)
            .map_or_else(|| self.path.clone(), |entry| entry.path.clone())
    }

    pub(crate) fn selected_commit_and_path(&self) -> Option<(Oid, String)> {
        let commit = self
            .selected
            .or_else(|| self.rows().first().map(|entry| entry.commit.id))?;
        Some((commit, self.path_for(commit)))
    }
}

/// Rows accumulated from one streaming walk, and the ticket that owns them.
///
/// The query worker emits batches and a done event, and a ticket can be
/// cancelled or replaced by a newer selection at any point in between; keeping
/// the ticket next to the rows makes "is this batch still mine?" a single
/// comparison instead of a race.
#[derive(Debug, Default)]
struct BatchRows {
    ticket: Option<QueryTicket>,
    /// Shared with the list closure, which appends nothing but must not copy
    /// the whole history on every frame.
    rows: Arc<Vec<PathHistoryCommit>>,
}

impl BatchRows {
    /// Starts a walk, replacing whatever the previous one had gathered.
    fn begin(&mut self, ticket: QueryTicket) -> Option<QueryTicket> {
        self.rows = Arc::new(Vec::new());
        self.ticket.replace(ticket)
    }

    fn append(&mut self, ticket: QueryTicket, entries: &[PathHistoryCommit]) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        Arc::make_mut(&mut self.rows).extend_from_slice(entries);
        true
    }

    /// True when `ticket` is the walk's; the ticket is released either way.
    fn done(&mut self, ticket: QueryTicket) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        self.ticket = None;
        true
    }

    fn fail(&mut self, ticket: QueryTicket) -> bool {
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
fn choose_selection(rows: &[PathHistoryCommit], current: Option<Oid>) -> Option<Oid> {
    match current {
        Some(current) if rows.iter().any(|entry| entry.commit.id == current) => Some(current),
        _ => rows.first().map(|entry| entry.commit.id),
    }
}

/// `"renamed from <old path>"` for a commit that knew the file under another
/// name, or `None` when it had the requested one.
fn rename_label(requested: &str, path: Option<&str>) -> Option<String> {
    let path = path?;
    (path != requested).then(|| t!("file_history.renamed_from", path = path).to_string())
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
        // The file history replaces the history pane, so the reflog has to
        // hand the main area back before it opens.
        if self.mode == crate::worktree_panel::MainMode::Reflog {
            self.leave_reflog(cx);
            self.mode = crate::worktree_panel::MainMode::History;
        }
        self.discard_blame();
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
        let ticket = session.path_history_streaming(path);
        let snapshot = session.snapshot();
        let Some(history) = self.file_history.as_mut() else {
            return;
        };
        if let Some(previous) = history.walk.begin(ticket) {
            session.cancel_query(previous);
        }
        history.heads = snapshot
            .head
            .is_some()
            .then(|| SnapshotHeads::from_snapshot(&snapshot));
        history.error = None;
        self.notify_if_active(cx);
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
        self.notify_if_active(cx);
        true
    }

    /// Cancels the outstanding query and diff without touching focus.
    pub(crate) fn cancel_file_history(&mut self) {
        let Some(history) = self.file_history.as_mut() else {
            return;
        };
        let query = history.walk.cancel();
        let diff = history.diff_ticket.take();
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

    /// One batch of the streaming walk.
    pub(crate) fn file_history_batch(
        &mut self,
        ticket: QueryTicket,
        entries: &[PathHistoryCommit],
        cx: &mut Context<Self>,
    ) -> bool {
        let request_diff = {
            let Some(history) = self.file_history.as_mut() else {
                return false;
            };
            if !history.walk.append(ticket, entries) {
                return false;
            }
            if history.selected.is_none()
                && let Some(first) = history.rows().first().map(|entry| entry.commit.id)
            {
                history.selected = Some(first);
                true
            } else {
                false
            }
        };
        self.notify_if_active(cx);
        if request_diff {
            self.request_history_diff(cx);
        }
        true
    }

    /// The walk reached the end of history.
    pub(crate) fn file_history_done(
        &mut self,
        ticket: QueryTicket,
        cx: &mut Context<Self>,
    ) -> bool {
        let moved = {
            let Some(history) = self.file_history.as_mut() else {
                return false;
            };
            if !history.walk.done(ticket) {
                return false;
            }
            let previous = history.selected;
            history.selected = choose_selection(history.rows(), previous);
            let moved = history.selected != previous;
            if moved {
                history.view = None;
                history.diff_rows = Rc::new(Vec::new());
                history.context.reset();
                history.diff_error = None;
            }
            moved
        };
        self.notify_if_active(cx);
        if moved {
            self.request_history_diff(cx);
        }
        true
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
        if !history.walk.fail(ticket) {
            return false;
        }
        history.error = Some(error.to_owned());
        self.notify_if_active(cx);
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
        history.context.reset();
        history.diff_rows = Rc::new(history.context.rows(view));
        history.view = Some(Arc::clone(view));
        history.diff_error = None;
        self.notify_if_active(cx);
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
        self.notify_if_active(cx);
        true
    }

    /// Restarts the walk only when HEAD, refs, or stashes actually changed.
    /// Append-only snapshot publishes during the initial history walk leave the
    /// path-history walk running uninterrupted.
    pub(crate) fn file_history_on_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if self.file_history.is_none() || self.mode != crate::worktree_panel::MainMode::History {
            return;
        }
        let snapshot = session.snapshot();
        if snapshot.head.is_none() {
            return;
        }
        let restart = {
            let Some(history) = self.file_history.as_mut() else {
                return;
            };
            match &history.heads {
                Some(recorded) => recorded.changed(&snapshot),
                None => {
                    history.heads = Some(SnapshotHeads::from_snapshot(&snapshot));
                    false
                }
            }
        };
        if restart {
            self.start_file_history(cx);
        }
    }

    /// Selects a commit and asks for that commit's diff of the path.
    pub(crate) fn select_history_commit(&mut self, commit: Oid, cx: &mut Context<Self>) {
        if self.file_history.as_ref().and_then(|it| it.selected) == Some(commit) {
            return;
        }
        if let Some(history) = self.file_history.as_mut() {
            history.selected = Some(commit);
            history.view = None;
            history.diff_rows = Rc::new(Vec::new());
            history.context.reset();
            history.diff_error = None;
        }
        self.request_history_diff(cx);
    }

    /// Applies one press on a hidden-lines bar of the file-history diff.
    pub(crate) fn expand_file_history_gap(
        &mut self,
        gap: usize,
        action: strata_core::diff::GapAction,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .file_history
            .as_ref()
            .and_then(|history| history.view.clone())
        else {
            return;
        };
        let Some(context) = self.file_history.as_mut().and_then(|history| {
            history
                .context
                .expand(gap, action, &view)
                .then(|| history.context.clone())
        }) else {
            return;
        };
        let revision = context.revision();
        let build_view = Arc::clone(&view);
        self.rebuild_rows_in_background(RowsTarget::FileHistory, revision, cx, move || {
            context.rows(&build_view)
        });
        self.notify_if_active(cx);
    }

    /// The file-history view's "Whole file" toggle.
    pub(crate) fn toggle_file_history_whole_file(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self
            .file_history
            .as_ref()
            .and_then(|history| history.view.clone())
        else {
            return;
        };
        let Some(context) = self.file_history.as_mut().and_then(|history| {
            history
                .context
                .toggle_whole_file(&view)
                .then(|| history.context.clone())
        }) else {
            self.notify_if_active(cx);
            return;
        };
        let revision = context.revision();
        let build_view = Arc::clone(&view);
        self.rebuild_rows_in_background(RowsTarget::FileHistory, revision, cx, move || {
            context.rows(&build_view)
        });
        self.notify_if_active(cx);
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
            .and_then(|commit| rows.iter().position(|entry| entry.commit.id == commit))
            .map_or(0, |position| {
                (position as isize + delta).clamp(0, rows.len() as isize - 1) as usize
            });
        let Some(commit) = rows.get(position).map(|entry| entry.commit.id) else {
            return;
        };
        history
            .scroll
            .scroll_to_item(position, gpui::ScrollStrategy::Center);
        self.select_history_commit(commit, cx);
    }

    /// Requests the selected commit's diff of the path with the settings'
    /// comparison options, under the name the file had in that commit.
    pub(crate) fn request_history_diff(&mut self, cx: &mut Context<Self>) {
        let (Some(session), Some(history)) = (self.session.as_ref(), self.file_history.as_mut())
        else {
            return;
        };
        let Some(commit) = history.selected else {
            return;
        };
        let path = history.path_for(commit);
        let ticket = session.request_diff_with(
            DiffRequest::Commit { commit, path },
            history_diff_options(cx),
        );
        if let Some(previous) = history.diff_ticket.replace(ticket) {
            session.cancel_diff(previous);
        }
        history.view = None;
        history.diff_rows = Rc::new(Vec::new());
        history.context.reset();
        history.diff_error = None;
        self.notify_if_active(cx);
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
            crate::ui::button("file-history-back", t!("file_history.back").to_string())
                .size(crate::ui::ButtonSize::Tiny)
                .ghost()
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.close_file_history(window, cx);
                    });
                })
        };
        let blame = {
            let entity = entity.clone();
            crate::ui::button("file-history-blame", t!("blame.title").to_string())
                .icon(gpui_kit_assets::IconName::FileText)
                .size(crate::ui::ButtonSize::Tiny)
                .ghost()
                .disabled(history.selected.is_none())
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.blame_file_history(window, cx);
                    });
                })
        };
        let count = {
            let rows = history.rows().len();
            let walking = history.walking();
            match (rows, walking) {
                (1, true) => t!("file_history.count_loading.one", count = rows).to_string(),
                (_, true) => t!("file_history.count_loading.other", count = rows).to_string(),
                (1, false) => t!("file_history.count.one", count = rows).to_string(),
                _ => t!("file_history.count.other", count = rows).to_string(),
            }
        };

        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .h(colors.design.px(40.0))
            .flex_none()
            .px_3p5()
            .bg(colors.background)
            .border_b_1()
            .border_color(colors.border)
            .child(back)
            .child(
                div()
                    .flex_none()
                    .text_size(colors.design.px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t!("file_history.title").to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(colors.mono.clone())
                    .text_size(colors.design.px(11.5))
                    .text_color(colors.accent)
                    .child(history.path.clone()),
            )
            .child(blame)
            .child(
                div()
                    .flex_none()
                    .text_size(colors.design.px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(count),
            );

        let diff_colors = DiffColors::from_design(&colors.design);
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
                .w(colors.design.px(LIST_WIDTH))
                .flex_none()
                .border_r_1()
                .border_color(colors.border)
                .child(Spinner::new().color(colors.muted_foreground))
                .child(
                    div()
                        .text_size(colors.design.px(11.0))
                        .text_color(colors.muted_foreground)
                        .child(t!("file_history.walking").to_string()),
                )
                .into_any_element();
        }
        if history.rows().is_empty() {
            return list_message(
                t!("file_history.empty").to_string(),
                colors.muted_foreground,
                colors,
            );
        }

        let selected = history.selected;
        let list_colors = colors.clone();
        let rows = history.rows_shared();
        let requested = history.path.clone();
        let count = rows.len() + usize::from(walking);
        uniform_list("file-history-commits", count, move |range, _window, _cx| {
            range
                .map(|position| {
                    if position >= rows.len() {
                        return loading_row(&list_colors);
                    }
                    let entry = &rows[position];
                    let commit = &entry.commit;
                    let commit_id = commit.id;
                    let date = style::date_label(
                        commit.author_time,
                        style::now_epoch(),
                        list_colors.design.date_format,
                    );
                    let rename = rename_label(&requested, Some(entry.path.as_str()));
                    let element = crate::ui::list_row(
                        ("file-history-row", position),
                        crate::ui::RowState::NORMAL.selected(selected == Some(commit_id)),
                        &list_colors.design,
                    )
                    // Two lines: the summary, and the name the file had there
                    // when a rename makes the row older than the requested
                    // path. Every row keeps the taller height so the list stays
                    // uniform.
                    .h(list_colors.design.px(38.0))
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
                            .child(div().min_w_0().truncate().child(commit.summary.clone()))
                            .children(rename.map(|rename| {
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(list_colors.design.px(10.0))
                                    .text_color(list_colors.muted_foreground)
                                    .child(rename)
                            })),
                    )
                    .child(
                        div()
                            .w(list_colors.design.px(92.0))
                            .flex_none()
                            .truncate()
                            .text_color(list_colors.muted_foreground)
                            .child(commit.author_name.clone()),
                    )
                    .child(
                        div()
                            .w(list_colors.design.px(96.0))
                            .flex_none()
                            .truncate()
                            .text_color(list_colors.muted_foreground)
                            .child(date),
                    )
                    .child(
                        div()
                            .w(list_colors.design.px(58.0))
                            .flex_none()
                            .font_family(list_colors.mono.clone())
                            .text_color(list_colors.muted_foreground)
                            .child(data::short_oid(commit_id)),
                    );
                    let entity = entity.clone();
                    element
                        .cursor_pointer()
                        .on_click(move |_event, _window, cx| {
                            entity.update(cx, |this, cx| {
                                this.select_history_commit(commit_id, cx);
                            });
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&history.scroll)
        .with_scrollbar(&history.scroll)
        .w(colors.design.px(LIST_WIDTH))
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
        let commit = history.selected;
        let path = commit.map_or_else(|| history.path.clone(), |id| history.path_for(id));

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .h(colors.design.px(36.0))
            .flex_none()
            .px_3p5()
            .bg(colors.title_bar)
            .border_b_1()
            .border_color(colors.border)
            .text_size(colors.design.px(11.0))
            .child(
                div()
                    .truncate()
                    .font_family(colors.mono.clone())
                    .child(path),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .text_color(colors.muted_foreground)
                            .child(match commit {
                                Some(commit) => t!(
                                    "file_history.row_detail",
                                    author = data::short_oid(commit),
                                    count = history.diff_rows.len()
                                )
                                .to_string(),
                                None => t!("file_history.no_selection").to_string(),
                            }),
                    )
                    .child(if history.view.is_some() {
                        let active = history.context.is_whole_file();
                        let entity = self.entity();
                        crate::ui::button(
                            "history-toggle-whole-file",
                            t!("history.whole_file").to_string(),
                        )
                        .size(crate::ui::ButtonSize::Tiny)
                        .ghost()
                        .selected(active)
                        .tooltip(if active {
                            t!("worktree.show_only_changes").to_string()
                        } else {
                            t!("worktree.show_every_line").to_string()
                        })
                        .on_click(move |_event, _window, cx| {
                            entity.update(cx, |this, cx| this.toggle_file_history_whole_file(cx));
                        })
                        .into_any_element()
                    } else {
                        div().into_any_element()
                    })
                    .child(
                        history
                            .view
                            .as_ref()
                            .and_then(|view| {
                                crate::image_diff::svg_toggle(&self.image_diff, view, self.entity())
                            })
                            .unwrap_or_else(|| div().into_any_element()),
                    ),
            );

        let body = if let Some(error) = &history.diff_error {
            message(error, colors.danger, colors)
        } else if history.selected.is_none() {
            message(
                t!("file_history.select_commit").to_string(),
                colors.muted_foreground,
                colors,
            )
        } else if let Some(view) = &history.view {
            if let Some(image) =
                crate::image_diff::body(&self.image_diff, view, &colors.design, self.entity(), cx)
            {
                image
            } else if view.diff.binary {
                message(
                    t!("worktree.binary_file").to_string(),
                    colors.muted_foreground,
                    colors,
                )
            } else if history.diff_rows.is_empty() {
                message(
                    t!("worktree.no_text_changes").to_string(),
                    colors.muted_foreground,
                    colors,
                )
            } else {
                let list = self.history_diff_rows(colors, diff_colors, cx);
                match history.context.notice() {
                    Some(notice) => div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(crate::ui::banner(
                            crate::ui::BannerKind::Warning,
                            notice,
                            &colors.design,
                        ))
                        .child(list)
                        .into_any_element(),
                    None => list,
                }
            }
        } else {
            crate::ui::loading_state(t!("worktree.computing_diff").to_string(), &colors.design)
                .into_any_element()
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
            diff_view::DiffList {
                id: "file-history-diff",
                rows: Rc::clone(&history.diff_rows),
                view: history.view.clone(),
                colors: *diff_colors,
                mono: colors.mono.clone(),
                scroll: &history.diff_scroll,
                on_gap: self.gap_handler(),
            },
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
    .h(colors.design.px(38.0))
    .mx(px(0.0))
    .w_full()
    .px_3()
    .text_size(px(colors.design.small()))
    .text_color(colors.muted_foreground)
    .child(Spinner::new().color(colors.muted_foreground))
    .child(t!("common.loading").to_string())
    .into_any_element()
}

fn list_message(text: impl Into<gpui::SharedString>, color: Hsla, colors: &Colors) -> AnyElement {
    div()
        .flex()
        .w(colors.design.px(LIST_WIDTH))
        .flex_none()
        .border_r_1()
        .border_color(colors.border)
        .child(crate::ui::message(text, color, &colors.design))
        .into_any_element()
}

fn message(text: impl Into<gpui::SharedString>, color: Hsla, colors: &Colors) -> AnyElement {
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
    use std::collections::HashMap;
    use strata_core::session::CommitList;
    use strata_core::{CommitInfo, RefKind, RepoState, WorktreeStatus};

    fn oid(n: u8) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        Oid::from(bytes)
    }

    fn entry(n: u8) -> PathHistoryCommit {
        PathHistoryCommit {
            commit: CommitInfo {
                id: oid(n),
                parents: Vec::new(),
                summary: format!("commit {n}"),
                author_name: "Test".to_owned(),
                author_email: "test@example.com".to_owned(),
                author_time: i64::from(n),
                committer_time: i64::from(n),
                shallow_boundary: false,
            },
            path: "file.txt".to_owned(),
        }
    }

    fn snapshot_with(
        head: Option<HeadInfo>,
        refs: Arc<Vec<RefInfo>>,
        generation: u64,
    ) -> RepoSnapshot {
        RepoSnapshot {
            generation,
            head,
            refs,
            refs_by_commit: Arc::new(HashMap::new()),
            commits: CommitList::default(),
            commit_index: Arc::new(HashMap::new()),
            status: Arc::new(WorktreeStatus::default()),
            stashes: Arc::new(Vec::new()),
            upstreams: Arc::new(HashMap::new()),
            remotes: Vec::new(),
            state: RepoState::Normal,
            conflicts: Arc::new(Vec::new()),
            busy: None,
        }
    }

    #[test]
    fn selection_keeps_a_listed_commit_and_otherwise_takes_the_newest() {
        let rows = [entry(4), entry(7), entry(9)];
        assert_eq!(choose_selection(&rows, Some(oid(7))), Some(oid(7)));
        assert_eq!(
            choose_selection(&rows, Some(oid(8))),
            Some(oid(4)),
            "a commit that left the list falls back to the newest match"
        );
        assert_eq!(choose_selection(&rows, None), Some(oid(4)));
        assert_eq!(choose_selection(&[], Some(oid(7))), None);
        assert_eq!(choose_selection(&[], None), None);
    }

    #[test]
    fn batches_append_to_the_walk_that_owns_them() {
        let mut walk = BatchRows::default();
        walk.begin(QueryTicket(1));
        assert!(walk.append(QueryTicket(1), &[entry(10), entry(11)]));
        assert!(walk.append(QueryTicket(1), &[entry(12)]));
        let ids: Vec<_> = walk.rows.iter().map(|it| it.commit.id).collect();
        assert_eq!(ids, vec![oid(10), oid(11), oid(12)]);
        assert!(walk.done(QueryTicket(1)));
        assert_eq!(walk.ticket, None);
        assert!(!walk.rows.is_empty());
    }

    #[test]
    fn batches_from_a_replaced_ticket_are_dropped() {
        let mut walk = BatchRows::default();
        walk.begin(QueryTicket(1));
        walk.append(QueryTicket(1), &[entry(10)]);
        assert_eq!(walk.begin(QueryTicket(2)), Some(QueryTicket(1)));
        assert!(
            !walk.append(QueryTicket(1), &[entry(11)]),
            "a late batch for the previous selection must not append"
        );
        assert!(walk.rows.is_empty());
        assert!(!walk.done(QueryTicket(1)), "and it cannot finish the walk");
        assert!(walk.append(QueryTicket(2), &[entry(20)]));
    }

    #[test]
    fn snapshot_heads_ignore_append_only_updates_and_detect_ref_or_head_changes() {
        let head = Some(HeadInfo::Branch {
            short_name: "main".to_owned(),
            target: Some(oid(1)),
        });
        let refs = Arc::new(vec![RefInfo {
            full_name: "refs/heads/main".to_owned(),
            short_name: "main".to_owned(),
            kind: RefKind::LocalBranch,
            target: oid(1),
        }]);
        let initial = snapshot_with(head.clone(), Arc::clone(&refs), 1);
        let recorded = SnapshotHeads::from_snapshot(&initial);

        // Next chunk during initial walk: generation bumps, same head/refs/stashes.
        let chunk = snapshot_with(head.clone(), Arc::clone(&refs), 2);
        assert!(!recorded.changed(&chunk));

        // Ref target moved: must trigger restart.
        let updated_refs = Arc::new(vec![RefInfo {
            full_name: "refs/heads/main".to_owned(),
            short_name: "main".to_owned(),
            kind: RefKind::LocalBranch,
            target: oid(2),
        }]);
        let moved_ref = snapshot_with(head, updated_refs, 3);
        assert!(recorded.changed(&moved_ref));

        // HEAD moved: must trigger restart.
        let moved_head = snapshot_with(
            Some(HeadInfo::Branch {
                short_name: "topic".to_owned(),
                target: Some(oid(3)),
            }),
            refs,
            4,
        );
        assert!(recorded.changed(&moved_head));
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
