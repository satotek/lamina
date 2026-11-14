//! Merge conflict resolution: the conflicts list and the per-file merge editor.
//!
//! The session is the only route to git. The three sides of a conflicted path
//! come from `Repo::conflict_sides` (a gix read, run on the background
//! executor) and are merged into regions by [`strata_core::merge`]; every write
//! is an [`Operation`] and nothing here spawns git.
//!
//! The editor shows ours and theirs side by side over the editable result.
//! Both side panes are one virtualised list whose rows carry a line of each
//! side, so they cannot drift out of sync, and a file above
//! [`MAX_EDITOR_BYTES`] keeps the whole-file choices and the external merge
//! tool instead of building a text area that large.

use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Hsla, InteractiveElement, IntoElement,
    ParentElement, ScrollStrategy, StatefulInteractiveElement, Styled, Subscription,
    UniformListScrollHandle, Window, actions, div, px, relative, uniform_list,
};
use gpui_component::ActiveTheme as _;
use gpui_component::Theme;
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use strata_core::highlight::{Highlighter, LineHighlights, language_for_path};
use strata_core::merge::{ConflictStyle, ContentHash, MergeAnalysis, SegmentKind};
use strata_core::session::{ConflictSide, Operation, OperationId, RepoSnapshot};
use strata_core::{ChangeKind, ConflictEntry, ConflictKind, ConflictSides, RepoState};

use crate::diff_view::{self, DiffColors, DiffStyle};
use crate::merge_editor::{Choice as MergeChoice, MergeModel, MergeRow, Resolution};
use crate::repo_view::RepoView;
use crate::scrollbar::WithScrollbar as _;
use crate::settings::Settings;
use crate::style::{self, Design};

actions!(
    strata_conflicts,
    [
        UseOurs,
        UseTheirs,
        MarkFileResolved,
        NextConflict,
        PreviousConflict,
        ResetConflict,
        TakeOurs,
        TakeTheirs,
        TakeBothOursFirst,
        TakeBothTheirsFirst,
        TakeBase,
    ]
);

pub fn bind(cx: &mut App) {
    crate::keymap::bind_group(cx, crate::keymap::Group::Conflicts);
}

/// Height of one conflicts-list row, matching the other file lists.
const ROW_HEIGHT: f32 = 20.0;
/// A conflicts list stops growing here and scrolls instead.
const MAX_LIST_ROWS: usize = 8;
/// Files larger than this keep the whole-file choices and the merge tool: a
/// text area with millions of characters cannot stay responsive, and the
/// colouring work would be paid on every load.
const MAX_EDITOR_BYTES: usize = 1024 * 1024;
/// A conflict this fragmented is better resolved in the merge tool.
const MAX_EDITOR_CONFLICTS: usize = 200;
/// Same probe length core's binary detector uses.
#[cfg(test)]
const BINARY_PROBE: usize = 8000;
/// Sides larger than this are not highlighted; the merge tool is the better
/// place for a file that big, and parsing it would only slow the load down.
const MAX_HIGHLIGHT_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested at the bottom).
// ---------------------------------------------------------------------------

/// The kind in the words `git status` prints next to an unmerged path.
pub(crate) fn kind_label(kind: ConflictKind) -> String {
    match kind {
        ConflictKind::BothModified => t!("conflict.kind.both_modified").to_string(),
        ConflictKind::BothAdded => t!("conflict.kind.both_added").to_string(),
        ConflictKind::BothDeleted => t!("conflict.kind.both_deleted").to_string(),
        ConflictKind::AddedByUs => t!("conflict.kind.added_by_us").to_string(),
        ConflictKind::AddedByThem => t!("conflict.kind.added_by_them").to_string(),
        ConflictKind::DeletedByUs => t!("conflict.kind.deleted_by_us").to_string(),
        ConflictKind::DeletedByThem => t!("conflict.kind.deleted_by_them").to_string(),
    }
}

/// One whole-file choice the resolution offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileChoice {
    pub label: String,
    pub side: ConflictSide,
    /// The choice removes the path, so it is painted as a destructive action.
    pub destructive: bool,
}

/// The whole-file choices a conflict kind allows.
///
/// A side that deleted the path has no version to check out; core resolves that
/// by removing the path from the index, which is exactly "delete file". So every
/// kind except t!("conflict.kind.both_deleted").to_string() (which has no sides at all) offers two choices.
pub(crate) fn choices(kind: ConflictKind) -> Vec<FileChoice> {
    let ours = FileChoice {
        label: t!("conflict.use_ours").to_string(),
        side: ConflictSide::Ours,
        destructive: false,
    };
    let theirs = FileChoice {
        label: t!("conflict.use_theirs").to_string(),
        side: ConflictSide::Theirs,
        destructive: false,
    };
    let keep_ours = FileChoice {
        label: t!("conflict.keep_file").to_string(),
        side: ConflictSide::Ours,
        destructive: false,
    };
    let keep_theirs = FileChoice {
        label: t!("conflict.keep_file").to_string(),
        side: ConflictSide::Theirs,
        destructive: false,
    };
    let delete_theirs = FileChoice {
        label: t!("conflict.delete_file").to_string(),
        side: ConflictSide::Theirs,
        destructive: true,
    };
    let delete_ours = FileChoice {
        label: t!("conflict.delete_file").to_string(),
        side: ConflictSide::Ours,
        destructive: true,
    };

    match kind {
        ConflictKind::BothModified | ConflictKind::BothAdded => vec![ours, theirs],
        // The path exists only on our side.
        ConflictKind::DeletedByThem | ConflictKind::AddedByUs => vec![keep_ours, delete_theirs],
        // The path exists only on their side.
        ConflictKind::DeletedByUs | ConflictKind::AddedByThem => vec![keep_theirs, delete_ours],
        ConflictKind::BothDeleted => Vec::new(),
    }
}

/// Whether the merge editor can resolve this kind of conflict.
///
/// A delete/modify conflict resolves by removing or keeping the path, which the
/// whole-file choices already do; writing an empty result there would create an
/// empty file instead of staging the deletion.
fn kind_has_editor(kind: ConflictKind) -> bool {
    matches!(kind, ConflictKind::BothModified | ConflictKind::BothAdded)
}

/// The kind implied by which index stages exist, used when the snapshot entry
/// for the path is already gone by the time the sides are read.
fn kind_from_presence(base: bool, ours: bool, theirs: bool) -> ConflictKind {
    match (base, ours, theirs) {
        (true, true, true) => ConflictKind::BothModified,
        (false, true, true) => ConflictKind::BothAdded,
        (true, false, false) => ConflictKind::BothDeleted,
        (false, true, false) => ConflictKind::AddedByUs,
        (false, false, true) => ConflictKind::AddedByThem,
        (true, false, true) => ConflictKind::DeletedByUs,
        (true, true, false) => ConflictKind::DeletedByThem,
        // Submodule and sparse entries carry no content, so the sides cannot
        // describe the kind; a content conflict keeps the path usable.
        (false, false, false) => ConflictKind::BothModified,
    }
}

/// Git calls a file binary when a NUL appears early. Only the tests need this
/// now that the merge analysis decides from decoded text.
#[cfg(test)]
fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_PROBE)].contains(&0)
}

/// Whether a worktree file still holds git's conflict markers. Only column-zero
/// markers count, as in git's own scan; a line of `=======` underlines is
/// therefore reported too, which errs on the side of asking.
pub(crate) fn has_conflict_markers(bytes: &[u8]) -> bool {
    bytes.split(|byte| *byte == b'\n').any(|line| {
        line.starts_with(b"<<<<<<<")
            || line.starts_with(b"|||||||")
            || line.starts_with(b"=======")
            || line.starts_with(b">>>>>>>")
    })
}

/// `812 B`, `1.4 KB`, `3.1 MB`.
fn format_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{:.1} MB", value / (KB * KB))
    }
}

fn sizes_line(data: &ConflictData) -> String {
    let side = |present: bool, bytes: usize| {
        if present {
            format_size(bytes)
        } else {
            t!("conflict.deleted").to_string().to_owned()
        }
    };
    format!(
        "ours {} · theirs {} · base {}",
        side(data.ours_present, data.ours_bytes),
        side(data.theirs_present, data.theirs_bytes),
        side(data.base_present, data.base_bytes)
    )
}

// ---------------------------------------------------------------------------
// Loaded conflict data.
// ---------------------------------------------------------------------------

/// Everything the conflict view shows for one path, computed off the UI thread.
pub(crate) struct ConflictData {
    pub(crate) kind: ConflictKind,
    pub(crate) ours_present: bool,
    pub(crate) theirs_present: bool,
    pub(crate) base_present: bool,
    pub(crate) ours_bytes: usize,
    pub(crate) theirs_bytes: usize,
    pub(crate) base_bytes: usize,
    pub(crate) worktree_markers: bool,
    /// Fingerprint of the worktree file the editor loaded; the resolve
    /// operation refuses to overwrite a file that no longer matches.
    pub(crate) worktree_hash: Option<ContentHash>,
    pub(crate) binary: bool,
    /// Every present stage is UTF-8 text.
    pub(crate) text: bool,
    /// The file is past the editor's size or conflict cap.
    pub(crate) too_large: bool,
    /// Visual rows of the two side panes.
    pub(crate) rows: Arc<Vec<MergeRow>>,
    /// First visual row of each conflict, for scrolling to it.
    pub(crate) conflict_rows: Vec<usize>,
    ours_lines: Arc<Vec<String>>,
    theirs_lines: Arc<Vec<String>>,
    ours_highlights: Arc<Vec<LineHighlights>>,
    theirs_highlights: Arc<Vec<LineHighlights>>,
    /// Moved into the editor once the input exists; `None` when the editor is
    /// not offered.
    model: Option<MergeModel>,
}

impl ConflictData {
    fn new(
        sides: ConflictSides,
        worktree: Option<&[u8]>,
        path: &str,
        kind: Option<ConflictKind>,
        style: ConflictStyle,
    ) -> Self {
        let mut analysis = MergeAnalysis::analyze(&sides, style);
        let binary = analysis.binary;
        let text = analysis.text;
        let kind = kind.unwrap_or_else(|| {
            kind_from_presence(
                sides.base.is_some(),
                sides.ours.is_some(),
                sides.theirs.is_some(),
            )
        });
        let conflicts = analysis
            .segments
            .iter()
            .filter(|segment| segment.kind == SegmentKind::Conflict)
            .count();
        let longest = [
            sides.base.as_ref().map_or(0, Vec::len),
            sides.ours.as_ref().map_or(0, Vec::len),
            sides.theirs.as_ref().map_or(0, Vec::len),
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        let too_large = text && (longest > MAX_EDITOR_BYTES || conflicts > MAX_EDITOR_CONFLICTS);
        let editable = text && !too_large && kind_has_editor(kind) && !analysis.segments.is_empty();

        let (rows, conflict_rows, model) = if editable {
            let (rows, conflict_rows) = MergeModel::rows(&analysis);
            (
                Arc::new(rows),
                conflict_rows,
                Some(MergeModel::new(&analysis)),
            )
        } else {
            (Arc::new(Vec::new()), Vec::new(), None)
        };
        let (ours_highlights, theirs_highlights) = if text {
            highlight_sides(
                path,
                (&analysis.ours.lines, analysis.ours.trailing_newline),
                (&analysis.theirs.lines, analysis.theirs.trailing_newline),
            )
        } else {
            Default::default()
        };

        let ours_lines = Arc::new(std::mem::take(&mut analysis.ours.lines));
        let theirs_lines = Arc::new(std::mem::take(&mut analysis.theirs.lines));

        Self {
            kind,
            ours_present: sides.ours.is_some(),
            theirs_present: sides.theirs.is_some(),
            base_present: sides.base.is_some(),
            ours_bytes: sides.ours.as_ref().map_or(0, Vec::len),
            theirs_bytes: sides.theirs.as_ref().map_or(0, Vec::len),
            base_bytes: sides.base.as_ref().map_or(0, Vec::len),
            worktree_markers: worktree.is_some_and(has_conflict_markers),
            worktree_hash: worktree.map(ContentHash::of),
            binary,
            text,
            too_large,
            rows,
            conflict_rows,
            ours_lines,
            theirs_lines,
            ours_highlights: Arc::new(ours_highlights),
            theirs_highlights: Arc::new(theirs_highlights),
            model,
        }
    }
}

/// The syntax spans of ours and theirs, indexed by line number minus one.
///
/// Highlighting the whole files rather than the conflict fragments keeps a
/// construct that starts outside a region (a doc comment, a raw string)
/// coloured the way it is in the file; [`MergeRow`] then finds each line's
/// spans by the line number it kept from the file.
fn highlight_sides(
    path: &str,
    ours: (&[String], bool),
    theirs: (&[String], bool),
) -> (Vec<LineHighlights>, Vec<LineHighlights>) {
    let Some(language) = language_for_path(path) else {
        return Default::default();
    };
    let mut highlighter = Highlighter::new();
    let mut spans: (Vec<LineHighlights>, Vec<LineHighlights>) = Default::default();
    for (spans, (lines, trailing)) in [(&mut spans.0, ours), (&mut spans.1, theirs)] {
        let mut text = lines.join("\n");
        if trailing && !lines.is_empty() {
            text.push('\n');
        }
        if text.len() > MAX_HIGHLIGHT_BYTES {
            continue;
        }
        *spans = highlighter.highlight(language, &text);
    }
    spans
}

struct Loaded {
    path: String,
    data: ConflictData,
    editor: Option<MergeEditor>,
}

/// The editable result and the input that edits it. The input outlives a
/// render, which is why it is an entity rather than a value.
struct MergeEditor {
    model: MergeModel,
    input: Entity<TextareaState>,
    /// A subscription that dies with the editor; dropping it would leave the
    /// input emitting changes nobody applies.
    _changes: Subscription,
}

/// Why a conflict operation was submitted, for the notice after it finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    Resolve,
    MarkResolved,
    Mergetool,
}

impl PendingAction {
    fn notice(self) -> String {
        match self {
            PendingAction::Resolve => t!("conflict.resolved_title").to_string(),
            PendingAction::MarkResolved => t!("conflict.marked_resolved").to_string(),
            PendingAction::Mergetool => t!("conflict.merge_tool_finished").to_string(),
        }
    }
}

/// State of the conflict UI. Held by the worktree panel, which renders the
/// conflicts list and hands the right-hand area to [`Conflicts::pane`].
#[derive(Default)]
pub(crate) struct Conflicts {
    loaded: Option<Loaded>,
    /// Last repository state seen. A new merge, abort or rebase invalidates the
    /// sides that were read for the previous one.
    state: RepoState,
    /// Path whose sides are being read; the header shows it meanwhile.
    loading: Option<String>,
    /// Bumped on every load so a slow read cannot replace a newer one.
    epoch: u64,
    pending: Option<(OperationId, PendingAction)>,
    /// Path that was already asked to be marked resolved while its result was
    /// still unresolved or held markers; a second click confirms.
    marker_warning: Option<String>,
    notice: Option<String>,
    error: Option<String>,
    list_scroll: UniformListScrollHandle,
    /// Shared by both side panes: one list, so one offset.
    merge_scroll: UniformListScrollHandle,
}

impl Conflicts {
    /// Drops loaded data, e.g. when the worktree view closes, so reopening it
    /// reads the sides again instead of showing what was on screen before.
    pub(crate) fn clear(&mut self) {
        self.loaded = None;
        self.state = RepoState::Normal;
        self.loading = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.pending = None;
        self.marker_warning = None;
        self.notice = None;
        self.error = None;
    }

    /// Invalidates loaded sides when the repository enters or leaves a
    /// multi-step operation, so a second merge of the same path is not shown
    /// with the first merge's contents.
    pub(crate) fn observe(&mut self, snapshot: &RepoSnapshot) {
        if self.state != snapshot.state {
            self.state = snapshot.state;
            self.loaded = None;
            self.loading = None;
            // A read already in flight describes the operation that just ended.
            self.epoch = self.epoch.wrapping_add(1);
            self.error = None;
            self.notice = None;
        }
    }

    /// Whether `path` still needs its sides read. A failed read is not retried
    /// automatically: the snapshot refresh that follows a new selection will
    /// ask again, and a loop of failing reads would only hide the error.
    pub(crate) fn needs_load(&self, path: &str) -> bool {
        self.loaded_path() != Some(path) && self.loading.as_deref() != Some(path)
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn begin_load(&mut self, path: &str) -> u64 {
        self.epoch = self.epoch.wrapping_add(1);
        self.loading = Some(path.to_owned());
        self.error = None;
        self.notice = None;
        self.marker_warning = None;
        self.epoch
    }

    fn finish_load(
        &mut self,
        epoch: u64,
        path: String,
        result: strata_core::Result<ConflictData>,
        cx: &mut Context<RepoView>,
    ) {
        if epoch != self.epoch {
            return;
        }
        self.loading = None;
        match result {
            Ok(mut data) => {
                let editor = data
                    .model
                    .take()
                    .and_then(|model| install_editor(model, cx));
                self.loaded = Some(Loaded { path, data, editor });
                self.error = None;
                // A new path starts at the top of the panes.
                self.merge_scroll = UniformListScrollHandle::new();
            }
            Err(error) => {
                self.loaded = None;
                self.error = Some(error.to_string());
            }
        }
    }

    fn loaded_path(&self) -> Option<&str> {
        self.loaded.as_ref().map(|loaded| loaded.path.as_str())
    }

    /// The loaded data, when it belongs to `path`.
    fn data_for(&self, path: &str) -> Option<&Loaded> {
        self.loaded.as_ref().filter(|loaded| loaded.path == path)
    }

    fn editor_for(&self, path: &str) -> Option<&MergeEditor> {
        self.data_for(path)?.editor.as_ref()
    }

    fn start(&mut self, id: OperationId, action: PendingAction) {
        self.pending = Some((id, action));
        self.notice = None;
        self.error = None;
        self.marker_warning = None;
    }

    /// True when the operation belonged to this module.
    pub(crate) fn on_operation_finished(&mut self, id: OperationId, error: Option<&str>) -> bool {
        let Some((pending_id, action)) = self.pending else {
            return false;
        };
        if pending_id != id {
            return false;
        }
        self.pending = None;
        match error {
            // A failed write leaves the loaded result alone: the stale-file
            // refusal must not also throw away the merge the user typed.
            Some(error) => self.error = Some(error.to_owned()),
            None => {
                // The stages of the path just changed; the next selection reads
                // them again.
                self.loaded = None;
                self.error = None;
                self.notice = Some(action.notice().to_owned());
            }
        }
        true
    }

    /// Whether the loaded editor's result still needs an explicit confirmation
    /// before it can be staged.
    fn result_needs_confirmation(&self) -> bool {
        self.loaded
            .as_ref()
            .and_then(|loaded| loaded.editor.as_ref())
            .is_some_and(|editor| editor.model.has_markers() || !editor.model.all_resolved())
    }

    /// The message and detail of the confirmation banner.
    fn confirmation_message(&self) -> (String, String) {
        let Some(model) = self
            .loaded
            .as_ref()
            .and_then(|loaded| loaded.editor.as_ref())
            .map(|editor| &editor.model)
        else {
            return (
                t!("conflict.still_markers").to_string(),
                t!("conflict.edit_elsewhere").to_string(),
            );
        };
        let unresolved = model.unresolved_count();
        match (unresolved, model.has_markers()) {
            (0, true) => (
                t!("conflict.result_has_markers").to_string(),
                t!("conflict.remove_or_mark").to_string(),
            ),
            (count, true) => (
                if count == 1 {
                    t!("conflict.count_unresolved.one", count = count).to_string()
                } else {
                    t!("conflict.count_unresolved.other", count = count).to_string()
                },
                t!("conflict.result_has_markers").to_string(),
            ),
            (count, false) => (
                if count == 1 {
                    t!("conflict.count_unresolved.one", count = count).to_string()
                } else {
                    t!("conflict.count_unresolved.other", count = count).to_string()
                },
                t!("conflict.markers_edited").to_string(),
            ),
        }
    }
}

/// Creates the result pane's text area for a freshly loaded merge.
///
/// Text areas are entities, so this needs a window; the load finishes on the UI
/// thread with one active, and a window that has gone away simply leaves the
/// conflict without an editor.
fn install_editor(model: MergeModel, cx: &mut Context<RepoView>) -> Option<MergeEditor> {
    let text = model.text();
    // The load finishes on the app's update path, which has no window of its
    // own; the active window, or the only one open, is the one that will paint
    // the text area.
    let handle = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())?;
    let input = match handle.update(cx, |_, window, app| {
        app.new(|cx| TextareaState::new(window, cx).default_value(text))
    }) {
        Ok(input) => input,
        Err(error) => {
            log::warn!("could not create the conflict result editor: {error}");
            return None;
        }
    };
    // A change the editor itself makes (a conflict choice) goes through
    // `set_value`, which emits nothing, so every event here is the user typing.
    let changes = cx.subscribe(&input, |view, input, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            let text = input.read(cx).value().to_string();
            view.conflict_result_edited(&text, cx);
        }
    });
    Some(MergeEditor {
        model,
        input,
        _changes: changes,
    })
}

// ---------------------------------------------------------------------------
// RepoView glue: every write goes through the session.
// ---------------------------------------------------------------------------

impl RepoView {
    /// Reads the index stages of `path` and merges them, off the UI thread.
    pub(crate) fn load_conflict_sides(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let repo = session.repo().clone();
        let kind = session
            .snapshot()
            .conflicts
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.kind);
        let epoch = self.worktree.conflicts.begin_load(&path);
        cx.spawn(async move |this, cx| {
            let read_path = path.clone();
            let data = cx
                .background_executor()
                .spawn(async move {
                    let sides = repo.conflict_sides(&read_path)?;
                    let style = repo.conflict_style();
                    let worktree = repo.file_in_worktree(&read_path).ok().flatten();
                    Ok(ConflictData::new(
                        sides,
                        worktree.as_deref(),
                        &read_path,
                        kind,
                        style,
                    ))
                })
                .await;
            this.update(cx, |this, cx| {
                this.worktree.conflicts.finish_load(epoch, path, data, cx);
                this.notify_if_active(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Takes one whole side of the selected conflict and stages it.
    pub(crate) fn use_conflict_side(&mut self, side: ConflictSide, cx: &mut Context<Self>) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let Some(path) = self.worktree.selected_conflict().map(str::to_owned) else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let id = session.run(Operation::ResolveConflict { path, side });
        self.worktree.conflicts.start(id, PendingAction::Resolve);
        cx.notify();
    }

    /// Applies a per-conflict choice and shows the new result.
    pub(crate) fn apply_conflict_choice(
        &mut self,
        choice: MergeChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let changed = {
            let Some(editor) = self
                .worktree
                .conflicts
                .loaded
                .as_mut()
                .and_then(|loaded| loaded.editor.as_mut())
            else {
                return;
            };
            if !editor.model.apply(choice) {
                // Re-applying the same choice still changes which one is
                // recorded, so repaint even when the text stayed the same.
                return;
            }
            let input = editor.input.clone();
            let text = editor.model.text();
            input.update(cx, |state, cx| state.set_value(text, window, cx));
            true
        };
        if changed {
            self.worktree.conflicts.marker_warning = None;
            self.notify_if_active(cx);
        }
    }

    /// Puts the selected conflict back to its marker block.
    pub(crate) fn reset_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let changed = {
            let Some(editor) = self
                .worktree
                .conflicts
                .loaded
                .as_mut()
                .and_then(|loaded| loaded.editor.as_mut())
            else {
                return;
            };
            if !editor.model.reset() {
                return;
            }
            let input = editor.input.clone();
            let text = editor.model.text();
            input.update(cx, |state, cx| state.set_value(text, window, cx));
            true
        };
        if changed {
            self.worktree.conflicts.marker_warning = None;
            self.notify_if_active(cx);
        }
    }

    /// Moves the counter to the next or previous conflict and scrolls its rows
    /// into view.
    pub(crate) fn move_conflict(&mut self, forward: bool, cx: &mut Context<Self>) {
        let row = {
            let Some(loaded) = self.worktree.conflicts.loaded.as_mut() else {
                return;
            };
            let Some(editor) = loaded.editor.as_mut() else {
                return;
            };
            let moved = if forward {
                editor.model.next_conflict()
            } else {
                editor.model.previous_conflict()
            };
            if !moved {
                return;
            }
            loaded
                .data
                .conflict_rows
                .get(editor.model.selected())
                .copied()
        };
        if let Some(row) = row {
            self.worktree
                .conflicts
                .merge_scroll
                .scroll_to_item(row, ScrollStrategy::Center);
        }
        self.notify_if_active(cx);
    }

    /// Selects the conflict a side-pane row belongs to.
    pub(crate) fn select_conflict(&mut self, conflict: usize, cx: &mut Context<Self>) {
        let row = {
            let Some(loaded) = self.worktree.conflicts.loaded.as_mut() else {
                return;
            };
            let Some(editor) = loaded.editor.as_mut() else {
                return;
            };
            if !editor.model.select(conflict) {
                return;
            }
            loaded.data.conflict_rows.get(conflict).copied()
        };
        if let Some(row) = row {
            self.worktree
                .conflicts
                .merge_scroll
                .scroll_to_item(row, ScrollStrategy::Center);
        }
        self.notify_if_active(cx);
    }

    /// Applies a manual edit from the result pane to the resolution model.
    pub(crate) fn conflict_result_edited(&mut self, text: &str, cx: &mut Context<Self>) {
        let changed = {
            let Some(editor) = self
                .worktree
                .conflicts
                .loaded
                .as_mut()
                .and_then(|loaded| loaded.editor.as_mut())
            else {
                return;
            };
            editor.model.apply_text(text)
        };
        if changed {
            self.worktree.conflicts.marker_warning = None;
            self.notify_if_active(cx);
        }
    }

    /// Stages the selected conflict after checking the worktree file for
    /// markers, so a half-edited file is not marked resolved by accident.
    pub(crate) fn mark_conflict_resolved(&mut self, cx: &mut Context<Self>) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let Some(path) = self.worktree.selected_conflict().map(str::to_owned) else {
            return;
        };
        if self.worktree.conflicts.result_needs_confirmation() {
            self.worktree.conflicts.marker_warning = Some(path);
            cx.notify();
            return;
        }
        if self.worktree.conflicts.editor_for(&path).is_some() {
            self.stage_resolved_conflict(cx);
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let repo = session.repo().clone();
        let read_path = path.clone();
        cx.spawn(async move |this, cx| {
            let markers = cx
                .background_executor()
                .spawn(async move {
                    repo.file_in_worktree(&read_path)
                        .ok()
                        .flatten()
                        .is_some_and(|bytes| has_conflict_markers(&bytes))
                })
                .await;
            this.update(cx, |this, cx| {
                // The selection may have moved while the file was read; the
                // answer only applies to the path it was asked about.
                if this.worktree.selected_conflict() != Some(path.as_str()) {
                    return;
                }
                if markers {
                    this.worktree.conflicts.marker_warning = Some(path.clone());
                } else {
                    this.stage_resolved_conflict(cx);
                }
                this.notify_if_active(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Runs `MarkResolved` for the selected conflict, warning or not.
    pub(crate) fn stage_resolved_conflict(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.worktree.selected_conflict().map(str::to_owned) else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        // With the editor loaded, the result in it is what gets staged; the
        // hash refuses the write if the file changed underneath it. Without
        // one (binary, oversized, delete/modify), staging the worktree file is
        // the same action the user would take in a terminal.
        let operation = match self.worktree.conflicts.editor_for(&path) {
            Some(editor) => Operation::ResolveWithContent {
                path: path.clone(),
                content: editor.model.bytes(),
                expected: self
                    .worktree
                    .conflicts
                    .data_for(&path)
                    .and_then(|loaded| loaded.data.worktree_hash),
            },
            None => Operation::MarkResolved(vec![path]),
        };
        let id = session.run(operation);
        self.worktree
            .conflicts
            .start(id, PendingAction::MarkResolved);
        cx.notify();
    }

    pub(crate) fn dismiss_marker_warning(&mut self, cx: &mut Context<Self>) {
        self.worktree.conflicts.marker_warning = None;
        cx.notify();
    }

    pub(crate) fn open_merge_tool(&mut self, cx: &mut Context<Self>) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let Some(path) = self.worktree.selected_conflict().map(str::to_owned) else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let id = session.run(Operation::OpenMergetool { path });
        self.worktree.conflicts.start(id, PendingAction::Mergetool);
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Colors {
    design: Design,
    background: Hsla,
    muted: Hsla,
    muted_foreground: Hsla,
    border: Hsla,
    danger: Hsla,
    warning: Hsla,
    success: Hsla,
    /// The diff palette, so conflict lines use the same add/remove tinting and
    /// syntax colours as the diff panes.
    diff: DiffColors,
}

impl Colors {
    fn from_theme(theme: &Theme, settings: &Settings) -> Self {
        let design = Design::new(theme, settings);
        Self {
            background: design.background,
            muted: design.field,
            muted_foreground: design.muted,
            border: design.border,
            danger: design.remove_text,
            warning: design.tag_text,
            success: design.add_text,
            diff: DiffColors::from_theme(theme),
            design,
        }
    }
}

fn conflict_row(
    entity: &Entity<RepoView>,
    index: usize,
    path: String,
    kind: ConflictKind,
    selected: Option<&str>,
    colors: &Colors,
) -> AnyElement {
    let is_selected = selected == Some(path.as_str());
    let entity = entity.clone();
    let click_path = path.clone();

    let (name, dir) = style::path_parts(&path);
    crate::ui::list_row(
        ("conflict-file", index),
        crate::ui::RowState::NORMAL.selected(is_selected),
        &colors.design,
    )
    .h(px(ROW_HEIGHT))
    .cursor_pointer()
    .on_click(move |_, window, cx| {
        let path = click_path.clone();
        entity.update(cx, |this, cx| {
            this.select_conflict_file(path.clone(), window, cx)
        });
    })
    .child(crate::ui::change_badge(
        ChangeKind::Conflicted,
        &colors.design,
    ))
    .child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
    .when_some(dir, |row, dir| {
        row.child(
            div()
                .max_w(px(120.0))
                .flex_none()
                .truncate()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(dir.to_owned()),
        )
    })
    .child(
        div()
            .flex_none()
            .text_size(px(9.5))
            .text_color(colors.muted_foreground)
            .child(kind_label(kind)),
    )
    .into_any_element()
}

impl Conflicts {
    /// The conflicts section above the unstaged list; empty when there are none.
    pub(crate) fn section(
        &self,
        entity: Entity<RepoView>,
        conflicts: &[ConflictEntry],
        selected: Option<&str>,
        cx: &Context<RepoView>,
    ) -> AnyElement {
        if conflicts.is_empty() {
            return div().into_any_element();
        }
        let colors = Colors::from_theme(cx.theme(), Settings::get(cx));
        let entries: Rc<Vec<(String, ConflictKind)>> = Rc::new(
            conflicts
                .iter()
                .map(|entry| (entry.path.clone(), entry.kind))
                .collect(),
        );
        let selected: Option<Rc<str>> = selected.map(Rc::from);
        let visible = entries.len().min(MAX_LIST_ROWS);
        let list_colors = colors.clone();
        let list = uniform_list(
            "conflict-files",
            entries.len(),
            move |range, _window, _cx| {
                range
                    .map(|index| {
                        let (path, kind) = entries[index].clone();
                        conflict_row(
                            &entity,
                            index,
                            path,
                            kind,
                            selected.as_deref(),
                            &list_colors,
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.list_scroll)
        .with_scrollbar(&self.list_scroll)
        .h(px(ROW_HEIGHT * visible as f32))
        .flex_none();

        div()
            .flex()
            .flex_col()
            .flex_none()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .pt_3()
                    .pb_1p5()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(colors.danger)
                            .child(t!("conflict.header", count = conflicts.len()).to_string()),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    /// The right-hand area while a conflicted file is selected.
    pub(crate) fn pane(
        &self,
        path: &str,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        cx: &Context<RepoView>,
    ) -> AnyElement {
        let colors = Colors::from_theme(cx.theme(), Settings::get(cx));
        let style = DiffStyle::from_settings(cx, &colors.design.mono);
        let busy = snapshot.busy.is_some() || self.pending.is_some();
        let loaded = self.data_for(path);

        let (name, dir) = style::path_parts(path);
        let mut header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .h(px(40.0))
            .flex_none()
            .px_3p5()
            .bg(colors.background)
            .border_b_1()
            .border_color(colors.border)
            .text_size(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(crate::ui::change_badge(
                        ChangeKind::Conflicted,
                        &colors.design,
                    ))
                    .child(div().truncate().child(name.to_owned()))
                    .when_some(dir, |row, dir| {
                        row.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.5))
                                .text_color(colors.muted_foreground)
                                .child(dir.to_owned()),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(colors.warning)
                            .child(loaded.map_or_else(
                                || "conflict".to_owned(),
                                |loaded| kind_label(loaded.data.kind),
                            )),
                    ),
            );
        if let Some(loaded) = loaded {
            header = header.child(
                div()
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(colors.muted_foreground)
                    .child(sizes_line(&loaded.data)),
            );
        }

        let ready = loaded.is_some() && !busy;
        let kind = loaded.map(|loaded| loaded.data.kind);
        let mut toolbar = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_none()
            .px_3p5()
            .h(px(40.0))
            .border_b_1()
            .border_color(colors.border);

        if let Some(kind) = kind {
            for (index, choice) in choices(kind).into_iter().enumerate() {
                let entity = entity.clone();
                toolbar = toolbar.child(
                    crate::ui::button(("conflict-choice", index), choice.label)
                        .disabled(!ready)
                        .when(choice.destructive, crate::ui::UiButton::danger)
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.use_conflict_side(choice.side, cx));
                        }),
                );
            }
            toolbar = toolbar
                .child(
                    crate::ui::button("conflict-mergetool", t!("conflict.merge_tool").to_string())
                        .disabled(!ready)
                        .on_click({
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.open_merge_tool(cx));
                            }
                        }),
                )
                .child(
                    crate::ui::button(
                        "conflict-mark-resolved",
                        t!("conflict.mark_resolved").to_string(),
                    )
                    .disabled(!ready)
                    .on_click({
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| this.mark_conflict_resolved(cx));
                        }
                    }),
                );
        }

        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(colors.background);

        if self.marker_warning.as_deref() == Some(path) {
            column = column.child(self.marker_warning_bar(entity.clone(), &colors));
        } else if loaded
            .is_some_and(|loaded| loaded.data.worktree_markers && loaded.editor.is_none())
        {
            column = column.child(crate::ui::banner(
                crate::ui::BannerKind::Warning,
                t!("conflict.on_disk_markers").to_string(),
                &colors.design,
            ));
        }

        column = column.child(match (&self.error, loaded) {
            (Some(error), _) => {
                crate::ui::message(error, colors.danger, &colors.design).into_any_element()
            }
            (None, None) => crate::ui::message(
                t!("conflict.reading").to_string(),
                colors.muted_foreground,
                &colors.design,
            )
            .into_any_element(),
            (None, Some(loaded)) => editor_body(
                loaded,
                &entity,
                &colors,
                &style,
                busy,
                self.merge_scroll.clone(),
            ),
        });

        if let Some(notice) = &self.notice {
            column = column.child(
                div()
                    .flex_none()
                    .px_3()
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .text_size(px(10.0))
                    .text_color(colors.success)
                    .child(notice.clone()),
            );
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(header)
            .child(toolbar)
            .child(column)
            .into_any_element()
    }

    fn marker_warning_bar(&self, entity: Entity<RepoView>, colors: &Colors) -> AnyElement {
        let cancel = {
            let entity = entity.clone();
            crate::ui::button("marker-cancel", t!("common.cancel").to_string()).on_click(
                move |_, _, cx| {
                    entity.update(cx, |this, cx| this.dismiss_marker_warning(cx));
                },
            )
        };
        let resolve = {
            let entity = entity.clone();
            crate::ui::button("marker-confirm", t!("conflict.mark_anyway").to_string())
                .danger()
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.stage_resolved_conflict(cx));
                })
        };
        let (message, detail) = self.confirmation_message();
        crate::ui::banner(crate::ui::BannerKind::Danger, message, &colors.design)
            .detail(detail)
            .action(cancel)
            .action(resolve)
            .into_any_element()
    }
}

/// The body under the toolbar: the merge editor, or why it is not offered.
fn editor_body(
    loaded: &Loaded,
    entity: &Entity<RepoView>,
    colors: &Colors,
    style: &DiffStyle,
    busy: bool,
    scroll: UniformListScrollHandle,
) -> AnyElement {
    let data = &loaded.data;
    if data.binary {
        return crate::ui::message(
            t!("conflict.binary").to_string(),
            colors.muted_foreground,
            &colors.design,
        )
        .into_any_element();
    }
    if !data.text {
        return crate::ui::message(
            t!("conflict.not_utf8").to_string(),
            colors.muted_foreground,
            &colors.design,
        )
        .into_any_element();
    }
    if !kind_has_editor(data.kind) {
        return crate::ui::message(
            t!("conflict.whole_file").to_string(),
            colors.muted_foreground,
            &colors.design,
        )
        .into_any_element();
    }
    if data.too_large {
        return crate::ui::message(
            t!("conflict.too_large").to_string(),
            colors.muted_foreground,
            &colors.design,
        )
        .into_any_element();
    }
    let Some(editor) = &loaded.editor else {
        return crate::ui::message(
            t!("conflict.no_text_diff").to_string(),
            colors.muted_foreground,
            &colors.design,
        )
        .into_any_element();
    };

    let conflict_bar = conflict_bar(editor, entity, colors, busy);
    let panes = side_panes(loaded, editor, entity, colors, style, scroll);
    let result = result_pane(editor, colors, style, busy);
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(conflict_bar)
        .child(panes)
        .child(result)
        .into_any_element()
}

/// The counter, the conflict navigation and the per-conflict choices.
fn conflict_bar(
    editor: &MergeEditor,
    entity: &Entity<RepoView>,
    colors: &Colors,
    busy: bool,
) -> AnyElement {
    let model = &editor.model;
    let mut bar = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1p5()
        .flex_none()
        .px_3()
        .py_1p5()
        .border_b_1()
        .border_color(colors.border);

    match model.counter() {
        Some(counter) => {
            bar = bar.child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if model.all_resolved() {
                        colors.success
                    } else {
                        colors.warning
                    })
                    .child(counter),
            );
            bar = bar
                .child(
                    crate::ui::button("conflict-previous", t!("conflict.previous").to_string())
                        .size(crate::ui::ButtonSize::Tiny)
                        .disabled(busy || model.conflict_count() < 2)
                        .on_click({
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.move_conflict(false, cx));
                            }
                        }),
                )
                .child(
                    crate::ui::button("conflict-next", t!("conflict.next").to_string())
                        .size(crate::ui::ButtonSize::Tiny)
                        .disabled(busy || model.conflict_count() < 2)
                        .on_click({
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.move_conflict(true, cx));
                            }
                        }),
                );
        }
        None => {
            bar = bar.child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(colors.success)
                    .child(t!("conflict.none_left").to_string()),
            );
        }
    }

    for (index, choice) in MergeChoice::ALL.into_iter().enumerate() {
        let entity = entity.clone();
        bar = bar.child(
            crate::ui::button(("conflict-take", index), choice.label())
                .size(crate::ui::ButtonSize::Tiny)
                .disabled(busy || model.conflict_count() == 0)
                .tooltip(format!("{} ({})", choice.label(), choice.shortcut()))
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.apply_conflict_choice(choice, window, cx)
                    });
                }),
        );
    }
    let entity_reset = entity.clone();
    bar.child(
        crate::ui::button("conflict-reset", t!("common.reset").to_string())
            .size(crate::ui::ButtonSize::Tiny)
            .disabled(
                busy || model.conflict_count() == 0
                    || model.selected_resolution() == Some(Resolution::Unresolved)
                    || model.selected_resolution().is_none(),
            )
            .on_click(move |_, window, cx| {
                entity_reset.update(cx, |this, cx| this.reset_conflict(window, cx));
            }),
    )
    .into_any_element()
}

/// Ours and theirs, one list row per line pair, so the two sides always show
/// the same scroll position.
fn side_panes(
    loaded: &Loaded,
    editor: &MergeEditor,
    entity: &Entity<RepoView>,
    colors: &Colors,
    style: &DiffStyle,
    scroll: UniformListScrollHandle,
) -> AnyElement {
    let data = &loaded.data;
    let rows = data.rows.clone();
    let ours_lines = data.ours_lines.clone();
    let theirs_lines = data.theirs_lines.clone();
    let ours_highlights = data.ours_highlights.clone();
    let theirs_highlights = data.theirs_highlights.clone();
    let selected = editor.model.selected();
    let row_height = style.row_height();
    let font_size = style.font_size();
    let mono = style.mono().clone();
    let diff = colors.diff;
    let design = colors.design.clone();
    let row_design = design.clone();
    let row_style = style.clone();
    let row_entity = entity.clone();

    let list = uniform_list(
        "conflict-merge-rows",
        rows.len(),
        move |range, _window, _cx| {
            range
                .map(|index| {
                    let row = rows[index];
                    let left = side_tint(row, true, selected, &row_design);
                    let right = side_tint(row, false, selected, &row_design);
                    let mut element = div()
                        .id(("conflict-merge-row", index))
                        .flex()
                        .flex_row()
                        .items_stretch()
                        .w_full()
                        .h(px(row_height))
                        .font_family(mono.clone())
                        .text_size(px(font_size));
                    if let Some(conflict) = row.conflict {
                        let click = row_entity.clone();
                        element = element.cursor_pointer().on_click(move |_, _, cx| {
                            click.update(cx, |this, cx| this.select_conflict(conflict, cx));
                        });
                    }
                    if row.conflict == Some(selected) {
                        element = element.border_l_2().border_color(row_design.accent);
                    }
                    element
                        .child(side_cell(
                            row.ours,
                            &ours_lines,
                            &ours_highlights,
                            left,
                            &diff,
                            &row_style,
                            true,
                            &row_design,
                        ))
                        .child(side_cell(
                            row.theirs,
                            &theirs_lines,
                            &theirs_highlights,
                            right,
                            &diff,
                            &row_style,
                            false,
                            &row_design,
                        ))
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        },
    )
    .track_scroll(&scroll)
    .with_scrollbar(&scroll)
    .flex_1()
    .min_h_0();

    let ours_header = pane_header(t!("conflict.ours_header").to_string(), &design);
    let theirs_header = pane_header(t!("conflict.theirs_header").to_string(), &design);
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .border_b_1()
        .border_color(design.border)
        .child(
            // The two headings sit over the halves of the single row list.
            div()
                .flex()
                .flex_row()
                .flex_none()
                .w_full()
                .child(div().flex_1().min_w_0().child(ours_header))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .border_l_1()
                        .border_color(design.border)
                        .child(theirs_header),
                ),
        )
        .child(list)
        .into_any_element()
}

fn pane_header(title: impl Into<gpui::SharedString>, design: &Design) -> AnyElement {
    div()
        .flex()
        .items_center()
        .h(px(20.0))
        .flex_none()
        .px_2()
        .bg(design.field)
        .text_size(px(9.5))
        .text_color(design.muted)
        .child(title.into())
        .into_any_element()
}

fn side_tint(row: MergeRow, ours_side: bool, selected: usize, design: &Design) -> Option<Hsla> {
    match (row.kind, ours_side) {
        // The selected conflict is the one the buttons act on; a stronger tint
        // keeps it visible among the others.
        (SegmentKind::Conflict, _) if row.conflict == Some(selected) => {
            Some(design.diff_removed_word)
        }
        (SegmentKind::Conflict, _) => Some(design.diff_removed_bg),
        (SegmentKind::Ours, true) => Some(design.diff_added_bg),
        (SegmentKind::Theirs, false) => Some(design.accent_soft),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn side_cell(
    line: Option<u32>,
    lines: &[String],
    highlights: &[LineHighlights],
    tint: Option<Hsla>,
    diff: &DiffColors,
    style: &DiffStyle,
    left: bool,
    design: &Design,
) -> AnyElement {
    let cell = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .when_some(tint, |cell, tint| cell.bg(tint))
        .when(left, |cell| cell.border_r_1().border_color(design.border));
    match line {
        Some(number) => {
            let text = lines
                .get(number as usize - 1)
                .map(String::as_str)
                .unwrap_or_default();
            let spans = highlights.get(number as usize - 1);
            cell.child(
                div()
                    .flex_none()
                    .w(px(38.0))
                    .pr_1()
                    .text_size(px(style.font_size() - 2.0))
                    .text_color(diff.gutter)
                    .text_right()
                    .child(number.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(diff_view::styled_line(text, style, spans, diff)),
            )
        }
        None => cell.child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(style.font_size() - 2.0))
                .text_color(diff.gutter)
                .child(""),
        ),
    }
    .into_any_element()
}

/// The editable result under the side panes.
fn result_pane(editor: &MergeEditor, colors: &Colors, style: &DiffStyle, busy: bool) -> AnyElement {
    let model = &editor.model;
    let status = match model.counter() {
        Some(_) if model.all_resolved() && !model.has_markers() => {
            t!("conflict.all_resolved").to_string()
        }
        Some(counter) => {
            let unresolved = model.unresolved_count();
            t!(
                "conflict.unresolved_count",
                counter = counter,
                count = unresolved
            )
            .to_string()
        }
        None => t!("conflict.automatic").to_string(),
    };
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .h(px(20.0))
                .flex_none()
                .px_2()
                .bg(colors.muted.opacity(0.4))
                .text_size(px(9.5))
                .text_color(colors.muted_foreground)
                .child(t!("conflict.result").to_string())
                .child(div().flex_1().min_w_0().truncate().child(status))
                .child(t!("conflict.edit_manually").to_string()),
        )
        .child(
            div().flex_1().min_h_0().w_full().p_1p5().child(
                Textarea::new(&editor.input)
                    .h(relative(1.0))
                    .w_full()
                    .text_size(px(style.font_size()))
                    .font_family(style.mono().clone())
                    .disabled(busy),
            ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sides(base: &str, ours: &str, theirs: &str) -> ConflictSides {
        ConflictSides {
            base: Some(base.as_bytes().to_vec()),
            ours: Some(ours.as_bytes().to_vec()),
            theirs: Some(theirs.as_bytes().to_vec()),
        }
    }

    fn data(sides: ConflictSides, kind: ConflictKind) -> ConflictData {
        ConflictData::new(sides, None, "src/lib.rs", Some(kind), ConflictStyle::Merge)
    }

    #[test]
    fn every_kind_has_plain_words() {
        assert_eq!(
            kind_label(ConflictKind::BothModified),
            t!("conflict.kind.both_modified").to_string()
        );
        assert_eq!(
            kind_label(ConflictKind::BothAdded),
            t!("conflict.kind.both_added").to_string()
        );
        assert_eq!(
            kind_label(ConflictKind::BothDeleted),
            t!("conflict.kind.both_deleted").to_string()
        );
        assert_eq!(
            kind_label(ConflictKind::AddedByUs),
            t!("conflict.kind.added_by_us").to_string()
        );
        assert_eq!(
            kind_label(ConflictKind::AddedByThem),
            t!("conflict.kind.added_by_them").to_string()
        );
        assert_eq!(
            kind_label(ConflictKind::DeletedByUs),
            t!("conflict.kind.deleted_by_us").to_string()
        );
        assert_eq!(
            kind_label(ConflictKind::DeletedByThem),
            t!("conflict.kind.deleted_by_them").to_string()
        );
    }

    #[test]
    fn delete_modify_kinds_offer_keep_and_delete() {
        let keep = choices(ConflictKind::DeletedByThem);
        assert_eq!(keep.len(), 2);
        assert_eq!(keep[0].label, "Keep file");
        assert_eq!(keep[0].side, ConflictSide::Ours, "our version still exists");
        assert_eq!(keep[1].label, "Delete file");
        assert_eq!(keep[1].side, ConflictSide::Theirs, "their side deleted it");
        assert!(keep[1].destructive);

        let keep_theirs = choices(ConflictKind::DeletedByUs);
        assert_eq!(keep_theirs[0].side, ConflictSide::Theirs);
        assert_eq!(keep_theirs[1].side, ConflictSide::Ours);
        assert!(keep_theirs[1].destructive);

        let content = choices(ConflictKind::BothModified);
        assert_eq!(content[0].label, "Use ours");
        assert_eq!(content[1].label, "Use theirs");
        assert!(content.iter().all(|choice| !choice.destructive));

        assert!(
            choices(ConflictKind::BothDeleted).is_empty(),
            "no side exists to check out"
        );
    }

    #[test]
    fn only_content_conflicts_get_the_merge_editor() {
        assert!(kind_has_editor(ConflictKind::BothModified));
        assert!(kind_has_editor(ConflictKind::BothAdded));
        for kind in [
            ConflictKind::DeletedByUs,
            ConflictKind::DeletedByThem,
            ConflictKind::AddedByUs,
            ConflictKind::AddedByThem,
            ConflictKind::BothDeleted,
        ] {
            assert!(!kind_has_editor(kind), "{kind:?} resolves by file");
        }
    }

    #[test]
    fn kinds_are_derived_from_the_stages_present() {
        assert_eq!(
            kind_from_presence(true, true, true),
            ConflictKind::BothModified
        );
        assert_eq!(
            kind_from_presence(false, true, true),
            ConflictKind::BothAdded
        );
        assert_eq!(
            kind_from_presence(true, true, false),
            ConflictKind::DeletedByThem
        );
        assert_eq!(
            kind_from_presence(true, false, true),
            ConflictKind::DeletedByUs
        );
        assert_eq!(
            kind_from_presence(false, false, false),
            ConflictKind::BothModified,
            "a submodule conflict keeps the path usable"
        );
    }

    #[test]
    fn markers_are_found_at_the_start_of_a_line() {
        assert!(has_conflict_markers(
            b"<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> topic\n"
        ));
        assert!(has_conflict_markers(b"a\n||||||| base\nb\n"));
        assert!(!has_conflict_markers(b"a\nb\n"));
        assert!(
            !has_conflict_markers(b"  <<<<<<< indented is not a marker\n"),
            "git's markers start in column zero"
        );
        assert!(
            has_conflict_markers(b"# heading\n=======\ntext\n"),
            "a rule of equals signs is indistinguishable from a marker"
        );
    }

    #[test]
    fn binary_detection_matches_the_nul_byte_rule() {
        assert!(is_binary(b"\x7fELF\x00\x00"));
        assert!(!is_binary(b"plain text\n"));
        let mut late_nul = vec![b'a'; BINARY_PROBE + 10];
        late_nul.push(0);
        assert!(!is_binary(&late_nul), "only the head is probed, as in git");
    }

    #[test]
    fn a_text_conflict_loads_rows_and_the_merge_model() {
        let data = data(
            sides(
                "one\ntwo\nthree\n",
                "one\nOURS\nthree\n",
                "one\nTHEIRS\nthree\n",
            ),
            ConflictKind::BothModified,
        );
        assert!(data.text);
        assert!(!data.binary && !data.too_large);
        assert_eq!(data.model.as_ref().map(MergeModel::conflict_count), Some(1));
        assert_eq!(data.conflict_rows, vec![1]);
        assert_eq!(data.rows.len(), 3);
        assert_eq!(data.rows[1].conflict, Some(0));
        assert!(data.model.is_some());
    }

    #[test]
    fn a_binary_conflict_has_sizes_instead_of_an_editor() {
        let data = data(
            ConflictSides {
                base: Some(vec![0, 1, 2]),
                ours: Some(vec![0, 3, 4]),
                theirs: Some(vec![0, 5, 6]),
            },
            ConflictKind::BothModified,
        );
        assert!(data.binary);
        assert!(!data.text);
        assert!(data.rows.is_empty());
        assert!(data.model.is_none());
        assert_eq!(sizes_line(&data), "ours 3 B · theirs 3 B · base 3 B");
    }

    #[test]
    fn a_delete_modify_conflict_keeps_whole_file_choices() {
        let data = data(
            ConflictSides {
                base: Some(b"a\nb\n".to_vec()),
                ours: None,
                theirs: Some(b"a\nb\nc\n".to_vec()),
            },
            ConflictKind::DeletedByUs,
        );
        assert!(data.text);
        assert!(data.model.is_none(), "resolving it is a file-level choice");
        assert!(data.rows.is_empty());
    }

    #[test]
    fn an_oversized_file_keeps_the_whole_file_choices() {
        let big = format!("{}\n", "x".repeat(MAX_EDITOR_BYTES + 1));
        let data = data(
            sides(&big, &format!("{big}ours\n"), &format!("{big}theirs\n")),
            ConflictKind::BothModified,
        );
        assert!(data.text);
        assert!(data.too_large);
        assert!(data.model.is_none());
    }

    #[test]
    fn highlight_sides_covers_both_editable_sides_and_skips_unknown_languages() {
        let sides = sides("fn a() {}\n", "fn b() {}\n", "fn c() {}\n");
        let ours_lines = sides.ours.as_deref().map(|bytes| {
            String::from_utf8_lossy(bytes)
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let theirs_lines = sides.theirs.as_deref().map(|bytes| {
            String::from_utf8_lossy(bytes)
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let (ours, theirs) = highlight_sides(
            "src/lib.rs",
            (ours_lines.as_deref().unwrap_or_default(), true),
            (theirs_lines.as_deref().unwrap_or_default(), true),
        );
        assert_eq!(ours.len(), 2, "one range per line, plus the empty tail");
        assert!(ours.iter().any(|spans| !spans.is_empty()));
        assert!(theirs.iter().any(|spans| !spans.is_empty()));

        let (ours, theirs) = highlight_sides("notes.unknown", (&[], true), (&[], true));
        assert!(ours.is_empty() && theirs.is_empty());
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }
}
