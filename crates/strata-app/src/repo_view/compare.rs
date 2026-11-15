//! Compare two commits: the files that differ between them, with one file's
//! diff beside the list.
//!
//! The comparison itself comes from `Session::request_compare`: the changed
//! files and their line counts between the two trees, plus how many commits
//! each side has that the other does not. A chosen file's diff comes from
//! `DiffRequest::Compare`, which is the same worker, options and highlighter
//! that a commit's detail diff uses. Tickets guard both answers, so a late
//! reply for a pair the user already swapped cannot replace the view.
//!
//! The view fills the main area while it is open, like the file history, and
//! keeps the History details split: the file column and the diff share the
//! saved fraction and the same draggable boundary.

use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Axis, Context, Entity, FontWeight, IntoElement, ParentElement, Styled,
    UniformListScrollHandle, Window, div, px, relative, uniform_list,
};
use gpui_component::ActiveTheme as _;
use gpui_component::Selectable as _;
use gpui_kit_assets::IconName;
use strata_core::session::{
    CompareFile, CompareMode, Comparison, DiffRequest, DiffTicket, DiffView, QueryTicket,
};
use strata_core::{HeadInfo, Oid};

use super::details::file_stat_label;
use super::split::{self, files_fraction};
use super::{RepoView, RowsTarget};
use crate::data;
use crate::diff_view::{self, DiffColors, DiffContext, Row as DiffRow};
use crate::repo_view::history_diff_options;
use crate::scrollbar::WithScrollbar as _;
use crate::style::{self, Design};
use crate::ui::{self, ButtonSize, split_handle};

/// One side of the comparison, as the menu that opened it named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompareSide {
    /// The commit, or `None` when the side is the empty tree: a range that
    /// starts at a root commit has nothing on its left.
    pub(crate) oid: Option<Oid>,
    /// A branch name when the side came from one. The header shows it beside
    /// the short id, because two branch tips of the same commit look alike.
    pub(crate) label: Option<String>,
    /// The files on disk instead of a commit's tree.
    pub(crate) worktree: bool,
}

impl CompareSide {
    pub(crate) fn commit(oid: Oid) -> Self {
        Self {
            oid: Some(oid),
            label: None,
            worktree: false,
        }
    }

    pub(crate) fn branch(name: impl Into<String>, oid: Oid) -> Self {
        Self {
            oid: Some(oid),
            label: Some(name.into()),
            worktree: false,
        }
    }

    /// The empty tree, for a range whose oldest commit is a root.
    pub(crate) fn empty_tree() -> Self {
        Self {
            oid: None,
            label: Some(t!("compare.empty_tree").to_string()),
            worktree: false,
        }
    }

    /// The uncommitted files on disk, for a comparison with the working tree.
    pub(crate) fn worktree() -> Self {
        Self {
            oid: None,
            label: Some(t!("compare.working_tree").to_string()),
            worktree: true,
        }
    }

    /// The name the header leads with: the branch, or the short id. The short
    /// id follows a branch name, but is not repeated when it is the title.
    fn title(&self) -> String {
        if self.worktree {
            return t!("compare.working_tree").to_string();
        }
        match (&self.label, self.oid) {
            (Some(label), _) => label.clone(),
            (None, Some(oid)) => data::short_oid(oid),
            (None, None) => t!("compare.empty_tree").to_string(),
        }
    }
}

/// State of the compare view. Rendering reads it; [`RepoView`] updates it from
/// menu actions, input and session events.
pub(crate) struct Compare {
    a: CompareSide,
    b: CompareSide,
    mode: CompareMode,
    ticket: Option<QueryTicket>,
    comparison: Option<Arc<Comparison>>,
    /// The comparison's files, shared with the list closure so a frame does
    /// not copy them.
    files: Rc<Vec<CompareFile>>,
    error: Option<String>,
    /// Index into `comparison.files` of the file whose diff is shown.
    selected: Option<usize>,
    diff_ticket: Option<DiffTicket>,
    view: Option<Arc<DiffView>>,
    pub(crate) diff_rows: Rc<Vec<DiffRow>>,
    /// Which hidden lines of the selected file's diff are revealed.
    pub(crate) context: DiffContext,
    diff_error: Option<String>,
    file_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
}

impl Compare {
    fn new(a: CompareSide, b: CompareSide, mode: CompareMode) -> Self {
        Self {
            a,
            b,
            mode,
            ticket: None,
            comparison: None,
            files: Rc::new(Vec::new()),
            error: None,
            selected: None,
            diff_ticket: None,
            view: None,
            diff_rows: Rc::new(Vec::new()),
            context: DiffContext::default(),
            diff_error: None,
            file_scroll: UniformListScrollHandle::default(),
            diff_scroll: UniformListScrollHandle::default(),
        }
    }

    fn files(&self) -> &[CompareFile] {
        &self.files
    }

    fn selected_file(&self) -> Option<&CompareFile> {
        self.selected.and_then(|index| self.files().get(index))
    }
}

// ---------------------------------------------------------------------------
// Menu rules.
//
// The menus ask these instead of repeating the conditions in a closure, so the
// reason a compare entry is disabled is testable without a window.
// ---------------------------------------------------------------------------

/// Why "Compare these commits" cannot run, or `None` when it can.
///
/// The caller passes only the selection's real commits: the synthetic
/// benchmark has ids but no trees, so its list arrives empty and the entry is
/// disabled with this reason.
pub(crate) fn two_commit_blocked(repo_open: bool, commits: &[Oid]) -> Option<String> {
    if !repo_open {
        return Some(t!("compare.no_repository").to_string());
    }
    if commits.len() != 2 {
        return Some(t!("compare.select_two").to_string());
    }
    None
}

/// Why "Compare with HEAD" cannot run, or `None` when it can.
pub(crate) fn with_head_blocked(
    repo_open: bool,
    commit: Option<Oid>,
    head: Option<Oid>,
) -> Option<String> {
    if !repo_open {
        return Some(t!("compare.no_repository").to_string());
    }
    if commit.is_none() {
        return Some(t!("compare.no_commit").to_string());
    }
    if head.is_none() {
        return Some(t!("compare.no_head").to_string());
    }
    None
}

/// Why "Compare with working tree" cannot run, or `None` when it can.
pub(crate) fn with_worktree_blocked(repo_open: bool, commit: Option<Oid>) -> Option<String> {
    if !repo_open {
        return Some(t!("compare.no_repository").to_string());
    }
    if commit.is_none() {
        return Some(t!("compare.no_commit").to_string());
    }
    None
}

/// Why "Compare with current branch" cannot run, or `None` when it can.
pub(crate) fn with_current_branch_blocked(
    repo_open: bool,
    is_current: bool,
    busy: bool,
    head: Option<Oid>,
) -> Option<String> {
    if !repo_open {
        return Some(t!("compare.no_repository").to_string());
    }
    if is_current {
        return Some(t!("compare.current_branch").to_string());
    }
    if busy {
        return Some(t!("compare.busy").to_string());
    }
    if head.is_none() {
        return Some(t!("compare.no_head").to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// RepoView glue.
// ---------------------------------------------------------------------------

impl RepoView {
    /// Opens the compare view for two sides. A real session is required: the
    /// synthetic benchmark has no trees to compare.
    pub(crate) fn open_compare(
        &mut self,
        a: CompareSide,
        b: CompareSide,
        mode: CompareMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        self.discard_file_history();
        self.compare = Some(Compare::new(a, b, mode));
        self.start_compare(cx);
        self.focus_root(window, cx);
        self.notify_if_active(cx);
    }

    /// "Compare these commits": the older tip on the left.
    pub(crate) fn open_compare_pair(
        &mut self,
        a: Oid,
        b: Oid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_compare(
            CompareSide::commit(a),
            CompareSide::commit(b),
            CompareMode::TwoDot,
            window,
            cx,
        );
    }

    /// "Open in Compare" for a selection range: `base` is the left tree, or
    /// `None` when the range starts at a root commit's empty tree.
    pub(crate) fn open_compare_range(
        &mut self,
        base: Option<Oid>,
        head: Oid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let left = match base {
            Some(oid) => CompareSide::commit(oid),
            None => CompareSide::empty_tree(),
        };
        self.open_compare(
            left,
            CompareSide::commit(head),
            CompareMode::TwoDot,
            window,
            cx,
        );
    }

    /// "Compare with HEAD": the commit on the left, HEAD on the right.
    pub(crate) fn open_compare_with_head(
        &mut self,
        commit: Oid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(head) = self.compare_head() else {
            return;
        };
        let label = self.compare_head_label();
        self.open_compare(
            CompareSide::commit(commit),
            CompareSide::branch(label, head),
            CompareMode::TwoDot,
            window,
            cx,
        );
    }

    /// "Compare with working tree": the commit on the left, the files on disk
    /// on the right.
    pub(crate) fn open_compare_with_worktree(
        &mut self,
        commit: Oid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_compare(
            CompareSide::commit(commit),
            CompareSide::worktree(),
            CompareMode::TwoDot,
            window,
            cx,
        );
    }

    /// "Compare with current branch": the branch on the left, HEAD on the
    /// right, three-dot by default so the list is what the branch adds.
    pub(crate) fn open_compare_with_current_branch(
        &mut self,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(tip), Some(head)) = (self.compare_branch_target(branch), self.compare_head())
        else {
            return;
        };
        let label = self.compare_head_label();
        self.open_compare(
            CompareSide::branch(branch, tip),
            CompareSide::branch(label, head),
            CompareMode::ThreeDot,
            window,
            cx,
        );
    }

    /// The commit HEAD points at, or `None` on an unborn branch.
    pub(crate) fn compare_head(&self) -> Option<Oid> {
        let snapshot = self.session.as_ref()?.snapshot();
        match snapshot.head.as_ref()? {
            HeadInfo::Branch { target, .. } => *target,
            HeadInfo::Detached { target } => Some(*target),
        }
    }

    /// What the header calls HEAD: the branch it is on, or `HEAD` itself.
    fn compare_head_label(&self) -> String {
        let Some(session) = self.session.as_ref() else {
            return "HEAD".to_owned();
        };
        let snapshot = session.snapshot();
        match snapshot.head.as_ref() {
            Some(HeadInfo::Branch { short_name, .. }) => short_name.clone(),
            _ => "HEAD".to_owned(),
        }
    }

    /// The commit a local branch points at, from the published refs.
    fn compare_branch_target(&self, branch: &str) -> Option<Oid> {
        let snapshot = self.session.as_ref()?.snapshot();
        snapshot
            .refs
            .iter()
            .find(|info| {
                info.kind == strata_core::RefKind::LocalBranch && info.short_name == branch
            })
            .map(|info| info.target)
    }

    /// Requests the comparison. Anything the view was waiting for is cancelled
    /// first, and the answer on screen is dropped so a stale pair never shows.
    fn start_compare(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(compare) = self.compare.as_mut() else {
            return;
        };
        let previous_query = compare.ticket.take();
        let previous_diff = compare.diff_ticket.take();
        if let Some(ticket) = previous_query {
            session.cancel_query(ticket);
        }
        if let Some(ticket) = previous_diff {
            session.cancel_diff(ticket);
        }
        let ticket = if compare.b.worktree {
            compare
                .a
                .oid
                .map(|base| session.request_compare_with_worktree(base))
        } else {
            match (compare.a.oid, compare.b.oid) {
                (Some(a), Some(b)) => Some(session.request_compare(a, b, compare.mode)),
                (None, Some(b)) => Some(session.request_compare_from(None, b)),
                // An empty right side never comes from a menu; the three-dot
                // mode has nothing to resolve either.
                (_, None) => None,
            }
        };
        let Some(ticket) = ticket else {
            compare.ticket = None;
            compare.comparison = None;
            compare.files = Rc::new(Vec::new());
            compare.error = None;
            compare.selected = None;
            compare.view = None;
            compare.diff_rows = Rc::new(Vec::new());
            compare.diff_error = None;
            self.notify_if_active(cx);
            return;
        };
        compare.ticket = Some(ticket);
        compare.comparison = None;
        compare.files = Rc::new(Vec::new());
        compare.error = None;
        compare.selected = None;
        compare.view = None;
        compare.diff_rows = Rc::new(Vec::new());
        compare.diff_error = None;
        self.notify_if_active(cx);
    }

    /// Closes the compare view and returns to the history split.
    pub(crate) fn close_compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compare.is_none() {
            return;
        }
        self.cancel_compare();
        self.compare = None;
        self.focus_root(window, cx);
        self.notify_if_active(cx);
    }

    /// Drops the view and its tickets when another main view takes over.
    pub(crate) fn discard_compare(&mut self) {
        if self.compare.is_some() {
            self.cancel_compare();
            self.compare = None;
        }
    }

    fn cancel_compare(&mut self) {
        let Some(compare) = self.compare.as_mut() else {
            return;
        };
        let query = compare.ticket.take();
        let diff = compare.diff_ticket.take();
        if let Some(session) = self.session.as_ref() {
            if let Some(ticket) = query {
                session.cancel_query(ticket);
            }
            if let Some(ticket) = diff {
                session.cancel_diff(ticket);
            }
        }
    }

    pub(crate) fn set_compare_mode(&mut self, mode: CompareMode, cx: &mut Context<Self>) {
        let Some(compare) = self.compare.as_mut() else {
            return;
        };
        if compare.mode == mode {
            return;
        }
        compare.mode = mode;
        self.start_compare(cx);
    }

    /// Swaps the two sides, which turns `A..B` into `B..A`.
    pub(crate) fn swap_compare(&mut self, cx: &mut Context<Self>) {
        let Some(compare) = self.compare.as_mut() else {
            return;
        };
        std::mem::swap(&mut compare.a, &mut compare.b);
        self.start_compare(cx);
    }

    /// The comparison answered, for `ticket` only.
    pub(crate) fn compare_ready(
        &mut self,
        ticket: QueryTicket,
        comparison: &Arc<Comparison>,
        cx: &mut Context<Self>,
    ) -> bool {
        let selected = {
            let Some(compare) = self.compare.as_mut() else {
                return false;
            };
            if compare.ticket != Some(ticket) {
                return false;
            }
            compare.ticket = None;
            compare.comparison = Some(Arc::clone(comparison));
            compare.files = Rc::new(comparison.files.clone());
            compare.error = None;
            compare.selected = (!comparison.files.is_empty()).then_some(0);
            compare.view = None;
            compare.diff_rows = Rc::new(Vec::new());
            compare.context.reset();
            compare.diff_error = None;
            compare.selected
        };
        if selected.is_some() {
            self.request_compare_diff(cx);
        }
        self.notify_if_active(cx);
        true
    }

    pub(crate) fn compare_failed(
        &mut self,
        ticket: QueryTicket,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(compare) = self.compare.as_mut() else {
            return false;
        };
        if compare.ticket != Some(ticket) {
            return false;
        }
        compare.ticket = None;
        compare.comparison = None;
        compare.files = Rc::new(Vec::new());
        compare.error = Some(error.to_owned());
        self.notify_if_active(cx);
        true
    }

    pub(crate) fn compare_diff_ready(
        &mut self,
        ticket: DiffTicket,
        view: &Arc<DiffView>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(compare) = self.compare.as_mut() else {
            return false;
        };
        if compare.diff_ticket != Some(ticket) {
            return false;
        }
        compare.context.reset();
        compare.diff_rows = Rc::new(compare.context.rows(view));
        compare.view = Some(Arc::clone(view));
        compare.diff_error = None;
        self.notify_if_active(cx);
        true
    }

    pub(crate) fn compare_diff_failed(
        &mut self,
        ticket: DiffTicket,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(compare) = self.compare.as_mut() else {
            return false;
        };
        if compare.diff_ticket != Some(ticket) {
            return false;
        }
        compare.diff_ticket = None;
        compare.view = None;
        compare.diff_rows = Rc::new(Vec::new());
        compare.diff_error = Some(error.to_owned());
        self.notify_if_active(cx);
        true
    }

    fn select_compare_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(compare) = self.compare.as_mut() else {
            return;
        };
        if index >= compare.files().len() {
            return;
        }
        if compare.selected == Some(index) && compare.view.is_some() {
            return;
        }
        compare.selected = Some(index);
        compare.view = None;
        compare.diff_rows = Rc::new(Vec::new());
        compare.context.reset();
        compare.diff_error = None;
        self.request_compare_diff(cx);
    }

    /// Applies one press on a hidden-lines bar of the compare diff.
    pub(crate) fn expand_compare_gap(
        &mut self,
        gap: usize,
        action: strata_core::diff::GapAction,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .compare
            .as_ref()
            .and_then(|compare| compare.view.clone())
        else {
            return;
        };
        let Some(context) = self.compare.as_mut().and_then(|compare| {
            compare
                .context
                .expand(gap, action, &view)
                .then(|| compare.context.clone())
        }) else {
            return;
        };
        let revision = context.revision();
        let build_view = Arc::clone(&view);
        self.rebuild_rows_in_background(RowsTarget::Compare, revision, cx, move || {
            context.rows(&build_view)
        });
        self.notify_if_active(cx);
    }

    /// The compare view's t!("history.whole_file").to_string() toggle.
    pub(crate) fn toggle_compare_whole_file(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self
            .compare
            .as_ref()
            .and_then(|compare| compare.view.clone())
        else {
            return;
        };
        let Some(context) = self.compare.as_mut().and_then(|compare| {
            compare
                .context
                .toggle_whole_file(&view)
                .then(|| compare.context.clone())
        }) else {
            // A refused toggle still has a notice to show.
            self.notify_if_active(cx);
            return;
        };
        let revision = context.revision();
        let build_view = Arc::clone(&view);
        self.rebuild_rows_in_background(RowsTarget::Compare, revision, cx, move || {
            context.rows(&build_view)
        });
        self.notify_if_active(cx);
    }

    /// Up/Down in the compare view: moves between the changed files.
    pub(crate) fn step_compare_file(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(compare) = self.compare.as_ref() else {
            return;
        };
        let count = compare.files().len();
        if count == 0 {
            return;
        }
        let position = compare.selected.map_or(0, |index| {
            (index as isize + delta).clamp(0, count as isize - 1) as usize
        });
        compare
            .file_scroll
            .scroll_to_item(position, gpui::ScrollStrategy::Center);
        self.select_compare_file(position, cx);
    }

    /// Asks for the selected file's diff between the two trees, with the
    /// settings' comparison options.
    pub(super) fn request_compare_diff(&mut self, cx: &mut Context<Self>) {
        let options = history_diff_options(cx);
        let (Some(session), Some(compare)) = (self.session.as_ref(), self.compare.as_mut()) else {
            return;
        };
        let Some(comparison) = compare.comparison.clone() else {
            return;
        };
        let Some(file) = compare.selected_file() else {
            return;
        };
        let request = if compare.b.worktree {
            let Some(base) = compare.a.oid else {
                return;
            };
            DiffRequest::CompareWorktree {
                base,
                path: file.stat.path.clone(),
            }
        } else {
            DiffRequest::Compare {
                base: comparison.base,
                head: comparison.head,
                old_path: file.stat.old_path.clone(),
                path: file.stat.path.clone(),
            }
        };
        let ticket = session.request_diff_with(request, options);
        if let Some(previous) = compare.diff_ticket.replace(ticket) {
            session.cancel_diff(previous);
        }
    }

    /// The whole main area while a comparison is open.
    pub(crate) fn compare_view(&self, entity: Entity<Self>, cx: &mut Context<Self>) -> AnyElement {
        let Some(compare) = self.compare.as_ref() else {
            return div().into_any_element();
        };
        let design = Design::new(cx.theme(), crate::settings::Settings::get(cx));
        let diff_colors = DiffColors::from_design(&design);

        let header = self.compare_header(&compare.a, &compare.b, compare.mode, entity.clone(), cx);
        let list = self.compare_file_list(entity.clone(), &design);
        let diff = self.compare_diff_pane(&design, &diff_colors, cx);

        // The file column keeps its saved share of the details area; the
        // splitter only rewrites that share, exactly like the History pane.
        let mut left = div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .min_h_0()
            .min_w_0()
            .border_r_1()
            .border_color(design.border)
            .bg(design.panel);
        left = match self.details_column_width(cx) {
            Some(width) => left.w(px(width)),
            None => left.flex_basis(relative(files_fraction(cx))),
        };
        left = left.child(list);

        let drag_entity = entity.clone();
        let reset_entity = entity.clone();
        let extent = self.splits.details.clone();
        left = left.child(split_handle(
            "compare-split-handle",
            Axis::Horizontal,
            move |event, _window, cx| {
                let Some(area) = extent.get() else {
                    return;
                };
                let fraction = area.fraction_at(event.event.position.x);
                drag_entity.update(cx, |this, cx| {
                    this.set_files_fraction(fraction, area.size(), cx);
                });
            },
            move |_event, _window, cx| {
                reset_entity.update(cx, |this, cx| this.reset_files_fraction(cx));
            },
        ));

        let measured_cell = self.splits.details.clone();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(design.background)
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .on_children_prepainted(move |bounds: Vec<gpui::Bounds<gpui::Pixels>>, _, _| {
                        if let Some(area) = split::measure(&bounds, Axis::Horizontal) {
                            measured_cell.set(Some(area));
                        }
                    })
                    .child(left)
                    .child(diff),
            )
            .into_any_element()
    }

    /// "A ⇄ B", the two-dot/three-dot toggle, the ahead/behind counts and the
    /// ways out of the view.
    fn compare_header(
        &self,
        a: &CompareSide,
        b: &CompareSide,
        mode: CompareMode,
        entity: Entity<Self>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let design = Design::new(cx.theme(), crate::settings::Settings::get(cx));

        let close = {
            let entity = entity.clone();
            ui::icon_button("compare-close", IconName::Close)
                .size(ButtonSize::Tiny)
                .ghost()
                .tooltip(t!("compare.close_tooltip").to_string())
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.close_compare(window, cx));
                })
        };
        let swap = if a.oid.is_some() && b.oid.is_some() && !a.worktree && !b.worktree {
            let entity = entity.clone();
            ui::icon_button("compare-swap", IconName::Replace)
                .size(ButtonSize::Tiny)
                .ghost()
                .tooltip(t!("compare.swap").to_string())
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |this, cx| this.swap_compare(cx));
                })
                .into_any_element()
        } else {
            // Swapping an empty tree would put it on the right, where the
            // comparison has no tree to diff against.
            div().into_any_element()
        };
        let toggle = if a.oid.is_none() || b.worktree {
            div().into_any_element()
        } else {
            let two_entity = entity.clone();
            let two_dot = ui::button("compare-two-dot", "A..B")
                .size(ButtonSize::Tiny)
                .ghost()
                .selected(mode == CompareMode::TwoDot)
                .tooltip(t!("compare.two_dot_tooltip").to_string())
                .on_click(move |_, _window, cx| {
                    two_entity.update(cx, |this, cx| {
                        this.set_compare_mode(CompareMode::TwoDot, cx)
                    });
                });
            let three_entity = entity;
            let three_dot = ui::button("compare-three-dot", "A...B")
                .size(ButtonSize::Tiny)
                .ghost()
                .selected(mode == CompareMode::ThreeDot)
                .tooltip(t!("compare.three_dot_tooltip").to_string())
                .on_click(move |_, _window, cx| {
                    three_entity.update(cx, |this, cx| {
                        this.set_compare_mode(CompareMode::ThreeDot, cx)
                    });
                });
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .child(two_dot)
                .child(three_dot)
                .into_any_element()
        };

        let side = |side: &CompareSide| {
            let mut block = div().flex().items_center().gap_1().min_w_0().child(
                div()
                    .flex_none()
                    .max_w(design.px(180.0))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(side.title()),
            );
            if side.label.is_some()
                && let Some(oid) = side.oid
            {
                block = block.child(
                    div()
                        .flex_none()
                        .font_family(design.mono.clone())
                        .text_size(design.px(10.5))
                        .text_color(design.muted)
                        .child(data::short_oid(oid)),
                );
            }
            block.into_any_element()
        };

        let counts = self.compare_counts(&design);

        div()
            .flex()
            .items_center()
            .gap_2()
            .h(design.px(40.0))
            .flex_none()
            .px_3()
            .bg(design.chrome)
            .border_b_1()
            .border_color(design.border)
            .child(close)
            .child(
                div()
                    .flex_none()
                    .text_size(design.px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t!("compare.title").to_string()),
            )
            .child(side(a))
            .child(div().flex_none().text_color(design.muted).child("⇄"))
            .child(side(b))
            .child(swap)
            .child(toggle)
            .child(div().flex_1())
            .children(counts)
            .into_any_element()
    }

    /// "3 ahead · 2 behind", or nothing while the comparison is in flight.
    fn compare_counts(&self, design: &Design) -> Option<AnyElement> {
        let compare = self.compare.as_ref()?;
        // The empty tree and the working tree have no history to count.
        if compare.b.worktree {
            return None;
        }
        compare.a.oid?;
        let comparison = compare.comparison.as_ref()?;
        let mut row = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .text_size(design.px(11.0))
            .text_color(design.muted);
        if comparison.ahead > 0 {
            row = row.child(
                div()
                    .text_color(design.add_text)
                    .child(t!("compare.ahead", count = comparison.ahead).to_string()),
            );
        }
        if comparison.behind > 0 {
            row = row.child(
                div()
                    .text_color(design.remove_text)
                    .child(t!("compare.behind", count = comparison.behind).to_string()),
            );
        }
        if comparison.ahead == 0 && comparison.behind == 0 {
            row = row.child(t!("compare.same_history").to_string());
        }
        Some(row.into_any_element())
    }

    fn compare_file_list(&self, entity: Entity<Self>, design: &Design) -> AnyElement {
        let Some(compare) = self.compare.as_ref() else {
            return div().into_any_element();
        };
        if let Some(error) = &compare.error {
            return ui::message(error, design.remove_text, design).into_any_element();
        }
        let Some(comparison) = compare.comparison.as_ref() else {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_size(design.px(11.0))
                .text_color(design.muted)
                .child(t!("compare.comparing").to_string())
                .into_any_element();
        };
        if comparison.files.is_empty() {
            return ui::message(
                t!("compare.no_changed_files").to_string(),
                design.muted,
                design,
            )
            .into_any_element();
        }

        let files = Rc::clone(&compare.files);
        let selected = compare.selected;
        let list_design = design.clone();
        let count = files.len();
        uniform_list("compare-files", count, move |range, _window, _cx| {
            range
                .map(|index| {
                    let entity = entity.clone();
                    let file = &files[index];
                    let (name, dir) = style::path_parts(&file.stat.path);
                    let row = ui::list_row(
                        ("compare-file", index),
                        ui::RowState::NORMAL.selected(selected == Some(index)),
                        &list_design,
                    )
                    .gap_2()
                    .px_3()
                    .child(ui::change_badge(file.kind, &list_design))
                    .child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
                    .when_some(dir, |row, dir| {
                        row.child(
                            div()
                                .max_w(list_design.px(110.0))
                                .flex_none()
                                .truncate()
                                .text_size(list_design.px(11.0))
                                .text_color(list_design.muted)
                                .child(dir.to_owned()),
                        )
                    })
                    .child(file_stat_label(&file.stat, &list_design));
                    row.cursor_pointer()
                        .on_click(move |_event, _window, cx| {
                            entity.update(cx, |this, cx| this.select_compare_file(index, cx));
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&compare.file_scroll)
        .with_scrollbar(&compare.file_scroll)
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn compare_diff_pane(
        &self,
        design: &Design,
        diff_colors: &DiffColors,
        cx: &mut App,
    ) -> AnyElement {
        let Some(compare) = self.compare.as_ref() else {
            return div().into_any_element();
        };
        let path = compare.selected_file().map_or_else(
            || t!("compare.no_file_selected").to_string().to_owned(),
            |file| file.stat.path.clone(),
        );
        let totals = compare
            .comparison
            .as_ref()
            .map_or_else(String::new, |comparison| {
                format!("+{} −{}", comparison.added, comparison.removed)
            });
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .h(design.px(36.0))
            .flex_none()
            .px_3p5()
            .bg(design.chrome)
            .border_b_1()
            .border_color(design.border)
            .text_size(design.px(11.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(design.mono.clone())
                    .child(path),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_none().text_color(design.muted).child(totals))
                    .child(if compare.view.is_some() {
                        let active = compare.context.is_whole_file();
                        let entity = self.entity();
                        ui::button(
                            "compare-toggle-whole-file",
                            t!("history.whole_file").to_string(),
                        )
                        .size(ButtonSize::Tiny)
                        .ghost()
                        .selected(active)
                        .tooltip(if active {
                            t!("worktree.show_only_changes").to_string()
                        } else {
                            t!("worktree.show_every_line").to_string()
                        })
                        .on_click(move |_event, _window, cx| {
                            entity.update(cx, |this, cx| this.toggle_compare_whole_file(cx));
                        })
                        .into_any_element()
                    } else {
                        div().into_any_element()
                    })
                    .child(
                        compare
                            .view
                            .as_ref()
                            .and_then(|view| {
                                crate::image_diff::svg_toggle(&self.image_diff, view, self.entity())
                            })
                            .unwrap_or_else(|| div().into_any_element()),
                    ),
            );

        let body = if let Some(error) = &compare.diff_error {
            ui::message(error, design.remove_text, design).into_any_element()
        } else if compare.selected.is_none() {
            ui::message(t!("compare.select_file").to_string(), design.muted, design)
                .into_any_element()
        } else if let Some(view) = &compare.view {
            if let Some(image) =
                crate::image_diff::body(&self.image_diff, view, design, self.entity(), cx)
            {
                image
            } else if view.diff.binary {
                ui::message(t!("worktree.binary_file").to_string(), design.muted, design)
                    .into_any_element()
            } else if compare.diff_rows.is_empty() {
                ui::message(
                    t!("worktree.no_text_changes").to_string(),
                    design.muted,
                    design,
                )
                .into_any_element()
            } else {
                let list = diff_view::diff_list(
                    diff_view::DiffList {
                        id: "compare-diff",
                        rows: Rc::clone(&compare.diff_rows),
                        view: compare.view.clone(),
                        colors: *diff_colors,
                        mono: design.mono.clone(),
                        scroll: &compare.diff_scroll,
                        on_gap: self.gap_handler(),
                    },
                    cx,
                );
                match compare.context.notice() {
                    Some(notice) => div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(ui::banner(ui::BannerKind::Warning, notice, design))
                        .child(list)
                        .into_any_element(),
                    None => list,
                }
            }
        } else {
            ui::message(
                t!("worktree.computing_diff").to_string(),
                design.muted,
                design,
            )
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::bind;
    use crate::repo_view::Launch;
    use crate::settings::Settings;
    use gpui::{QuitMode, TestAppContext, VisualTestContext};

    fn oid(byte: u8) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        Oid::from(bytes)
    }

    /// Escape leaves the view with the real keymap: search binds Escape later in
    /// the same context, so its dismiss handler is what closes the view.
    #[gpui::test]
    fn escape_closes_the_compare_view(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            bind(cx);
            crate::search::bind(cx);
        });
        let window = cx.add_window(|window, cx| RepoView::new(Launch::fake(8), window, cx));
        let view = window.root(cx).expect("the window's root view");
        view.update(cx, |view, _| {
            view.compare = Some(Compare::new(
                CompareSide::commit(oid(1)),
                CompareSide::commit(oid(2)),
                CompareMode::TwoDot,
            ));
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus_root(window, cx));
        });
        cx.simulate_keystrokes("escape");
        assert!(
            view.read_with(&cx, |view, _| view.compare.is_none()),
            "escape must close the compare view"
        );
    }

    #[test]
    fn two_commits_must_be_selected() {
        assert_eq!(
            two_commit_blocked(false, &[oid(1), oid(2)]),
            Some(t!("compare.no_repository").to_string())
        );
        assert_eq!(
            two_commit_blocked(true, &[]),
            Some(t!("compare.select_two").to_string())
        );
        assert_eq!(
            two_commit_blocked(true, &[oid(1)]),
            Some(t!("compare.select_two").to_string())
        );
        assert_eq!(two_commit_blocked(true, &[oid(1), oid(2)]), None);
        assert_eq!(
            two_commit_blocked(true, &[oid(1), oid(2), oid(3)]),
            Some(t!("compare.select_two").to_string())
        );
    }

    #[test]
    fn compare_with_head_needs_both_commits() {
        assert_eq!(
            with_head_blocked(false, Some(oid(1)), Some(oid(2))),
            Some(t!("compare.no_repository").to_string())
        );
        assert_eq!(
            with_head_blocked(true, None, Some(oid(2))),
            Some(t!("compare.no_commit").to_string())
        );
        assert_eq!(
            with_head_blocked(true, Some(oid(1)), None),
            Some(t!("compare.no_head").to_string()),
            "an unborn branch has nothing to compare against"
        );
        assert_eq!(with_head_blocked(true, Some(oid(1)), Some(oid(2))), None);
    }

    #[test]
    fn compare_with_worktree_needs_a_repository_and_a_commit() {
        assert_eq!(
            with_worktree_blocked(false, Some(oid(1))),
            Some(t!("compare.no_repository").to_string())
        );
        assert_eq!(
            with_worktree_blocked(true, None),
            Some(t!("compare.no_commit").to_string())
        );
        assert_eq!(with_worktree_blocked(true, Some(oid(1))), None);
    }

    #[test]
    fn comparing_the_current_branch_names_its_reason() {
        assert_eq!(
            with_current_branch_blocked(true, true, false, Some(oid(1))),
            Some(t!("compare.current_branch").to_string())
        );
        assert_eq!(
            with_current_branch_blocked(true, false, true, Some(oid(1))),
            Some(t!("compare.busy").to_string())
        );
        assert_eq!(
            with_current_branch_blocked(true, false, false, None),
            Some(t!("compare.no_head").to_string())
        );
        assert_eq!(
            with_current_branch_blocked(true, false, false, Some(oid(1))),
            None
        );
    }
}
