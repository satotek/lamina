//! Merge conflict resolution: the conflicts list, the per-file conflict view and
//! the actionable merge banner.
//!
//! The session is the only route to git. The three sides of a conflicted path
//! come from `Repo::conflict_sides` (a gix read, run on the background
//! executor) and every write is an [`Operation`]; nothing here spawns git.
//!
//! Per-hunk resolution is deliberately absent: it needs an operation that writes
//! a merged file and stages it, which core does not have. Until it does, the
//! whole-file operations are the only choices offered.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, Hsla, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, actions, div, px, uniform_list,
};
use gpui_component::ActiveTheme as _;
use gpui_component::Theme;
use gpui_component::WindowExt as _;
use gpui_component::button::ButtonVariant;
use strata_core::diff::{DiffOptions, LineKind, diff_text};
use strata_core::session::{ConflictSide, Operation, OperationId, RepoSnapshot};
use strata_core::{ConflictEntry, ConflictKind, ConflictSides, RepoState};

use crate::repo_view::RepoView;
use crate::scrollbar::WithScrollbar as _;
use crate::worktree_panel::WORKTREE_PANEL;

actions!(strata_conflicts, [UseOurs, UseTheirs, MarkFileResolved]);

// `KeyBinding::new` parses without an OS, so a platform modifier is chosen at
// compile time instead of using `secondary-`. The keys only do something while
// a conflicted file is selected, so single letters are safe here; the commit
// box is outside the panel's key context and never sees them.
pub fn bind(cx: &mut App) {
    let scope = Some(WORKTREE_PANEL);
    cx.bind_keys([
        KeyBinding::new("o", UseOurs, scope),
        KeyBinding::new("t", UseTheirs, scope),
        KeyBinding::new("m", MarkFileResolved, scope),
    ]);
}

/// Height of one conflicts-list row, matching the other file lists.
const ROW_HEIGHT: f32 = 20.0;
/// A conflicts list stops growing here and scrolls instead.
const MAX_LIST_ROWS: usize = 8;
/// Regions past this are not element-built; the whole file is available through
/// the merge tool, and a conflict this fragmented is better resolved there.
const MAX_REGIONS: usize = 200;
/// Same probe length core's binary detector uses.
const BINARY_PROBE: usize = 8000;

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested at the bottom).
// ---------------------------------------------------------------------------

/// The kind in the words `git status` prints next to an unmerged path.
pub(crate) fn kind_label(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BothModified => "both modified",
        ConflictKind::BothAdded => "both added",
        ConflictKind::BothDeleted => "both deleted",
        ConflictKind::AddedByUs => "added by us",
        ConflictKind::AddedByThem => "added by them",
        ConflictKind::DeletedByUs => "deleted by us",
        ConflictKind::DeletedByThem => "deleted by them",
    }
}

/// One whole-file choice the resolution offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Choice {
    pub label: &'static str,
    pub side: ConflictSide,
    /// The choice removes the path, so it is painted as a destructive action.
    pub destructive: bool,
}

/// The whole-file choices a conflict kind allows.
///
/// A side that deleted the path has no version to check out; core resolves that
/// by removing the path from the index, which is exactly "delete file". So every
/// kind except "both deleted" (which has no sides at all) offers two choices.
pub(crate) fn choices(kind: ConflictKind) -> Vec<Choice> {
    let ours = Choice {
        label: "Use ours",
        side: ConflictSide::Ours,
        destructive: false,
    };
    let theirs = Choice {
        label: "Use theirs",
        side: ConflictSide::Theirs,
        destructive: false,
    };
    let keep_ours = Choice {
        label: "Keep file",
        side: ConflictSide::Ours,
        destructive: false,
    };
    let keep_theirs = Choice {
        label: "Keep file",
        side: ConflictSide::Theirs,
        destructive: false,
    };
    let delete_theirs = Choice {
        label: "Delete file",
        side: ConflictSide::Theirs,
        destructive: true,
    };
    let delete_ours = Choice {
        label: "Delete file",
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

/// One region of a text conflict: the base lines the sides disagree about and
/// what each side puts there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Region {
    /// 1-based first base line; for an insertion this is the line it follows,
    /// and 0 means "before the first line".
    pub base_start: u32,
    pub base: Vec<String>,
    pub ours: Vec<String>,
    pub theirs: Vec<String>,
    pub ours_changed: bool,
    pub theirs_changed: bool,
}

impl Region {
    fn base_label(&self) -> String {
        match self.base.len() {
            0 if self.base_start == 0 => "before the first base line".to_owned(),
            0 => format!("after base line {}", self.base_start),
            1 => format!("base line {}", self.base_start),
            count => format!(
                "base lines {}–{}",
                self.base_start,
                self.base_start + count as u32 - 1
            ),
        }
    }
}

/// A changed run of base lines and the text one side puts in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    /// 0-based, end-exclusive base line range.
    start: usize,
    end: usize,
    /// The side's replacement text, empty for a deletion.
    lines: Vec<String>,
}

/// `None` when either side is binary, which has no line diff to show.
fn side_changes(base: &[u8], side: Option<&[u8]>) -> Option<Vec<Change>> {
    let diff = diff_text(
        base,
        side.unwrap_or_default(),
        &DiffOptions { context_lines: 0 },
    );
    if diff.binary {
        return None;
    }
    Some(
        diff.hunks
            .iter()
            .map(|hunk| {
                // A header of length 0 points at the insertion slot rather than
                // at a line, which is how core defines `header_start`.
                let start = if hunk.old_len == 0 {
                    hunk.old_start as usize
                } else {
                    hunk.old_start as usize - 1
                };
                Change {
                    start,
                    end: start + hunk.old_len as usize,
                    lines: hunk
                        .lines
                        .iter()
                        .filter(|line| line.kind == LineKind::Added)
                        .map(|line| line.content.clone())
                        .collect(),
                }
            })
            .collect(),
    )
}

/// Splits text into display lines; a trailing newline does not add an empty one.
fn text_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect();
    if text.is_empty() || text.ends_with('\n') {
        lines.pop();
    }
    lines
}

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

/// The base lines each side changed, grouped into regions.
///
/// Each side is diffed against the base with no context, and overlapping
/// changes from the two diffs are merged into one region so a base line never
/// appears in two regions. A side that did not touch a region contributes the
/// base text, which is what its version of the file holds there.
fn regions_from_sides(sides: &ConflictSides) -> Vec<Region> {
    let base_bytes = sides.base.as_deref().unwrap_or_default();
    let base_lines = text_lines(base_bytes);
    let ours = side_changes(base_bytes, sides.ours.as_deref()).unwrap_or_default();
    let theirs = side_changes(base_bytes, sides.theirs.as_deref()).unwrap_or_default();

    let mut bounds: Vec<(usize, usize)> = ours
        .iter()
        .chain(theirs.iter())
        .map(|change| (change.start, change.end))
        .collect();
    bounds.sort_unstable();

    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(bounds.len());
    for (start, end) in bounds {
        match merged.last_mut() {
            // Strictly overlapping runs merge. A zero-length run is an insertion:
            // it belongs to the region it touches so its text is shown next to
            // the change it sits beside.
            Some(last)
                if start < last.1 || (start == last.1 && (start == end || last.0 == last.1)) =>
            {
                last.1 = last.1.max(end);
            }
            _ => merged.push((start, end)),
        }
    }

    merged
        .into_iter()
        .map(|(start, end)| {
            let (ours_lines, ours_changed) = side_text(&base_lines, &ours, start, end);
            let (theirs_lines, theirs_changed) = side_text(&base_lines, &theirs, start, end);
            Region {
                base_start: if end > start {
                    start as u32 + 1
                } else {
                    start as u32
                },
                base: base_lines[start..end].to_vec(),
                ours: ours_lines,
                theirs: theirs_lines,
                ours_changed,
                theirs_changed,
            }
        })
        .collect()
}

/// One side's text for `[start, end)`: the base lines with that side's changes
/// spliced in, and whether it changed anything there.
fn side_text(base: &[String], changes: &[Change], start: usize, end: usize) -> (Vec<String>, bool) {
    let mut out = Vec::with_capacity(end.saturating_sub(start));
    let mut changed = false;
    let mut pos = start;
    for change in changes {
        // Regions are unions of whole changes, so this only filters changes that
        // belong to a different region.
        if change.start < start || change.end > end {
            continue;
        }
        if change.start > pos {
            out.extend_from_slice(&base[pos..change.start]);
        }
        out.extend(change.lines.iter().cloned());
        changed = true;
        pos = change.end;
    }
    if pos < end {
        out.extend_from_slice(&base[pos..end]);
    }
    (out, changed)
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
            "deleted".to_owned()
        }
    };
    format!(
        "ours {} · theirs {} · base {}",
        side(data.ours_present, data.ours_bytes),
        side(data.theirs_present, data.theirs_bytes),
        side(data.base_present, data.base_bytes)
    )
}

/// A banner that needs a live snapshot and the pending state of this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BannerState {
    pub text: String,
    pub can_continue: bool,
    pub can_abort: bool,
}

fn banner_state(state: RepoState, conflicts: usize, busy: bool) -> Option<BannerState> {
    let count = match conflicts {
        1 => "1 conflict".to_owned(),
        count => format!("{count} conflicts"),
    };
    let resolved = conflicts == 0;
    let banner = match state {
        RepoState::Normal => return None,
        RepoState::Merging => BannerState {
            text: if resolved {
                "Merge in progress — all conflicts resolved. Continue commits the merge with git's prepared message.".to_owned()
            } else {
                format!("Merge in progress — {count} to resolve.")
            },
            can_continue: resolved && !busy,
            can_abort: !busy,
        },
        // Core has merge continue/abort only; conflict resolution itself works
        // for the other operations because it goes through the index stages.
        RepoState::Rebasing => BannerState {
            text: format!(
                "Rebase in progress — {count} to resolve. Continuing or aborting a rebase is not supported yet."
            ),
            can_continue: false,
            can_abort: false,
        },
        RepoState::CherryPicking => BannerState {
            text: format!(
                "Cherry-pick in progress — {count} to resolve. Continuing or aborting a cherry-pick is not supported yet."
            ),
            can_continue: false,
            can_abort: false,
        },
        RepoState::Reverting => BannerState {
            text: format!(
                "Revert in progress — {count} to resolve. Continuing or aborting a revert is not supported yet."
            ),
            can_continue: false,
            can_abort: false,
        },
        RepoState::Bisecting => BannerState {
            text: "Bisect in progress.".to_owned(),
            can_continue: false,
            can_abort: false,
        },
        RepoState::ApplyingPatches => BannerState {
            text: format!("Applying patches — {count} to resolve."),
            can_continue: false,
            can_abort: false,
        },
    };
    Some(banner)
}

// ---------------------------------------------------------------------------
// Loaded conflict data.
// ---------------------------------------------------------------------------

/// Everything the conflict view shows for one path, computed off the UI thread.
pub(crate) struct ConflictData {
    pub ours_present: bool,
    pub theirs_present: bool,
    pub base_present: bool,
    pub ours_bytes: usize,
    pub theirs_bytes: usize,
    pub base_bytes: usize,
    pub worktree_markers: bool,
    pub binary: bool,
    pub regions: Vec<Region>,
}

impl ConflictData {
    fn new(sides: ConflictSides, worktree: Option<&[u8]>) -> Self {
        let binary = [
            sides.base.as_deref(),
            sides.ours.as_deref(),
            sides.theirs.as_deref(),
            worktree,
        ]
        .into_iter()
        .flatten()
        .any(is_binary);
        Self {
            ours_present: sides.ours.is_some(),
            theirs_present: sides.theirs.is_some(),
            base_present: sides.base.is_some(),
            ours_bytes: sides.ours.as_ref().map_or(0, Vec::len),
            theirs_bytes: sides.theirs.as_ref().map_or(0, Vec::len),
            base_bytes: sides.base.as_ref().map_or(0, Vec::len),
            worktree_markers: worktree.is_some_and(has_conflict_markers),
            binary,
            regions: if binary {
                Vec::new()
            } else {
                regions_from_sides(&sides)
            },
        }
    }
}

struct Loaded {
    path: String,
    kind: ConflictKind,
    data: ConflictData,
}

/// Why a conflict operation was submitted, for the notice after it finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    Resolve,
    MarkResolved,
    Mergetool,
    Continue,
    Abort,
}

impl PendingAction {
    fn notice(self) -> &'static str {
        match self {
            PendingAction::Resolve => "Conflict resolved",
            PendingAction::MarkResolved => "Marked as resolved",
            PendingAction::Mergetool => "Merge tool finished",
            PendingAction::Continue => "Merge committed",
            PendingAction::Abort => "Merge aborted",
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
    /// Path that was already asked to be marked resolved while its worktree
    /// file still held markers; a second click confirms.
    marker_warning: Option<String>,
    notice: Option<String>,
    error: Option<String>,
    list_scroll: UniformListScrollHandle,
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
        kind: Option<ConflictKind>,
        result: strata_core::Result<ConflictData>,
    ) {
        if epoch != self.epoch {
            return;
        }
        self.loading = None;
        match result {
            Ok(data) => {
                let kind = kind.unwrap_or_else(|| {
                    // The path left the conflict list between the read and its
                    // result; the stages that were read still describe the kind.
                    kind_from_presence(data.base_present, data.ours_present, data.theirs_present)
                });
                self.loaded = Some(Loaded { path, kind, data });
                self.error = None;
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
        // The stages of the path just changed; the next selection reads them again.
        self.loaded = None;
        match error {
            Some(error) => self.error = Some(error.to_owned()),
            None => {
                self.error = None;
                self.notice = Some(action.notice().to_owned());
            }
        }
        true
    }

    pub(crate) fn banner_state(&self, snapshot: &RepoSnapshot) -> Option<BannerState> {
        let busy = snapshot.busy.is_some() || self.pending.is_some();
        banner_state(snapshot.state, snapshot.conflicts.len(), busy)
    }
}

// ---------------------------------------------------------------------------
// RepoView glue: every write goes through the session.
// ---------------------------------------------------------------------------

impl RepoView {
    /// Reads the index stages of `path` and derives its conflict regions, off
    /// the UI thread.
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
                    let worktree = repo.file_in_worktree(&read_path).ok().flatten();
                    Ok(ConflictData::new(sides, worktree.as_deref()))
                })
                .await;
            this.update(cx, |this, cx| {
                this.worktree.conflicts.finish_load(epoch, path, kind, data);
                cx.notify();
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

    /// Stages the selected conflict after checking the worktree file for
    /// markers, so a half-edited file is not marked resolved by accident.
    pub(crate) fn mark_conflict_resolved(&mut self, cx: &mut Context<Self>) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let Some(path) = self.worktree.selected_conflict().map(str::to_owned) else {
            return;
        };
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
                cx.notify();
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
        let id = session.run(Operation::MarkResolved(vec![path]));
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

    pub(crate) fn continue_merge(&mut self, cx: &mut Context<Self>) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if !session.snapshot().conflicts.is_empty() {
            return;
        }
        let id = session.run(Operation::MergeContinue);
        self.worktree.conflicts.start(id, PendingAction::Continue);
        cx.notify();
    }

    pub(crate) fn abort_merge(&mut self, cx: &mut Context<Self>) {
        if self.worktree.conflicts.pending.is_some() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let id = session.run(Operation::MergeAbort);
        self.worktree.conflicts.start(id, PendingAction::Abort);
        cx.notify();
    }
}

/// Asks before throwing away the work done in a stopped merge.
pub(crate) fn confirm_abort_merge(entity: Entity<RepoView>, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _window, _cx| {
        let entity = entity.clone();
        alert
            .title("Abort the merge?")
            .description(
                "Git restores HEAD and the worktree from before the merge. Conflicts resolved so far are lost.",
            )
            .show_cancel(true)
            .cancel_text("Keep resolving")
            .ok_text("Abort merge")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, _, cx| {
                entity.update(cx, |this, cx| this.abort_merge(cx));
                true
            })
    });
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Colors {
    background: Hsla,
    foreground: Hsla,
    muted: Hsla,
    muted_foreground: Hsla,
    border: Hsla,
    selection: Hsla,
    title_bar: Hsla,
    accent: Hsla,
    accent_foreground: Hsla,
    danger: Hsla,
    warning: Hsla,
    success: Hsla,
    info: Hsla,
    mono: SharedString,
}

impl Colors {
    fn from_theme(theme: &Theme) -> Self {
        Self {
            background: theme.background,
            foreground: theme.foreground,
            muted: theme.muted,
            muted_foreground: theme.muted_foreground,
            border: theme.border,
            selection: theme.list_active,
            title_bar: theme.title_bar,
            accent: theme.primary,
            accent_foreground: theme.primary_foreground,
            danger: theme.danger,
            warning: theme.warning,
            success: theme.success,
            info: theme.info,
            mono: theme.mono_font_family.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Normal,
    Primary,
    Danger,
}

fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    tone: Tone,
    colors: &Colors,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let (background, foreground) = match (enabled, tone) {
        (false, _) => (colors.muted, colors.muted_foreground),
        (true, Tone::Primary) => (colors.accent, colors.accent_foreground),
        (true, Tone::Danger) => (colors.danger, colors.accent_foreground),
        (true, Tone::Normal) => (colors.muted.opacity(0.5), colors.foreground),
    };
    let mut element = div()
        .id(id)
        .flex_none()
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(colors.border)
        .text_size(px(10.5))
        .text_color(foreground)
        .bg(background);
    if enabled {
        element = element.cursor_pointer().on_click(handler);
    }
    element.child(label.into()).into_any_element()
}

fn message(text: &str, color: Hsla, colors: &Colors) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .p_4()
        .text_size(px(12.0))
        .text_color(color)
        .bg(colors.background)
        .child(text.to_owned())
        .into_any_element()
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

    let mut row = div()
        .id(("conflict-file", index))
        .flex()
        .items_center()
        .gap_2()
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_2()
        .text_size(px(11.0))
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            let path = click_path.clone();
            entity.update(cx, |this, cx| {
                this.select_conflict_file(path.clone(), window, cx)
            });
        })
        .child(
            div()
                .w(px(14.0))
                .flex_none()
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(colors.danger)
                .child("!"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(if is_selected {
                    colors.accent
                } else {
                    colors.foreground
                })
                .child(path),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(9.5))
                .text_color(colors.muted_foreground)
                .child(kind_label(kind)),
        );
    if is_selected {
        row = row.bg(colors.selection);
    }
    row.into_any_element()
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
        let colors = Colors::from_theme(cx.theme());
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
                    .pt_2()
                    .pb_1()
                    .child(
                        div()
                            .text_size(px(10.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(colors.danger)
                            .child(format!("CONFLICTS ({})", conflicts.len())),
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
        let colors = Colors::from_theme(cx.theme());
        let busy = snapshot.busy.is_some() || self.pending.is_some();
        let loaded = self.data_for(path);
        let kind = loaded.map(|loaded| loaded.kind);

        let mut header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .h(px(30.0))
            .flex_none()
            .px_3()
            .bg(colors.title_bar)
            .border_b_1()
            .border_color(colors.border)
            .text_size(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(div().truncate().child(path.to_owned()))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(colors.warning)
                            .child(kind.map(kind_label).unwrap_or("conflict")),
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
        let mut toolbar = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_none()
            .px_3()
            .h(px(30.0))
            .border_b_1()
            .border_color(colors.border);

        if let Some(kind) = kind {
            for (index, choice) in choices(kind).into_iter().enumerate() {
                let entity = entity.clone();
                let button_colors = colors.clone();
                toolbar = toolbar.child(button(
                    ("conflict-choice", index),
                    choice.label,
                    ready,
                    if choice.destructive {
                        Tone::Danger
                    } else {
                        Tone::Normal
                    },
                    &button_colors,
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| this.use_conflict_side(choice.side, cx));
                    },
                ));
            }
            toolbar = toolbar
                .child(button(
                    "conflict-mergetool",
                    "Open in merge tool",
                    ready,
                    Tone::Normal,
                    &colors,
                    {
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| this.open_merge_tool(cx));
                        }
                    },
                ))
                .child(button(
                    "conflict-mark-resolved",
                    "Mark as resolved",
                    ready,
                    Tone::Primary,
                    &colors,
                    {
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| this.mark_conflict_resolved(cx));
                        }
                    },
                ));
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
        } else if loaded.is_some_and(|loaded| loaded.data.worktree_markers) {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .px_3()
                    .py_1()
                    .bg(colors.warning.opacity(0.18))
                    .text_size(px(10.5))
                    .text_color(colors.warning)
                    .child(
                        "The file on disk still has conflict markers; edit it, then mark it resolved.",
                    ),
            );
        }

        column = column.child(match (&self.error, loaded) {
            (Some(error), _) => message(error, colors.danger, &colors),
            (None, None) => message("reading conflict…", colors.muted_foreground, &colors),
            (None, Some(loaded)) if loaded.data.binary => message(
                "Binary conflict — choose a whole version above.",
                colors.muted_foreground,
                &colors,
            ),
            (None, Some(loaded)) if loaded.data.regions.is_empty() => message(
                "No textual differences between the sides.",
                colors.muted_foreground,
                &colors,
            ),
            (None, Some(loaded)) => self.regions(&loaded.data.regions, &colors),
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
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .flex_none()
            .px_3()
            .py_1()
            .bg(colors.danger.opacity(0.18))
            .text_size(px(10.5))
            .text_color(colors.danger)
            .child("This file still contains <<<<<<< conflict markers. Mark it resolved anyway?")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    .child(button(
                        "marker-cancel",
                        "Cancel",
                        true,
                        Tone::Normal,
                        colors,
                        {
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.dismiss_marker_warning(cx));
                            }
                        },
                    ))
                    .child(button(
                        "marker-confirm",
                        "Mark resolved anyway",
                        true,
                        Tone::Danger,
                        colors,
                        {
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.stage_resolved_conflict(cx));
                            }
                        },
                    )),
            )
            .into_any_element()
    }

    fn regions(&self, regions: &[Region], colors: &Colors) -> AnyElement {
        let mut list = div()
            .id("conflict-regions")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll();
        for (index, region) in regions.iter().take(MAX_REGIONS).enumerate() {
            list = list.child(region_block(index, region, colors));
        }
        if regions.len() > MAX_REGIONS {
            list = list.child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .text_size(px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(format!(
                        "{} more regions not shown; use the merge tool for this file.",
                        regions.len() - MAX_REGIONS
                    )),
            );
        }
        list.into_any_element()
    }
}

fn region_block(index: usize, region: &Region, colors: &Colors) -> AnyElement {
    let mut block = div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .border_t_1()
        .border_color(colors.border)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .h(px(20.0))
                .px_3()
                .flex_none()
                .bg(colors.muted.opacity(0.35))
                .text_size(px(10.0))
                .text_color(colors.muted_foreground)
                .child(format!("Region {} · {}", index + 1, region.base_label()))
                .child(change_badges(region, colors)),
        );
    block = block.child(side_block(
        "OURS (HEAD)",
        &region.ours,
        region.ours_changed,
        colors.success,
        colors,
    ));
    block = block.child(side_block(
        "THEIRS (incoming)",
        &region.theirs,
        region.theirs_changed,
        colors.info,
        colors,
    ));
    if !region.base.is_empty() {
        block = block.child(side_block(
            "BASE",
            &region.base,
            false,
            colors.muted,
            colors,
        ));
    }
    block.into_any_element()
}

fn change_badges(region: &Region, colors: &Colors) -> AnyElement {
    let mut badges = div().flex().items_center().gap_1();
    if region.ours_changed {
        badges = badges.child(div().text_color(colors.success).child("ours changed"));
    }
    if region.theirs_changed {
        badges = badges.child(div().text_color(colors.info).child("theirs changed"));
    }
    badges.into_any_element()
}

fn side_block(
    title: &str,
    lines: &[String],
    changed: bool,
    tint: Hsla,
    colors: &Colors,
) -> AnyElement {
    let mut block = div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .border_t_1()
        .border_color(colors.border)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .h(px(20.0))
                .px_3()
                .flex_none()
                .bg(if changed {
                    tint.opacity(0.18)
                } else {
                    colors.background
                })
                .text_size(px(10.0))
                .text_color(if changed {
                    colors.foreground
                } else {
                    colors.muted_foreground
                })
                .child(title.to_owned()),
        );
    if lines.is_empty() {
        block = block.child(
            div()
                .px_3()
                .py_1()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child("(no content)"),
        );
    } else {
        for line in lines {
            block = block.child(line_row(line, tint, changed, colors));
        }
    }
    block.into_any_element()
}

fn line_row(text: &str, tint: Hsla, changed: bool, colors: &Colors) -> AnyElement {
    let mut row = div()
        .flex()
        .items_center()
        .w_full()
        .px_3()
        .font_family(colors.mono.clone())
        .text_size(px(11.5))
        .text_color(colors.foreground);
    if changed {
        row = row.bg(tint.opacity(0.08));
    }
    row.child(SharedString::from(text.to_owned()))
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

    #[test]
    fn every_kind_has_plain_words() {
        assert_eq!(kind_label(ConflictKind::BothModified), "both modified");
        assert_eq!(kind_label(ConflictKind::BothAdded), "both added");
        assert_eq!(kind_label(ConflictKind::BothDeleted), "both deleted");
        assert_eq!(kind_label(ConflictKind::AddedByUs), "added by us");
        assert_eq!(kind_label(ConflictKind::AddedByThem), "added by them");
        assert_eq!(kind_label(ConflictKind::DeletedByUs), "deleted by us");
        assert_eq!(kind_label(ConflictKind::DeletedByThem), "deleted by them");
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
    fn text_lines_keep_content_and_drop_the_final_newline() {
        assert_eq!(
            text_lines(b"a\r\nb\n"),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert_eq!(text_lines(b"a\nb"), vec!["a".to_owned(), "b".to_owned()]);
        assert!(text_lines(b"").is_empty());
        assert_eq!(text_lines(b"\n"), vec![String::new()]);
    }

    #[test]
    fn each_side_changes_are_shown_against_the_base() {
        let sides = sides(
            "one\ntwo\nthree\n",
            "one\nTWO\nthree\n",
            "one\ntwo\nTHREE\n",
        );
        let regions = regions_from_sides(&sides);
        assert_eq!(regions.len(), 2, "the two changes are separate regions");

        assert_eq!(regions[0].base, vec!["two".to_owned()]);
        assert_eq!(regions[0].ours, vec!["TWO".to_owned()]);
        assert_eq!(regions[0].theirs, vec!["two".to_owned()]);
        assert!(regions[0].ours_changed);
        assert!(!regions[0].theirs_changed);

        assert_eq!(regions[1].base, vec!["three".to_owned()]);
        assert_eq!(regions[1].ours, vec!["three".to_owned()]);
        assert_eq!(regions[1].theirs, vec!["THREE".to_owned()]);
        assert!(!regions[1].ours_changed);
        assert!(regions[1].theirs_changed);
    }

    #[test]
    fn the_same_line_changed_by_both_sides_is_one_region() {
        let sides = sides("a\nb\nc\n", "a\nB1\nc\n", "a\nB2\nc\n");
        let regions = regions_from_sides(&sides);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].ours, vec!["B1".to_owned()]);
        assert_eq!(regions[0].theirs, vec!["B2".to_owned()]);
        assert_eq!(regions[0].base, vec!["b".to_owned()]);
        assert!(regions[0].ours_changed && regions[0].theirs_changed);
    }

    #[test]
    fn a_side_that_deleted_the_file_shows_no_content() {
        let sides = ConflictSides {
            base: Some(b"a\nb\n".to_vec()),
            ours: None,
            theirs: Some(b"a\nb\nc\n".to_vec()),
        };
        let regions = regions_from_sides(&sides);
        assert_eq!(regions.len(), 1);
        assert!(regions[0].ours_changed);
        assert!(regions[0].ours.is_empty(), "the file is gone on our side");
        assert_eq!(regions[0].theirs, vec!["a", "b", "c"]);
    }

    #[test]
    fn both_added_shows_the_whole_file() {
        let sides = ConflictSides {
            base: None,
            ours: Some(b"ours\n".to_vec()),
            theirs: Some(b"theirs\n".to_vec()),
        };
        let regions = regions_from_sides(&sides);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].ours, vec!["ours".to_owned()]);
        assert_eq!(regions[0].theirs, vec!["theirs".to_owned()]);
        assert!(regions[0].base.is_empty());
    }

    #[test]
    fn identical_sides_have_no_regions() {
        let sides = sides("a\nb\n", "a\nb\n", "a\nb\n");
        assert!(regions_from_sides(&sides).is_empty());
    }

    #[test]
    fn an_insertion_beside_the_other_sides_change_stays_in_that_region() {
        let sides = sides("a\nc\n", "a\nb\nc\n", "a\nC\n");
        let regions = regions_from_sides(&sides);
        assert_eq!(regions.len(), 1, "the insertion touches the change");
        assert_eq!(regions[0].base, vec!["c".to_owned()]);
        assert_eq!(regions[0].ours, vec!["b".to_owned(), "c".to_owned()]);
        assert_eq!(regions[0].theirs, vec!["C".to_owned()]);
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
    fn a_binary_conflict_has_sizes_instead_of_regions() {
        let sides = ConflictSides {
            base: Some(vec![0, 1, 2]),
            ours: Some(vec![0, 3, 4]),
            theirs: Some(vec![0, 5, 6]),
        };
        let data = ConflictData::new(sides, Some(&[0, 7]));
        assert!(data.binary);
        assert!(data.regions.is_empty());
        assert_eq!(sizes_line(&data), "ours 3 B · theirs 3 B · base 3 B");
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn the_banner_counts_conflicts_and_gates_continue() {
        assert_eq!(banner_state(RepoState::Normal, 0, false), None);

        let open = banner_state(RepoState::Merging, 2, false).expect("merging banner");
        assert!(open.text.contains("2 conflicts"));
        assert!(!open.can_continue, "conflicts must be resolved first");
        assert!(open.can_abort);

        let resolved = banner_state(RepoState::Merging, 0, false).expect("merging banner");
        assert!(resolved.can_continue);
        assert!(resolved.can_abort);

        let busy = banner_state(RepoState::Merging, 0, true).expect("merging banner");
        assert!(!busy.can_continue, "an operation is already running");
        assert!(!busy.can_abort);

        let rebasing = banner_state(RepoState::Rebasing, 1, false).expect("rebase banner");
        assert!(rebasing.text.contains("1 conflict"));
        assert!(!rebasing.can_continue && !rebasing.can_abort);
    }
}
