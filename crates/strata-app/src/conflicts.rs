//! Merge conflict resolution: the conflicts list and the per-file merge editor.
//!
//! The session is the only route to git. The three sides of a conflicted path
//! come from `Repo::conflict_sides` (a gix read, run on the background
//! executor) and are merged into regions by [`strata_core::merge`]; every write
//! is an [`Operation`] and nothing here spawns git.
//!
//! The editor shows A (ours) and B (theirs) side by side over the editable
//! result, with a third base column when it is asked for. Both side panes are
//! one virtualised list whose rows carry a line of each side, so they cannot
//! drift out of sync, and a file above [`MAX_EDITOR_BYTES`] keeps the
//! whole-file choices and the external merge tool instead of building an
//! editor that large.
//!
//! A text conflict opens the editor over the whole repository view: the
//! sidebar and the working-tree lists step aside until Escape or the header's
//! close button returns to the normal layout. The choice buttons are gone;
//! each conflict has a checkbox per side in the side panes, and the header's
//! checkbox takes that side for every conflict in the file.

use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Hsla, InteractiveElement, IntoElement,
    ParentElement, Point, ScrollStrategy, StatefulInteractiveElement, Styled, Subscription,
    UniformListScrollHandle, Window, actions, div, px, relative, uniform_list,
};
use gpui_component::ActiveTheme as _;
use gpui_component::Selectable as _;
use gpui_component::Theme;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{
    Editor, EditorState, GutterDecoration, GutterDecorationCollection, InputEvent, RangeDecoration,
    RangeDecorationCollection, RangeDecorationStyle,
};
use gpui_component::menu::PopupMenuItem;
use gpui_component::tooltip::Tooltip;
use gpui_kit_assets::IconName;
use strata_core::encoding::TextEncoding;
use strata_core::highlight::{Highlighter, LineHighlights, language_for_path};
use strata_core::image::{ImageKind, ImageSide};
use strata_core::merge::{ConflictStyle, ContentHash, MergeAnalysis, SegmentKind};
use strata_core::repo::{InProgressCommit, InProgressDetails, MergeTool};
use strata_core::session::{ConflictSide, Operation, OperationId, RepoSnapshot};
use strata_core::{
    ChangeKind, ConflictEntry, ConflictKind, ConflictSides, HeadInfo, Oid, RepoState,
};

use crate::activity::ActivityKind;
use crate::diff_view::{self, DiffColors, DiffStyle};
use crate::merge_editor::{Choice as MergeChoice, LineMark, MergeModel, MergeRow, Side, Ticked};
use crate::repo_view::RepoView;
use crate::scrollbar::WithScrollbar as _;
use crate::settings::Settings;
use crate::style::{self, Design, LANES};

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

/// Folds a command's multi-line message into the one line a banner shows.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether the header offers the external merge tool, and the tooltip that
/// explains the state. A disabled button still needs to say what is missing.
pub(crate) fn merge_tool_state(tool: Option<&MergeTool>) -> (bool, String) {
    match tool {
        Some(tool) => (
            true,
            t!("conflict.merge_tool_configured", tool = tool.name.clone()).to_string(),
        ),
        None => (false, t!("conflict.merge_tool_missing").to_string()),
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

/// What one side pane's header says: the pane letter, the commit it shows and
/// the part it plays in the operation that stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SideRole {
    pub sha: Option<String>,
    /// A branch, source ref or commit summary.
    pub name: Option<String>,
    pub role: String,
}

impl SideRole {
    fn new(role: String) -> Self {
        Self {
            sha: None,
            name: None,
            role,
        }
    }

    fn at(mut self, sha: Option<String>, name: Option<String>) -> Self {
        self.sha = sha;
        self.name = name;
        self
    }

    /// `A · 60a3858 · feature`, with the parts that are known.
    pub(crate) fn label(&self, side: Side) -> String {
        let mut parts = vec![side.letter().to_owned()];
        if let Some(sha) = &self.sha {
            parts.push(sha.clone());
        }
        if let Some(name) = &self.name {
            parts.push(name.clone());
        }
        parts.join(" · ")
    }
}

/// Seven hex characters, the length git uses for a short id.
fn short_hash(id: Oid) -> String {
    id.to_hex().to_string().chars().take(7).collect()
}

fn head_parts(head: Option<&HeadInfo>) -> (Option<String>, Option<String>) {
    match head {
        Some(HeadInfo::Branch { short_name, target }) => {
            (target.map(short_hash), Some(short_name.clone()))
        }
        Some(HeadInfo::Detached { target }) => (Some(short_hash(*target)), Some("HEAD".to_owned())),
        None => (None, None),
    }
}

fn details_commit(commit: Option<&InProgressCommit>) -> (Option<String>, Option<String>) {
    match commit {
        Some(commit) => (
            Some(commit.short_id.clone()),
            commit
                .summary
                .clone()
                .or_else(|| Some(commit.short_id.clone())),
        ),
        None => (None, None),
    }
}

/// The two pane headers for the operation that stopped.
///
/// A merge and a plain checkout are described in ours/theirs terms. During a
/// rebase, cherry-pick or revert those words would be misleading: side A is
/// what the work is being applied on top of, and side B is the commit being
/// applied or reverted.
pub(crate) fn side_roles(
    state: RepoState,
    details: Option<&InProgressDetails>,
    head: Option<&HeadInfo>,
) -> (SideRole, SideRole) {
    let (head_sha, head_name) = head_parts(head);
    match (state, details) {
        (
            RepoState::Rebasing,
            Some(InProgressDetails::Rebase {
                onto,
                current_commit,
                ..
            }),
        ) => {
            let (applied_sha, applied_name) = details_commit(current_commit.as_ref());
            (
                SideRole::new(t!("conflict.role.rebase_onto").to_string()).at(
                    onto.as_ref().map(|onto| onto.short_id.clone()),
                    onto.as_ref().and_then(|onto| onto.ref_name.clone()),
                ),
                SideRole::new(t!("conflict.role.rebase_applying").to_string())
                    .at(applied_sha, applied_name),
            )
        }
        (RepoState::Rebasing, _) => (
            SideRole::new(t!("conflict.role.rebase_onto").to_string()),
            SideRole::new(t!("conflict.role.rebase_applying").to_string()),
        ),
        (RepoState::CherryPicking, Some(InProgressDetails::CherryPick { commit, .. })) => {
            let (sha, name) = details_commit(commit.as_ref());
            (
                SideRole::new(t!("conflict.role.current").to_string())
                    .at(head_sha.clone(), head_name.clone()),
                SideRole::new(t!("conflict.role.cherry_pick").to_string()).at(sha, name),
            )
        }
        (RepoState::CherryPicking, _) => (
            SideRole::new(t!("conflict.role.current").to_string()).at(head_sha, head_name),
            SideRole::new(t!("conflict.role.cherry_pick").to_string()),
        ),
        (RepoState::Reverting, Some(InProgressDetails::Revert { commit, .. })) => {
            let (sha, name) = details_commit(commit.as_ref());
            (
                SideRole::new(t!("conflict.role.current").to_string())
                    .at(head_sha.clone(), head_name.clone()),
                SideRole::new(t!("conflict.role.revert").to_string()).at(sha, name),
            )
        }
        (RepoState::Reverting, _) => (
            SideRole::new(t!("conflict.role.current").to_string()).at(head_sha, head_name),
            SideRole::new(t!("conflict.role.revert").to_string()),
        ),
        (RepoState::Merging, Some(InProgressDetails::Merge { heads, source })) => {
            let (sha, name) = match heads.first() {
                Some(commit) => (
                    Some(commit.short_id.clone()),
                    source.clone().or_else(|| commit.summary.clone()),
                ),
                None => (None, source.clone()),
            };
            (
                SideRole::new(t!("conflict.role.ours").to_string()).at(head_sha, head_name),
                SideRole::new(t!("conflict.role.theirs").to_string()).at(sha, name),
            )
        }
        _ => (
            SideRole::new(t!("conflict.role.ours").to_string()).at(head_sha, head_name),
            SideRole::new(t!("conflict.role.theirs").to_string()),
        ),
    }
}

/// The three stage pictures of a conflicted image, when the path names one.
///
/// A raster image is binary, so the text editor has nothing to merge; showing
/// the pictures is the useful view. SVG is text and keeps the editor.
pub(crate) struct ConflictImages {
    pub base: Option<ImageSide>,
    pub ours: Option<ImageSide>,
    pub theirs: Option<ImageSide>,
}

pub(crate) fn conflict_images(path: &str, sides: &ConflictSides) -> Option<ConflictImages> {
    if strata_core::image::kind_for_path(path) != Some(ImageKind::Raster) {
        return None;
    }
    let stage = |bytes: &Option<Vec<u8>>| {
        bytes.as_ref().map(|bytes| {
            ImageSide::new(
                ImageKind::Raster,
                Some(bytes.clone()),
                bytes.len() as u64,
                strata_core::image::MAX_IMAGE_BYTES,
            )
        })
    };
    let images = ConflictImages {
        base: stage(&sides.base),
        ours: stage(&sides.ours),
        theirs: stage(&sides.theirs),
    };
    (images.ours.is_some() || images.theirs.is_some()).then_some(images)
}

/// The byte range of every line of `text`, including its newline.
///
/// Decorations are placed by these ranges; a line without a final newline ends
/// at the text's end.
fn line_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.len();
        ranges.push(start..end);
        start = end;
    }
    // An empty text still has one (empty) line, so the result pane always has
    // a place to put the caret.
    if ranges.is_empty() {
        ranges.push(0..0);
    }
    ranges
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
    /// The encoding the result is written back in.
    pub(crate) encoding: TextEncoding,
    /// The file is past the editor's size or conflict cap.
    pub(crate) too_large: bool,
    /// The stage pictures of a conflicted raster image.
    pub(crate) images: Option<ConflictImages>,
    /// The external merge tool the repository configures, when it sets one.
    /// Without it the header does not offer git's fallback tool list.
    pub(crate) merge_tool: Option<MergeTool>,
    /// Visual rows of the side panes.
    pub(crate) rows: Arc<Vec<MergeRow>>,
    /// First visual row of each conflict, for scrolling to it.
    pub(crate) conflict_rows: Vec<usize>,
    ours_lines: Arc<Vec<String>>,
    theirs_lines: Arc<Vec<String>>,
    base_lines: Arc<Vec<String>>,
    ours_highlights: Arc<Vec<LineHighlights>>,
    theirs_highlights: Arc<Vec<LineHighlights>>,
    base_highlights: Arc<Vec<LineHighlights>>,
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
        merge_tool: Option<MergeTool>,
    ) -> Self {
        let images = conflict_images(path, &sides);
        let mut analysis = MergeAnalysis::analyze(&sides, style);
        let binary = analysis.binary;
        let text = analysis.text;
        let encoding = analysis.encoding;
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
        let (ours_highlights, theirs_highlights, base_highlights) = if text {
            (
                highlight_file(path, &analysis.ours.lines, analysis.ours.trailing_newline),
                highlight_file(
                    path,
                    &analysis.theirs.lines,
                    analysis.theirs.trailing_newline,
                ),
                highlight_file(path, &analysis.base.lines, analysis.base.trailing_newline),
            )
        } else {
            Default::default()
        };

        let base_lines = Arc::new(std::mem::take(&mut analysis.base.lines));
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
            encoding,
            too_large,
            images,
            merge_tool,
            rows,
            conflict_rows,
            ours_lines,
            theirs_lines,
            base_lines,
            ours_highlights: Arc::new(ours_highlights),
            theirs_highlights: Arc::new(theirs_highlights),
            base_highlights: Arc::new(base_highlights),
            model,
        }
    }
}

/// The syntax spans of one stage, indexed by line number minus one.
///
/// Highlighting the whole files rather than the conflict fragments keeps a
/// construct that starts outside a region (a doc comment, a raw string)
/// coloured the way it is in the file; [`MergeRow`] then finds each line's
/// spans by the line number it kept from the file.
fn highlight_file(path: &str, lines: &[String], trailing: bool) -> Vec<LineHighlights> {
    let Some(language) = language_for_path(path) else {
        return Vec::new();
    };
    let mut text = lines.join("\n");
    if trailing && !lines.is_empty() {
        text.push('\n');
    }
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return Vec::new();
    }
    Highlighter::new().highlight(language, &text)
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
    /// A code editor rather than a plain text area: its range and gutter
    /// decorations are what colour a conflict block and mark each result line
    /// with the side it came from.
    input: Entity<EditorState>,
    ranges: RangeDecorationCollection,
    gutters: GutterDecorationCollection,
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
    /// A failed external merge tool run. Kept apart from [`Self::error`] so
    /// the notice sits above the editor instead of replacing it: the tool runs
    /// against the same worktree file and leaves the result the user has.
    merge_tool_error: Option<String>,
    list_scroll: UniformListScrollHandle,
    /// Shared by both side panes: one list, so one offset.
    merge_scroll: UniformListScrollHandle,
    /// The merge editor is using the whole view, with the sidebar and file
    /// lists hidden. Set when a text conflict opens; cleared by Escape or the
    /// header's close button.
    full_window: bool,
    /// The optional read-only base column between A and B.
    show_base: bool,
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
        self.merge_tool_error = None;
        self.full_window = false;
    }

    /// Whether the editor is using the whole view. A load that no longer has
    /// an editor (binary, too large, resolved) also leaves the mode, so a stale
    /// flag cannot keep the sidebar hidden.
    pub(crate) fn is_full_window(&self) -> bool {
        self.full_window
            && self
                .loaded
                .as_ref()
                .is_some_and(|loaded| loaded.editor.is_some())
    }

    /// Leaves the full-window editor; returns whether it was in it.
    pub(crate) fn exit_full_window(&mut self) -> bool {
        if !self.is_full_window() {
            return false;
        }
        self.full_window = false;
        true
    }

    pub(crate) fn toggle_base_column(&mut self) {
        self.show_base = !self.show_base;
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
            self.merge_tool_error = None;
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
        self.merge_tool_error = None;
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
                let editable = editor.is_some();
                self.loaded = Some(Loaded { path, data, editor });
                self.error = None;
                self.full_window = editable;
                // A new path starts at the top of the panes.
                self.merge_scroll = UniformListScrollHandle::new();
                if let Some(editor) = self
                    .loaded
                    .as_ref()
                    .and_then(|loaded| loaded.editor.as_ref())
                {
                    refresh_decorations(editor, cx);
                }
            }
            Err(error) => {
                self.loaded = None;
                self.error = Some(error.to_string());
                self.full_window = false;
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
        self.merge_tool_error = None;
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
            Some(error) if action == PendingAction::Mergetool => {
                self.merge_tool_error = Some(error.to_owned());
            }
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
    // the editor.
    let handle = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())?;
    let input = match handle.update(cx, |_, window, app| {
        app.new(|cx| {
            EditorState::new(window, cx)
                .default_value(text)
                .soft_wrap(false)
        })
    }) {
        Ok(input) => input,
        Err(error) => {
            log::warn!("could not create the conflict result editor: {error}");
            return None;
        }
    };
    let (ranges, gutters) = input.update(cx, |state, cx| {
        state.set_gutter_decorations_width(px(3.0), cx);
        let ranges = state.create_range_decorations_collection(Vec::new(), cx);
        let gutters = state.create_gutter_decorations_collection(Vec::new(), cx);
        (ranges, gutters)
    });
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
        ranges,
        gutters,
        _changes: changes,
    })
}

/// The fill and the gutter mark for one result line.
fn mark_colors(mark: LineMark, colors: &Colors) -> (Hsla, Hsla) {
    match mark {
        LineMark::Side(Side::A) => (colors.side_a_soft, colors.side_a),
        LineMark::Side(Side::B) => (colors.side_b_soft, colors.side_b),
        // A change git merged on its own keeps its side's hue at a low alpha,
        // so it reads as context next to the stronger hand-picked fills.
        LineMark::AutoMerged(Side::A) => {
            (colors.side_a_soft.opacity(0.45), colors.side_a.opacity(0.5))
        }
        LineMark::AutoMerged(Side::B) => {
            (colors.side_b_soft.opacity(0.45), colors.side_b.opacity(0.5))
        }
        LineMark::Unresolved => (colors.warning_soft, colors.warning),
    }
}

/// Repaints the result pane's conflict blocks and left markers.
///
/// Consecutive lines with the same origin share one decoration, so a resolved
/// conflict is one fill and one gutter mark per side rather than one per line.
fn refresh_decorations(editor: &MergeEditor, cx: &mut App) {
    let colors = Colors::from_theme(cx.theme(), Settings::get(cx));
    let text = editor.model.text();
    let ranges = line_ranges(&text);
    let marks = editor.model.result_marks();
    let mut fills = Vec::new();
    let mut gutters = Vec::new();
    let mut index = 0;
    while index < marks.len() {
        let Some(mark) = marks[index] else {
            index += 1;
            continue;
        };
        let start = index;
        while index + 1 < marks.len() && marks[index + 1] == Some(mark) {
            index += 1;
        }
        let Some(first) = ranges.get(start) else {
            break;
        };
        let Some(last) = ranges.get(index) else {
            break;
        };
        let range = first.start..last.end;
        let (fill, gutter) = mark_colors(mark, &colors);
        fills.push(
            RangeDecoration::new(range.clone())
                .with_style(RangeDecorationStyle::Fill)
                .with_color(fill),
        );
        gutters.push(GutterDecoration::new(range).with_color(gutter));
        index += 1;
    }
    editor.ranges.set(fills, cx);
    editor.gutters.set(gutters, cx);
}

/// Replaces the editor's text with the model's result and repaints the marks.
fn set_editor_text(editor: &MergeEditor, window: &mut Window, cx: &mut Context<RepoView>) {
    let input = editor.input.clone();
    let text = editor.model.text();
    input.update(cx, |state, cx| state.set_value(text, window, cx));
    refresh_decorations(editor, cx);
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
        let repo = session.reader();
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
                    let merge_tool = repo.merge_tool();
                    let worktree = repo.file_in_worktree(&read_path).ok().flatten();
                    Ok(ConflictData::new(
                        sides,
                        worktree.as_deref(),
                        &read_path,
                        kind,
                        style,
                        merge_tool,
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
        let Some(path) = self.worktree.selected_conflict().map(str::to_owned) else {
            return;
        };
        self.resolve_conflict_path(path, side, cx);
    }

    /// Resolves `path` using `side` and stages the result.
    pub(crate) fn resolve_conflict_path(
        &mut self,
        path: String,
        side: ConflictSide,
        cx: &mut Context<Self>,
    ) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let operation = Operation::ResolveConflict { path, side };
        let activity = ActivityKind::of(&operation);
        let id = session.run(operation);
        self.activity.begin(id, activity, cx);
        self.worktree.conflicts.start(id, PendingAction::Resolve);
        cx.notify();
    }

    /// Switches to the Working Tree view and opens `path` in the merge editor.
    pub(crate) fn open_conflict_in_merge_editor(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_worktree(window, cx);
        self.select_conflict_file(path, window, cx);
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
            // Re-applying the same choice still changes which one is
            // recorded, so repaint even when the text stayed the same.
            editor.model.apply(choice);
            set_editor_text(editor, window, cx);
            true
        };
        if changed {
            self.worktree.conflicts.marker_warning = None;
            self.notify_if_active(cx);
        }
    }

    /// Ticks or unticks one side of one conflict from its gutter checkbox.
    pub(crate) fn toggle_conflict_side(
        &mut self,
        conflict: usize,
        side: Side,
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
            editor.model.tick(conflict, side);
            set_editor_text(editor, window, cx);
            true
        };
        if changed {
            self.worktree.conflicts.marker_warning = None;
            self.notify_if_active(cx);
        }
    }

    /// The side header's checkbox: take `side` for every conflict, or clear it
    /// everywhere when it already covers the file.
    pub(crate) fn toggle_conflict_side_everywhere(
        &mut self,
        side: Side,
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
            editor.model.tick_all(side);
            set_editor_text(editor, window, cx);
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
            set_editor_text(editor, window, cx);
            true
        };
        if changed {
            self.worktree.conflicts.marker_warning = None;
            self.notify_if_active(cx);
        }
    }

    /// Shows or hides the base column between A and B.
    pub(crate) fn toggle_conflict_base(&mut self, cx: &mut Context<Self>) {
        self.worktree.conflicts.toggle_base_column();
        cx.notify();
    }

    /// Leaves the whole-view merge editor for the normal working-tree layout.
    pub(crate) fn exit_conflict_full_window(&mut self, cx: &mut Context<Self>) {
        if self.worktree.conflicts.exit_full_window() {
            cx.notify();
        }
    }

    /// Moves the counter to the next or previous conflict and scrolls both
    /// side panes and the result to it.
    pub(crate) fn move_conflict(&mut self, forward: bool, cx: &mut Context<Self>) {
        let (row, result_line, line_height) = {
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
            (
                loaded
                    .data
                    .conflict_rows
                    .get(editor.model.selected())
                    .copied(),
                editor.model.selected_result_line(),
                editor.input.read(cx).line_height(),
            )
        };
        self.scroll_merge_to(row, result_line, line_height, cx);
        self.notify_if_active(cx);
    }

    /// Selects the conflict a side-pane row belongs to.
    pub(crate) fn select_conflict(&mut self, conflict: usize, cx: &mut Context<Self>) {
        let (row, result_line, line_height) = {
            let Some(loaded) = self.worktree.conflicts.loaded.as_mut() else {
                return;
            };
            let Some(editor) = loaded.editor.as_mut() else {
                return;
            };
            if !editor.model.select(conflict) {
                return;
            }
            (
                loaded.data.conflict_rows.get(conflict).copied(),
                editor.model.selected_result_line(),
                editor.input.read(cx).line_height(),
            )
        };
        self.scroll_merge_to(row, result_line, line_height, cx);
        self.notify_if_active(cx);
    }

    /// Scrolls the side panes and the result pane to one conflict.
    fn scroll_merge_to(
        &mut self,
        row: Option<usize>,
        result_line: Option<usize>,
        line_height: Option<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = row {
            self.worktree
                .conflicts
                .merge_scroll
                .scroll_to_item(row, ScrollStrategy::Center);
        }
        if let Some(line) = result_line
            && let Some(height) = line_height
        {
            let line_height = f32::from(height);
            // The result pane has no scroll handle of ours to move; the
            // editor's own offset is the one that scrolls its viewport.
            if let Some(editor) = self
                .worktree
                .conflicts
                .loaded
                .as_ref()
                .and_then(|loaded| loaded.editor.as_ref())
            {
                editor.input.update(cx, |state, cx| {
                    state.set_scroll_offset(
                        Point {
                            x: px(0.0),
                            y: px(-(line as f32 * line_height)),
                        },
                        cx,
                    );
                });
            }
        }
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
            let changed = editor.model.apply_text(text);
            if changed {
                refresh_decorations(editor, cx);
            }
            changed
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
        let repo = session.reader();
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
        let activity = ActivityKind::of(&operation);
        let id = session.run(operation);
        self.activity.begin(id, activity, cx);
        self.worktree
            .conflicts
            .start(id, PendingAction::MarkResolved);
        cx.notify();
    }

    pub(crate) fn dismiss_marker_warning(&mut self, cx: &mut Context<Self>) {
        self.worktree.conflicts.marker_warning = None;
        cx.notify();
    }

    pub(crate) fn dismiss_merge_tool_error(&mut self, cx: &mut Context<Self>) {
        self.worktree.conflicts.merge_tool_error = None;
        cx.notify();
    }

    /// Takes one side of one hunk in a single click, the same result as ticking
    /// that side's checkbox but without clearing an existing pick on a second
    /// click.
    pub(crate) fn take_conflict_side(
        &mut self,
        conflict: usize,
        side: Side,
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
            editor.model.take(conflict, side);
            set_editor_text(editor, window, cx);
            true
        };
        if changed {
            self.worktree.conflicts.marker_warning = None;
            self.notify_if_active(cx);
        }
    }

    /// Applies a choice to one hunk, for the take button's both-sides entry.
    pub(crate) fn take_conflict_both(
        &mut self,
        conflict: usize,
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
            editor.model.apply_at(conflict, choice);
            set_editor_text(editor, window, cx);
            true
        };
        if changed {
            self.worktree.conflicts.marker_warning = None;
            self.notify_if_active(cx);
        }
    }

    pub(crate) fn open_merge_tool(&mut self, cx: &mut Context<Self>) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let Some(path) = self.worktree.selected_conflict().map(str::to_owned) else {
            return;
        };
        // The header disables the button without a configured tool; keep a
        // programmatic call from reaching git's fallback list as well.
        if !self
            .worktree
            .conflicts
            .data_for(&path)
            .is_some_and(|loaded| loaded.data.merge_tool.is_some())
        {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let id = session.run(Operation::OpenMergetool { path });
        self.activity.begin(id, ActivityKind::Other, cx);
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
    /// The wash behind unresolved result lines.
    warning_soft: Hsla,
    success: Hsla,
    /// Side A and B each keep one colour everywhere: the pane header, the
    /// conflict block on that side and the result line the side produced.
    side_a: Hsla,
    side_a_soft: Hsla,
    side_b: Hsla,
    side_b_soft: Hsla,
    /// The stronger tint of the selected conflict on each side.
    side_a_selected: Hsla,
    side_b_selected: Hsla,
    /// One distinct colour per conflict index, for the matching left rules
    /// that tie a hunk in the side panes to its counterpart in the result.
    lanes: [Hsla; LANES],
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
            warning_soft: design.tag_background,
            success: design.add_text,
            side_a: design.accent,
            side_a_soft: design.accent_soft,
            side_a_selected: design.selected,
            side_b: design.add_text,
            side_b_soft: design.add_background,
            side_b_selected: design.diff_added_word,
            lanes: design.lane,
            diff: DiffColors::from_design(&design),
            design,
        }
    }

    /// The strong colour of one side.
    fn side(&self, side: Side) -> Hsla {
        match side {
            Side::A => self.side_a,
            Side::B => self.side_b,
        }
    }

    /// The wash behind one side's conflict block in the side panes.
    fn side_tint(&self, side: Side, selected: bool) -> Hsla {
        match (side, selected) {
            (Side::A, true) => self.side_a_selected,
            (Side::A, false) => self.side_a_soft,
            (Side::B, true) => self.side_b_selected,
            (Side::B, false) => self.side_b_soft,
        }
    }

    /// The left rule of one conflict. The graph lane palette already carries
    /// distinct hues that work in both themes, so neighbouring hunks never
    /// share one and the rule stays cheap to paint.
    fn conflict_rule(&self, conflict: usize) -> Hsla {
        self.lanes[conflict % LANES]
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
    .h(colors.design.px(ROW_HEIGHT))
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
                .max_w(colors.design.px(120.0))
                .flex_none()
                .truncate()
                .text_size(colors.design.px(11.0))
                .text_color(colors.muted_foreground)
                .child(dir.to_owned()),
        )
    })
    .child(
        div()
            .flex_none()
            .text_size(colors.design.px(9.5))
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
        .h(colors.design.px(ROW_HEIGHT * visible as f32))
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
                            .text_size(colors.design.px(11.0))
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
        details: Option<&InProgressDetails>,
        images: &crate::image_diff::State,
        cx: &Context<RepoView>,
    ) -> AnyElement {
        let colors = Colors::from_theme(cx.theme(), Settings::get(cx));
        let style = DiffStyle::from_settings(cx, &colors.design.mono);
        let busy = snapshot.busy.is_some() || self.pending.is_some();
        let loaded = self.data_for(path);
        let editor = loaded.and_then(|loaded| loaded.editor.as_ref());
        let (side_a, side_b) = side_roles(snapshot.state, details, snapshot.head.as_ref());

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

        // An external tool can fail without touching the result the editor
        // holds, so its failure is a banner above the editor rather than an
        // error page in place of it.
        if let Some(error) = &self.merge_tool_error {
            column = column.child(
                crate::ui::banner(
                    crate::ui::BannerKind::Danger,
                    t!("conflict.merge_tool_failed").to_string(),
                    &colors.design,
                )
                .detail(one_line(error))
                .action(
                    crate::ui::button("merge-tool-dismiss", t!("common.close").to_string())
                        .size(crate::ui::ButtonSize::Tiny)
                        .on_click({
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.dismiss_merge_tool_error(cx));
                            }
                        }),
                ),
            );
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
            (None, Some(loaded)) => match (&loaded.editor, loaded.data.images.as_ref()) {
                (Some(editor), _) => editor_body(
                    loaded,
                    editor,
                    &entity,
                    &colors,
                    &style,
                    busy,
                    self.show_base,
                    self.merge_scroll.clone(),
                    (&side_a, &side_b),
                ),
                (None, Some(_)) => image_body(loaded, images, &entity, &colors, busy),
                (None, None) => whole_file_body(loaded, &entity, &colors, busy),
            },
        });

        if let Some(notice) = &self.notice {
            column = column.child(
                div()
                    .flex_none()
                    .px_3()
                    .h(colors.design.px(22.0))
                    .flex()
                    .items_center()
                    .text_size(colors.design.px(10.0))
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
            .child(self.pane_header(path, loaded, editor, &entity, &colors, busy))
            .child(column)
            .into_any_element()
    }

    /// The whole-view header: the warning, the file, the counter and the
    /// actions that used to live on a second toolbar row.
    #[allow(clippy::too_many_arguments)]
    fn pane_header(
        &self,
        path: &str,
        loaded: Option<&Loaded>,
        editor: Option<&MergeEditor>,
        entity: &Entity<RepoView>,
        colors: &Colors,
        busy: bool,
    ) -> AnyElement {
        let (name, dir) = style::path_parts(path);
        let ready = loaded.is_some() && !busy;
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .h(colors.design.px(40.0))
            .flex_none()
            .px_3p5()
            .bg(colors.background)
            .border_b_1()
            .border_color(colors.border)
            .text_size(colors.design.px(12.0))
            .child(
                gpui_component::Icon::from(IconName::TriangleAlert)
                    .w(colors.design.px(14.0))
                    .h(colors.design.px(14.0))
                    .text_color(colors.warning),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().min_w_0().truncate().child(name.to_owned()))
                    .when_some(dir, |line, dir| {
                        line.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(colors.design.px(11.5))
                                .text_color(colors.muted_foreground)
                                .child(dir.to_owned()),
                        )
                    })
                    .when_some(loaded, |line, loaded| {
                        line.child(
                            div()
                                .flex_none()
                                .text_size(colors.design.px(10.0))
                                .text_color(colors.warning)
                                .child(kind_label(loaded.data.kind)),
                        )
                    }),
            );

        if let Some(editor) = editor
            && let Some(counter) = editor.model.counter()
        {
            row = row
                .child(
                    div()
                        .flex_none()
                        .text_size(colors.design.px(11.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if editor.model.all_resolved() {
                            colors.success
                        } else {
                            colors.warning
                        })
                        .child(counter),
                )
                .child(
                    crate::ui::icon_button("conflict-previous", IconName::ChevronUp)
                        .size(crate::ui::ButtonSize::Tiny)
                        .ghost()
                        .disabled(busy || editor.model.conflict_count() < 2)
                        .tooltip(t!("conflict.previous").to_string())
                        .on_click({
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.move_conflict(false, cx));
                            }
                        }),
                )
                .child(
                    crate::ui::icon_button("conflict-next", IconName::ChevronDown)
                        .size(crate::ui::ButtonSize::Tiny)
                        .ghost()
                        .disabled(busy || editor.model.conflict_count() < 2)
                        .tooltip(t!("conflict.next").to_string())
                        .on_click({
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.move_conflict(true, cx));
                            }
                        }),
                );
        }

        if let Some(loaded) = loaded
            && loaded.data.text
        {
            row = row.child(crate::ui::status_badge(
                loaded.data.encoding.label(),
                crate::ui::BadgeTone::Neutral,
                &colors.design,
            ));
        }

        row = row.child(div().flex_none().w(colors.design.px(4.0)));

        if editor.is_some() {
            let base_label = if self.show_base {
                t!("conflict.hide_base").to_string()
            } else {
                t!("conflict.show_base").to_string()
            };
            row = row.child(
                crate::ui::button("conflict-show-base", base_label)
                    .size(crate::ui::ButtonSize::Tiny)
                    .ghost()
                    .selected(self.show_base)
                    .disabled(busy)
                    .on_click({
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| this.toggle_conflict_base(cx));
                        }
                    }),
            );
            row = row.child(conflict_menu(entity.clone(), colors, busy));
        }

        if let Some(loaded) = loaded {
            let (tool_ready, tool_hint) = merge_tool_state(loaded.data.merge_tool.as_ref());
            row = row
                .child(
                    crate::ui::button("conflict-mergetool", t!("conflict.merge_tool").to_string())
                        .size(crate::ui::ButtonSize::Tiny)
                        .ghost()
                        .disabled(!ready || !tool_ready)
                        .tooltip(tool_hint)
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
                        if editor.is_some() {
                            t!("conflict.save_resolved").to_string()
                        } else {
                            t!("conflict.mark_resolved").to_string()
                        },
                    )
                    .size(crate::ui::ButtonSize::Tiny)
                    .disabled(!ready)
                    .on_click({
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| this.mark_conflict_resolved(cx));
                        }
                    }),
                );
        }

        if self.is_full_window() {
            row = row.child(
                crate::ui::icon_button("conflict-close", IconName::X)
                    .size(crate::ui::ButtonSize::Tiny)
                    .ghost()
                    .tooltip(t!("conflict.close_editor").to_string())
                    .on_click({
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| this.exit_conflict_full_window(cx));
                        }
                    }),
            );
        }

        row.into_any_element()
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

/// The little menu that keeps the two whole-conflict actions off the header:
/// take the base, or put the conflict back to its markers.
fn conflict_menu(entity: Entity<RepoView>, colors: &Colors, busy: bool) -> AnyElement {
    let base_entity = entity.clone();
    let reset_entity = entity;
    crate::ui::context_menu(
        "conflict-menu",
        &colors.design,
        crate::ui::icon_button("conflict-menu-trigger", IconName::Ellipsis)
            .size(crate::ui::ButtonSize::Tiny)
            .ghost()
            .tooltip(t!("conflict.menu").to_string()),
        move |menu, _, _| {
            let base_entity = base_entity.clone();
            let base = PopupMenuItem::new(format!(
                "{} ({})",
                MergeChoice::Base.label(),
                MergeChoice::Base.shortcut()
            ))
            .disabled(busy)
            .on_click(move |_, window, cx| {
                base_entity.update(cx, |this, cx| {
                    this.apply_conflict_choice(MergeChoice::Base, window, cx)
                });
            });
            let reset_entity = reset_entity.clone();
            let reset = PopupMenuItem::new(format!("{} (alt-0)", t!("common.reset")))
                .disabled(busy)
                .on_click(move |_, window, cx| {
                    reset_entity.update(cx, |this, cx| this.reset_conflict(window, cx));
                });
            menu.item(base).item(reset)
        },
    )
    .into_any_element()
}

/// The body of a text conflict: the side panes over the editable result.
#[allow(clippy::too_many_arguments)]
fn editor_body(
    loaded: &Loaded,
    editor: &MergeEditor,
    entity: &Entity<RepoView>,
    colors: &Colors,
    style: &DiffStyle,
    busy: bool,
    show_base: bool,
    scroll: UniformListScrollHandle,
    roles: (&SideRole, &SideRole),
) -> AnyElement {
    let panes = side_panes(
        loaded, editor, entity, colors, style, show_base, scroll, roles,
    );
    let result = result_pane(editor, colors, style, busy);
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(panes)
        .child(result)
        .into_any_element()
}

/// A binary conflict that is not a picture: why there is no editor, plus the
/// whole-file choices.
fn whole_file_body(
    loaded: &Loaded,
    entity: &Entity<RepoView>,
    colors: &Colors,
    busy: bool,
) -> AnyElement {
    let data = &loaded.data;
    let message = if data.binary {
        t!("conflict.binary").to_string()
    } else if !data.text {
        t!("conflict.not_utf8").to_string()
    } else if !kind_has_editor(data.kind) {
        t!("conflict.whole_file").to_string()
    } else if data.too_large {
        t!("conflict.too_large").to_string()
    } else {
        t!("conflict.no_text_diff").to_string()
    };
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(whole_file_choices(data.kind, entity, colors, busy))
        .child(crate::ui::message(
            message,
            colors.muted_foreground,
            &colors.design,
        ))
        .child(
            div()
                .flex_none()
                .px_3()
                .pb_2()
                .text_size(colors.design.px(10.0))
                .text_color(colors.muted_foreground)
                .child(sizes_line(data)),
        )
        .into_any_element()
}

/// The stage pictures of a binary image conflict, labelled with the same A and
/// B colours as the text panes.
fn image_body(
    loaded: &Loaded,
    images: &crate::image_diff::State,
    entity: &Entity<RepoView>,
    colors: &Colors,
    busy: bool,
) -> AnyElement {
    let data = &loaded.data;
    let Some(stages) = &data.images else {
        return div().into_any_element();
    };
    let picture = |label: String, side: &ImageSide, color: Hsla| {
        // The top edge repeats the pane's colour so a glance at the pictures
        // says which side is which, like the text panes' tinted blocks.
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .border_t_2()
            .border_color(color)
            .child(crate::image_diff::labelled_picture(
                images,
                side,
                label,
                color,
                &colors.design,
                false,
            ))
            .into_any_element()
    };
    let mut row = div().flex().flex_1().min_h_0().w_full();
    if let Some(side) = stages.ours.as_ref() {
        row = row.child(picture(
            format!("A · {}", t!("conflict.ours_header")),
            side,
            colors.side_a,
        ));
    }
    if let Some(side) = stages.base.as_ref() {
        row = row.child(picture(
            t!("conflict.base_header").to_string(),
            side,
            colors.muted_foreground,
        ));
    }
    if let Some(side) = stages.theirs.as_ref() {
        row = row.child(picture(
            format!("B · {}", t!("conflict.theirs_header")),
            side,
            colors.side_b,
        ));
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(whole_file_choices(data.kind, entity, colors, busy))
        .child(row)
        .into_any_element()
}

/// Use ours / use theirs (or keep / delete) for a conflict without a text
/// editor.
fn whole_file_choices(
    kind: ConflictKind,
    entity: &Entity<RepoView>,
    colors: &Colors,
    busy: bool,
) -> AnyElement {
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .flex_none()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(colors.border);
    for (index, choice) in choices(kind).into_iter().enumerate() {
        let entity = entity.clone();
        row = row.child(
            crate::ui::button(("conflict-choice", index), choice.label)
                .size(crate::ui::ButtonSize::Tiny)
                .disabled(busy)
                .when(choice.destructive, crate::ui::UiButton::danger)
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.use_conflict_side(choice.side, cx));
                }),
        );
    }
    row.into_any_element()
}

/// A and B, one list row per line pair, so the sides always show the same
/// scroll position. The optional base column sits between them.
#[allow(clippy::too_many_arguments)]
fn side_panes(
    loaded: &Loaded,
    editor: &MergeEditor,
    entity: &Entity<RepoView>,
    colors: &Colors,
    style: &DiffStyle,
    show_base: bool,
    scroll: UniformListScrollHandle,
    roles: (&SideRole, &SideRole),
) -> AnyElement {
    let data = &loaded.data;
    let rows = data.rows.clone();
    let conflict_rows = data.conflict_rows.clone();
    let ours_lines = data.ours_lines.clone();
    let theirs_lines = data.theirs_lines.clone();
    let base_lines = data.base_lines.clone();
    let ours_highlights = data.ours_highlights.clone();
    let theirs_highlights = data.theirs_highlights.clone();
    let base_highlights = data.base_highlights.clone();
    let selected = editor.model.selected();
    let ticks: Rc<Vec<Ticked>> = Rc::new(
        (0..editor.model.conflict_count())
            .map(|conflict| editor.model.ticked(conflict).unwrap_or_default())
            .collect(),
    );
    let row_height = style.row_height();
    let font_size = style.font_size();
    let mono = style.mono().clone();
    let diff = colors.diff;
    let design = colors.design.clone();
    let row_colors = colors.clone();
    let row_style = style.clone();
    let row_entity = entity.clone();
    let header_entity = entity.clone();

    let list = uniform_list(
        "conflict-merge-rows",
        rows.len(),
        move |range, _window, _cx| {
            range
                .map(|index| {
                    let row = rows[index];
                    let first_row = row
                        .conflict
                        .is_some_and(|conflict| conflict_rows.get(conflict) == Some(&index));
                    let left = if row.kind == SegmentKind::Conflict {
                        Some(row_colors.side_tint(Side::A, row.conflict == Some(selected)))
                    } else if row.kind == SegmentKind::Ours {
                        Some(row_colors.side_a_soft)
                    } else {
                        None
                    };
                    let right = if row.kind == SegmentKind::Conflict {
                        Some(row_colors.side_tint(Side::B, row.conflict == Some(selected)))
                    } else if row.kind == SegmentKind::Theirs {
                        Some(row_colors.side_b_soft)
                    } else {
                        None
                    };
                    // A matching rule on both panes ties a hunk together: the
                    // result pane is a separate view, so a painted band between
                    // them would have to sync two scroll offsets to stay right.
                    let rule = row.conflict.map(|conflict| {
                        (
                            row_colors.conflict_rule(conflict),
                            row.conflict == Some(selected),
                        )
                    });
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

                    let checkbox = |side: Side| {
                        row.conflict.map(|conflict| {
                            let ticked = ticks.get(conflict).copied().unwrap_or_default();
                            tick_box(
                                &row_entity,
                                conflict,
                                side,
                                ticked,
                                first_row,
                                row_colors.side(side),
                                &row_colors.design,
                            )
                        })
                    };
                    let actions = |side: Side| {
                        row.conflict
                            .filter(|_| first_row)
                            .map(|conflict| take_actions(&row_entity, conflict, side, &row_colors))
                    };
                    let a_box = checkbox(Side::A);
                    let b_box = checkbox(Side::B);
                    let a_actions = actions(Side::A);
                    let b_actions = actions(Side::B);
                    element = element.child(side_cell(
                        row.ours,
                        &ours_lines,
                        &ours_highlights,
                        left,
                        &diff,
                        &row_style,
                        Separator::Right,
                        &row_colors.design,
                        a_box,
                        rule,
                        a_actions,
                    ));
                    if show_base {
                        element = element.child(side_cell(
                            row.base,
                            &base_lines,
                            &base_highlights,
                            None,
                            &diff,
                            &row_style,
                            Separator::Right,
                            &row_colors.design,
                            None,
                            None,
                            None,
                        ));
                    }
                    element
                        .child(side_cell(
                            row.theirs,
                            &theirs_lines,
                            &theirs_highlights,
                            right,
                            &diff,
                            &row_style,
                            Separator::None,
                            &row_colors.design,
                            b_box,
                            rule,
                            b_actions,
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

    let all_a = editor.model.all_ticked(Side::A);
    let all_b = editor.model.all_ticked(Side::B);
    let header_a = side_header(
        Side::A,
        roles.0,
        all_a,
        &header_entity,
        &design,
        colors,
        show_base,
    );
    let header_base = div()
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .h(design.px(28.0))
        .border_l_1()
        .border_color(design.border)
        .child(pane_header(
            t!("conflict.base_header").to_string(),
            &design,
            None,
        ));
    let header_b = side_header(
        Side::B,
        roles.1,
        all_b,
        &header_entity,
        &design,
        colors,
        false,
    );

    let mut headings = div().flex().flex_row().flex_none().w_full().child(header_a);
    if show_base {
        headings = headings.child(header_base);
    }
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .border_b_1()
        .border_color(design.border)
        .child(headings.child(header_b))
        .child(list)
        .into_any_element()
}

/// One side's header: its checkbox, `A · sha · branch` and the part it plays.
fn side_header(
    side: Side,
    role: &SideRole,
    all_ticked: bool,
    entity: &Entity<RepoView>,
    design: &Design,
    colors: &Colors,
    with_border: bool,
) -> AnyElement {
    let color = colors.side(side);
    let tint = colors.side_tint(side, false);
    let check_entity = entity.clone();
    let tooltip = format!(
        "{} ({})",
        if side == Side::A {
            MergeChoice::Ours.label()
        } else {
            MergeChoice::Theirs.label()
        },
        if side == Side::A {
            MergeChoice::Ours.shortcut()
        } else {
            MergeChoice::Theirs.shortcut()
        }
    );
    let checkbox = crate::ui::checkbox(("conflict-all", usize::from(side == Side::B)), all_ticked)
        .tooltip(tooltip)
        .on_click(move |_, window, cx| {
            check_entity.update(cx, |this, cx| {
                this.toggle_conflict_side_everywhere(side, window, cx)
            });
        });
    let mut header = pane_header("", design, Some(checkbox));
    header = header
        .bg(tint)
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(color)
                .child(role.label(side)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(design.muted)
                .child(role.role.clone()),
        );
    if with_border {
        header = header.border_r_1().border_color(design.border);
    }
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .h(design.px(28.0))
        .child(header)
        .into_any_element()
}

fn pane_header(
    title: impl Into<gpui::SharedString>,
    design: &Design,
    checkbox: Option<Checkbox>,
) -> gpui::Div {
    let mut header = div()
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap_1p5()
        .h_full()
        .px_2()
        .bg(design.field)
        .text_size(design.px(9.5))
        .text_color(design.muted);
    if let Some(checkbox) = checkbox {
        header = header.child(checkbox);
    }
    header.child(title.into())
}

/// The gutter checkbox for one conflict. Only the first row of a conflict
/// carries one; the rest of the block leaves the gutter empty.
#[allow(clippy::too_many_arguments)]
fn tick_box(
    entity: &Entity<RepoView>,
    conflict: usize,
    side: Side,
    ticked: Ticked,
    visible: bool,
    color: Hsla,
    design: &Design,
) -> AnyElement {
    let mut gutter = div()
        .flex_none()
        .w(design.px(22.0))
        .h_full()
        .flex()
        .items_center()
        .justify_center();
    if visible {
        let entity = entity.clone();
        let tooltip = format!("{} · {}", side.letter(), t!("conflict.tick_side"));
        gutter = gutter.child(crate::ui::gutter_checkbox(
            ("conflict-tick", conflict * 2 + usize::from(side == Side::B)),
            ticked.has(side),
            color,
            design,
            tooltip,
            move |_, window, cx| {
                entity.update(cx, |this, cx| {
                    this.toggle_conflict_side(conflict, side, window, cx)
                });
            },
        ));
    }
    gutter.into_any_element()
}

/// The one-click actions on a conflict block's first row.
///
/// The take button applies that side to the result; the small menu next to it
/// offers both sides at once, where there is room for a second control.
fn take_actions(
    entity: &Entity<RepoView>,
    conflict: usize,
    side: Side,
    colors: &Colors,
) -> AnyElement {
    let take_entity = entity.clone();
    let take = crate::ui::button(
        ("conflict-take", conflict * 2 + usize::from(side == Side::B)),
        t!("conflict.take_side").to_string(),
    )
    .icon(IconName::ArrowLeft)
    .size(crate::ui::ButtonSize::Tiny)
    .ghost()
    .tooltip(format!(
        "{} · {}",
        side.letter(),
        t!("conflict.take_side_tooltip")
    ))
    .on_click(move |_, window, cx| {
        take_entity.update(cx, |this, cx| {
            this.take_conflict_side(conflict, side, window, cx)
        });
    });

    let mut row = div()
        .flex()
        .flex_none()
        .items_center()
        .gap_0p5()
        .child(take);
    if side == Side::A {
        let both_entity = entity.clone();
        row = row.child(crate::ui::context_menu(
            ("conflict-take-menu", conflict),
            &colors.design,
            crate::ui::icon_button(("conflict-take-menu-trigger", conflict), IconName::Ellipsis)
                .size(crate::ui::ButtonSize::Tiny)
                .ghost()
                .tooltip(t!("conflict.take_both_menu").to_string()),
            move |menu, _, _| {
                let entity = both_entity.clone();
                let both = PopupMenuItem::new(t!("conflict.take_both").to_string()).on_click(
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| {
                            this.take_conflict_both(
                                conflict,
                                MergeChoice::OursThenTheirs,
                                window,
                                cx,
                            )
                        });
                    },
                );
                menu.item(both)
            },
        ));
    }
    row.into_any_element()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Separator {
    Right,
    None,
}

/// One line of one stage, with its gutter checkbox and the tint of its side.
#[allow(clippy::too_many_arguments)]
fn side_cell(
    line: Option<u32>,
    lines: &[String],
    highlights: &[LineHighlights],
    tint: Option<Hsla>,
    diff: &DiffColors,
    style: &DiffStyle,
    separator: Separator,
    design: &Design,
    gutter: Option<AnyElement>,
    rule: Option<(Hsla, bool)>,
    actions: Option<AnyElement>,
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
        .when_some(rule, |cell, (color, selected)| {
            // The selected conflict keeps its rule at full strength; the others
            // stay visible but step back.
            cell.border_l_2()
                .border_color(color.opacity(if selected { 1.0 } else { 0.55 }))
        })
        .when(separator == Separator::Right, |cell| {
            cell.border_r_1().border_color(design.border)
        });
    let cell = match gutter {
        Some(gutter) => cell.child(gutter),
        None => cell,
    };
    let cell = match line {
        Some(number) => {
            let text = lines
                .get(number as usize - 1)
                .map(String::as_str)
                .unwrap_or_default();
            let spans = highlights.get(number as usize - 1);
            cell.child(
                div()
                    .flex_none()
                    .w(style.px(38.0))
                    .pr_1()
                    .text_size(px(style.font_size() - style.scale(2.0)))
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
                .text_size(px(style.font_size() - style.scale(2.0)))
                .text_color(diff.gutter)
                .child(""),
        ),
    };
    match actions {
        Some(actions) => cell
            .relative()
            .child(
                div()
                    .absolute()
                    .right_1()
                    .top_0()
                    .bottom_0()
                    .flex()
                    .items_center()
                    .rounded(design.px(3.0))
                    .bg(design.background)
                    .child(actions),
            )
            .into_any_element(),
        None => cell.into_any_element(),
    }
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
    let mut legend = div().flex().items_center().gap_1();
    for (side, label) in [(Side::A, "A"), (Side::B, "B")] {
        legend = legend.child(
            div()
                .size(colors.design.px(7.0))
                .rounded_full()
                .bg(colors.side(side)),
        );
        legend = legend.child(div().text_size(colors.design.px(9.0)).child(label));
    }
    legend = legend.child(
        div()
            .size(colors.design.px(7.0))
            .rounded_full()
            .bg(colors.warning),
    );
    legend = legend.child(
        div()
            .text_size(colors.design.px(9.0))
            .child(t!("conflict.unresolved").to_string()),
    );
    // The faint fill on a change git merged by itself needs a name of its own;
    // the chip carries the tooltip that explains the tint.
    legend = legend.child(
        div()
            .id("conflict-auto-merged")
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .rounded(colors.design.px(3.0))
            .bg(colors.side_a_soft.opacity(0.45))
            .child(
                div()
                    .size(colors.design.px(7.0))
                    .rounded_full()
                    .bg(colors.side_a.opacity(0.5)),
            )
            .child(
                div()
                    .text_size(colors.design.px(9.0))
                    .child(t!("conflict.auto_merged_legend").to_string()),
            )
            .tooltip(move |window, cx| {
                Tooltip::new(t!("conflict.auto_merged").to_string()).build(window, cx)
            }),
    );

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
                .h(colors.design.px(22.0))
                .flex_none()
                .px_2()
                .bg(colors.muted.opacity(0.4))
                .text_size(colors.design.px(9.5))
                .text_color(colors.muted_foreground)
                .child(t!("conflict.result").to_string())
                .child(div().flex_1().min_w_0().truncate().child(status))
                .child(legend)
                .child(t!("conflict.edit_manually").to_string()),
        )
        .child(
            div().flex_1().min_h_0().w_full().p_1p5().child(
                Editor::new(&editor.input)
                    .h(relative(1.0))
                    .w_full()
                    .bordered(false)
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
        ConflictData::new(
            sides,
            None,
            "src/lib.rs",
            Some(kind),
            ConflictStyle::Merge,
            None,
        )
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
    fn the_merge_tool_button_names_the_configured_tool_or_what_is_missing() {
        let (ready, tooltip) = merge_tool_state(None);
        assert!(!ready, "without a configured tool the button is disabled");
        assert_eq!(tooltip, t!("conflict.merge_tool_missing").to_string());

        let tool = MergeTool {
            name: "meld".to_owned(),
            gui: false,
        };
        let (ready, tooltip) = merge_tool_state(Some(&tool));
        assert!(ready);
        assert_eq!(
            tooltip,
            t!("conflict.merge_tool_configured", tool = tool.name.clone()).to_string()
        );
        assert!(tooltip.contains("meld"), "the tooltip names the tool");
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
        let ours = highlight_file(
            "src/lib.rs",
            ours_lines.as_deref().unwrap_or_default(),
            true,
        );
        let theirs = highlight_file(
            "src/lib.rs",
            theirs_lines.as_deref().unwrap_or_default(),
            true,
        );
        assert_eq!(ours.len(), 2, "one range per line, plus the empty tail");
        assert!(ours.iter().any(|spans| !spans.is_empty()));
        assert!(theirs.iter().any(|spans| !spans.is_empty()));

        assert!(highlight_file("notes.unknown", &[], true).is_empty());
    }

    fn oid(hex: &str) -> Oid {
        format!("{hex:0<40}").parse().expect("a full object id")
    }

    fn commit(short_id: &str, summary: &str) -> InProgressCommit {
        InProgressCommit {
            id: None,
            short_id: short_id.to_owned(),
            summary: Some(summary.to_owned()),
        }
    }

    fn branch(name: &str, hex: &str) -> HeadInfo {
        HeadInfo::Branch {
            short_name: name.to_owned(),
            target: Some(oid(hex)),
        }
    }

    #[test]
    fn a_merge_names_our_branch_and_the_source_being_merged() {
        let merge = InProgressDetails::Merge {
            heads: vec![commit("60a3858", "Topic work")],
            source: Some("feature".to_owned()),
        };
        let (a, b) = side_roles(
            RepoState::Merging,
            Some(&merge),
            Some(&branch("main", "a1b2c3d000000000000000000000000000000000")),
        );
        assert_eq!(a.label(Side::A), "A · a1b2c3d · main");
        assert_eq!(a.role, t!("conflict.role.ours").to_string());
        assert_eq!(b.label(Side::B), "B · 60a3858 · feature");
        assert_eq!(b.role, t!("conflict.role.theirs").to_string());
    }

    #[test]
    fn a_rebase_names_the_upstream_and_the_commit_being_applied() {
        let rebase = InProgressDetails::Rebase {
            branch: Some("feature".to_owned()),
            onto: Some(strata_core::repo::RebaseOnto {
                id: None,
                short_id: "a1b2c3d".to_owned(),
                ref_name: Some("main".to_owned()),
            }),
            step: Some(2),
            total_steps: Some(5),
            current_commit: Some(commit("9f8e7d6", "Fix parser")),
        };
        let (a, b) = side_roles(
            RepoState::Rebasing,
            Some(&rebase),
            Some(&branch(
                "feature",
                "9f8e7d600000000000000000000000000000000",
            )),
        );
        assert_eq!(a.label(Side::A), "A · a1b2c3d · main");
        assert_eq!(a.role, t!("conflict.role.rebase_onto").to_string());
        assert_eq!(b.label(Side::B), "B · 9f8e7d6 · Fix parser");
        assert_eq!(b.role, t!("conflict.role.rebase_applying").to_string());
        assert_ne!(
            a.role,
            t!("conflict.role.ours").to_string(),
            "a rebase's sides are not simply ours and theirs"
        );
    }

    #[test]
    fn a_rebase_without_details_still_names_both_roles() {
        let (a, b) = side_roles(RepoState::Rebasing, None, None);
        assert_eq!(a.role, t!("conflict.role.rebase_onto").to_string());
        assert_eq!(b.role, t!("conflict.role.rebase_applying").to_string());
        assert_eq!(a.label(Side::A), "A");
    }

    #[test]
    fn cherry_pick_and_revert_name_the_commit_they_are_working_with() {
        let head = branch("main", "a1b2c3d000000000000000000000000000000000");
        let cherry = InProgressDetails::CherryPick {
            commit: Some(commit("9f8e7d6", "Fix parser")),
            remaining: Some(1),
        };
        let (a, b) = side_roles(RepoState::CherryPicking, Some(&cherry), Some(&head));
        assert_eq!(a.role, t!("conflict.role.current").to_string());
        assert_eq!(a.label(Side::A), "A · a1b2c3d · main");
        assert_eq!(b.label(Side::B), "B · 9f8e7d6 · Fix parser");
        assert_eq!(b.role, t!("conflict.role.cherry_pick").to_string());

        let revert = InProgressDetails::Revert {
            commit: Some(commit("9f8e7d6", "Bad commit")),
            remaining: None,
        };
        let (_, b) = side_roles(RepoState::Reverting, Some(&revert), Some(&head));
        assert_eq!(b.role, t!("conflict.role.revert").to_string());
        assert_eq!(b.label(Side::B), "B · 9f8e7d6 · Bad commit");
    }

    #[test]
    fn only_raster_paths_become_image_conflicts() {
        let bytes = ConflictSides {
            base: Some(vec![0, 1, 2]),
            ours: Some(vec![0, 3, 4]),
            theirs: Some(vec![0, 5, 6]),
        };
        let image = conflict_images("assets/logo.png", &bytes).expect("a png is an image");
        assert!(image.ours.is_some() && image.theirs.is_some() && image.base.is_some());
        assert!(conflict_images("src/lib.rs", &bytes).is_none());
        assert!(
            conflict_images("assets/drawing.svg", &bytes).is_none(),
            "SVG stays text and keeps the editor"
        );

        let deleted = ConflictSides {
            base: None,
            ours: None,
            theirs: Some(vec![0, 5, 6]),
        };
        let image = conflict_images("logo.jpg", &deleted).expect("one side is enough");
        assert!(image.ours.is_none());
        assert!(image.theirs.is_some());
    }

    #[test]
    fn a_binary_image_conflict_loads_pictures_instead_of_a_text_editor() {
        let data = ConflictData::new(
            ConflictSides {
                base: Some(vec![0, 1, 2]),
                ours: Some(vec![0, 3, 4]),
                theirs: Some(vec![0, 5, 6]),
            },
            None,
            "assets/logo.png",
            Some(ConflictKind::BothModified),
            ConflictStyle::Merge,
            None,
        );
        assert!(data.binary);
        assert!(data.images.is_some());
        assert!(data.model.is_none());
    }

    #[test]
    fn a_legacy_text_conflict_carries_its_encoding_label() {
        let shift_jis =
            TextEncoding::from_label("Shift_JIS").expect("Shift_JIS is a known encoding");
        let data = ConflictData::new(
            ConflictSides {
                base: Some(shift_jis.encode("設定\n")),
                ours: Some(shift_jis.encode("設定\n速度は速い\n")),
                theirs: Some(shift_jis.encode("設定\n速度は遅い\n")),
            },
            None,
            "設定.txt",
            Some(ConflictKind::BothModified),
            ConflictStyle::Merge,
            None,
        );
        assert!(data.text);
        assert_eq!(data.encoding.label(), "Shift_JIS");
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
