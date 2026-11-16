//! Standalone Compare tool for arbitrary files and folders (2-way and 3-way),
//! independent of any git repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, ElementId, Entity, FocusHandle, Focusable,
    Hsla, InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    ScrollHandle, ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled,
    Subscription, UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
};
use gpui_component::Selectable as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::popover::Popover;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Icon, TitleBar, WindowExt as _};
use gpui_kit_assets::IconName;
use strata_core::compare::{
    FolderCompareEntry, FolderCompareOptions, FolderCompareResult, FolderEntryKind,
    FolderEntryStatus, RepoCompareSide, RepoCompareTree, ThreeWayCompareMode, ThreeWayDiffKind,
    ThreeWayFileCompare, ThreeWayImageCompare, ThreeWayPeerStatus, ThreeWayRow, ThreeWaySide,
    ThreeWayTreeChange, ThreeWayTreeCompare, TwoWayFileCompare, compare_folders,
    compare_three_optional_files, compare_three_sides, compare_two_bytes,
    compare_two_optional_files, compare_two_repo_sides,
};
use strata_core::diff::{DiffOptions, GapAction};
use strata_core::image::ImageKind;
use strata_core::lfs::format_size;
use strata_core::session::RepoReader;
use strata_core::text::display_width_with_tabs;
use strata_core::{Oid, RefInfo, RefKind};

use crate::data;
use crate::diff_view::{self, DiffColors, DiffContext, DiffHighlights, DiffStyle, Row as DiffRow};
use crate::image_diff;
use crate::scrollbar::WithScrollbar as _;
use crate::settings::Settings;
use crate::style::{self, Design};
use crate::ui::{self, BadgeTone, ButtonKind, ButtonSize, RowState};
use crate::workspace::Workspace;

/// Space the title bar reserves around its own row: the component's left
/// padding and the platform's window controls, which the row must never
/// paint over.
const COMPARE_TITLE_CHROME: f32 = 144.0;

/// The gap between the title bar's parts.
const COMPARE_ROW_GAP: f32 = 8.0;

/// The 1 px rule between the app mark and the title, with its margins.
const COMPARE_DIVIDER_WIDTH: f32 = 9.0;

/// The narrowest the title may get before the details around it give way. A
/// truncated title is still readable; a title squeezed to nothing is not.
const COMPARE_MIN_TITLE_WIDTH: f32 = 120.0;

/// A small ghost button, which is how refresh and the back arrow are drawn at
/// every width.
const COMPARE_ICON_BUTTON_WIDTH: f32 = 30.0;

/// The gap inside the action pair on the right.
const COMPARE_BUTTON_GAP: f32 = 4.0;

/// The "Back to file list" label beside its arrow, at the widest tier.
const COMPARE_BACK_LABEL_WIDTH: f32 = 150.0;

/// The "New Compare…" label beside its icon, at the widest tier.
const COMPARE_NEW_LABEL_WIDTH: f32 = 112.0;

/// Below this width the wordmark gives its space to the title.
const COMPARE_WORDMARK_BELOW: f32 = 1100.0;

/// Below this width the back button keeps only its arrow, with the
/// destination in the tooltip.
const COMPARE_BACK_LABEL_BELOW: f32 = 820.0;

/// Below this width the refresh and New Compare actions become icons, with
/// their names in the tooltips.
const COMPARE_ACTION_LABEL_BELOW: f32 = 900.0;

/// What the compare title bar shows at a given window width.
///
/// The row never grows taller than the title bar and never pushes the window
/// controls off screen: the title takes the leftover width and truncates,
/// while the details around it give way in one order as the window narrows —
/// the wordmark, the back button's label, then the New Compare label. Window
/// controls are drawn by the title bar itself, so every width here is the
/// part this row may use.
#[derive(Clone, Copy, Debug)]
struct CompareHeaderFit {
    /// The window has a list to return to, so the back button is shown.
    has_back: bool,
    /// The "STRATA" wordmark next to the app mark.
    wordmark: bool,
    /// Whether the back button shows its destination beside the arrow.
    back_label: bool,
    /// Whether New Compare shows its label beside its icon.
    new_compare_label: bool,
    zoom: f32,
}

impl CompareHeaderFit {
    fn new(width: f32, has_back: bool, zoom: f32) -> Self {
        let effective = width / zoom;
        let mut fit = Self {
            has_back,
            wordmark: effective >= COMPARE_WORDMARK_BELOW,
            back_label: has_back && effective >= COMPARE_BACK_LABEL_BELOW,
            new_compare_label: effective >= COMPARE_ACTION_LABEL_BELOW,
            zoom,
        };
        // Every tier above leaves room at its own threshold; this is the
        // safety net for a zoomed window, where the threshold and the row it
        // reserves drift apart.
        while fit.used() > width && fit.shed() {}
        fit
    }

    /// The width the row reserves, including the narrowest title it keeps.
    fn used(&self) -> f32 {
        let mark = style::BRAND_MARK
            + if self.wordmark {
                // The gap inside the app mark group.
                7.0 + style::wordmark_width(style::BRAND_WORDMARK)
            } else {
                0.0
            };
        let back = if self.has_back {
            COMPARE_ROW_GAP
                + COMPARE_ICON_BUTTON_WIDTH
                + if self.back_label {
                    COMPARE_BACK_LABEL_WIDTH
                } else {
                    0.0
                }
        } else {
            0.0
        };
        let actions = COMPARE_ICON_BUTTON_WIDTH
            + COMPARE_BUTTON_GAP
            + if self.new_compare_label {
                COMPARE_NEW_LABEL_WIDTH
            } else {
                COMPARE_ICON_BUTTON_WIDTH
            };
        (COMPARE_TITLE_CHROME
            + mark
            + COMPARE_ROW_GAP
            + COMPARE_DIVIDER_WIDTH
            + COMPARE_ROW_GAP
            // The compare glyph before the title, with its gap.
            + 22.0
            + back
            + COMPARE_ROW_GAP
            + COMPARE_MIN_TITLE_WIDTH
            + COMPARE_ROW_GAP
            + actions)
            * self.zoom
    }

    /// Gives up one detail, the least load-bearing first. Returns false once
    /// only the icon-only row is left, which always fits the app's minimum
    /// window.
    fn shed(&mut self) -> bool {
        if self.wordmark {
            // The app mark alone still identifies the window.
            self.wordmark = false;
        } else if self.back_label {
            // The arrow keeps the way back; the tooltip names the
            // destination.
            self.back_label = false;
        } else if self.new_compare_label {
            self.new_compare_label = false;
        } else {
            return false;
        }
        true
    }
}

/// Validated target for a standalone 2-way or 3-way comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompareTarget {
    TwoWayFiles {
        left: PathBuf,
        right: PathBuf,
    },
    ThreeWayFiles {
        left: PathBuf,
        middle: PathBuf,
        right: PathBuf,
    },
    TwoWayFolders {
        left: PathBuf,
        right: PathBuf,
    },
    ThreeWayFolders {
        left: PathBuf,
        middle: PathBuf,
        right: PathBuf,
    },
}

impl CompareTarget {
    /// Returns a concise title made from the compared file or folder names.
    pub(crate) fn title(&self) -> String {
        match self {
            CompareTarget::TwoWayFiles { left, right }
            | CompareTarget::TwoWayFolders { left, right } => {
                format!("{} ↔ {}", path_label(left), path_label(right))
            }
            CompareTarget::ThreeWayFiles {
                left,
                middle,
                right,
            }
            | CompareTarget::ThreeWayFolders {
                left,
                middle,
                right,
            } => format!(
                "{} ↔ {} ↔ {}",
                path_label(left),
                path_label(middle),
                path_label(right)
            ),
        }
    }

    pub(crate) fn paths(&self) -> Vec<PathBuf> {
        match self {
            CompareTarget::TwoWayFiles { left, right }
            | CompareTarget::TwoWayFolders { left, right } => vec![left.clone(), right.clone()],
            CompareTarget::ThreeWayFiles {
                left,
                middle,
                right,
            }
            | CompareTarget::ThreeWayFolders {
                left,
                middle,
                right,
            } => vec![left.clone(), middle.clone(), right.clone()],
        }
    }
}

/// Width the compare toolbar's fixed ends always keep: the mode choice, the
/// change count with its two arrows, and the row's own padding.
const COMPARE_TOOLBAR_FIXED_WIDTH: f32 = 360.0;

/// The two "use … as base" buttons beside the mode choice, shown when a
/// revision comparison reads the middle as the base.
const COMPARE_BASE_ACTIONS_WIDTH: f32 = 240.0;

/// One status badge, at the widest label the locales use.
const COMPARE_BADGE_WIDTH: f32 = 132.0;

/// How many of `total` badges fit beside the toolbar's fixed ends.
///
/// Badges are the first thing to go when the toolbar narrows: the mode choice
/// and the change count are controls, while a badge repeats a count the panes
/// already show.
fn visible_badges(total: usize, width: f32, zoom: f32, reserved: f32) -> usize {
    let usable = width / zoom - reserved;
    if usable <= 0.0 {
        return 0;
    }
    ((usable / COMPARE_BADGE_WIDTH).floor() as usize).min(total)
}

fn path_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

// ---------------------------------------------------------------------------
// Repository revision comparison
// ---------------------------------------------------------------------------

/// The branch, tag or remote branch pointing at a revision, local branches
/// first, or an empty label when none does.
pub(crate) fn label_at_revision(refs: &[RefInfo], oid: Option<Oid>) -> String {
    let Some(oid) = oid else {
        return String::new();
    };
    for kind in [RefKind::LocalBranch, RefKind::Tag, RefKind::RemoteBranch] {
        if let Some(reference) = refs
            .iter()
            .find(|reference| reference.kind == kind && reference.target == oid)
        {
            return reference.short_name.clone();
        }
    }
    String::new()
}

/// One side of a repository revision comparison: a commit, the index or the
/// working tree, named the way the entry point that opened it named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompareRevision {
    pub side: RepoCompareSide,
    /// Branch or tag name, or a descriptive label for the index and the
    /// working tree. A commit's short id is always shown beside it.
    pub label: String,
}

impl CompareRevision {
    pub(crate) fn commit(label: impl Into<String>, oid: Oid) -> Self {
        Self {
            side: RepoCompareSide::Commit(oid),
            label: label.into(),
        }
    }

    /// A named side for either a commit or one of the revisionless sides.
    pub(crate) fn named(label: impl Into<String>, side: RepoCompareSide) -> Self {
        Self {
            side,
            label: label.into(),
        }
    }

    pub(crate) fn worktree() -> Self {
        Self::named(
            t!("compare.working_tree").to_string(),
            RepoCompareSide::Worktree,
        )
    }

    /// `main (a1b2c3d)`, or just the short id when there is no branch name.
    /// The index and the working tree have no id to show.
    pub(crate) fn display(&self) -> String {
        let RepoCompareSide::Commit(oid) = self.side else {
            return self.label.clone();
        };
        let short = data::short_oid(oid);
        if self.label.is_empty() || self.label == short {
            short
        } else {
            format!("{} ({})", self.label, short)
        }
    }
}

/// The three sides of a repository comparison and the reader they are read
/// through, so the work runs on a background executor.
#[derive(Clone)]
pub(crate) struct RepoThreeWaySource {
    pub reader: Arc<dyn RepoReader>,
    /// Repository root, for the tab's hint path.
    pub root: PathBuf,
    pub left: CompareRevision,
    pub middle: CompareRevision,
    pub right: CompareRevision,
}

impl RepoThreeWaySource {
    fn title(&self) -> String {
        format!(
            "{} ↔ {} ↔ {}",
            self.left.display(),
            self.middle.display(),
            self.right.display()
        )
    }

    /// What the pane headers call each side.
    fn pane_labels(&self) -> [String; 3] {
        [
            self.left.display(),
            self.middle.display(),
            self.right.display(),
        ]
    }

    fn sides(&self) -> [RepoCompareSide; 3] {
        [self.left.side, self.middle.side, self.right.side]
    }
}

/// Two sides of a repository comparison.
#[derive(Clone)]
pub(crate) struct RepoTwoWaySource {
    pub reader: Arc<dyn RepoReader>,
    pub root: PathBuf,
    pub left: CompareRevision,
    pub right: CompareRevision,
}

impl RepoTwoWaySource {
    fn title(&self) -> String {
        format!("{} ↔ {}", self.left.display(), self.right.display())
    }

    fn pane_labels(&self) -> [String; 2] {
        [self.left.display(), self.right.display()]
    }
}

/// One file of a repository comparison where each side may know it under a
/// different path, as a history walk follows renames.
#[derive(Clone)]
pub(crate) struct RepoThreeWayFileSource {
    pub source: RepoThreeWaySource,
    /// The path in `[left, middle, right]`.
    pub paths: [String; 3],
}

impl RepoThreeWayFileSource {
    fn title(&self) -> String {
        format!(
            "{} — {}",
            path_label(Path::new(&self.paths[2])),
            self.source.title()
        )
    }
}

/// What the compare view shows: arbitrary paths, two or three repository
/// sides, or one file of three sides.
enum CompareRoot {
    Paths(CompareTarget),
    Revisions(Box<RepoThreeWaySource>),
    RevisionPair(Box<RepoTwoWaySource>),
    RevisionFile(Box<RepoThreeWayFileSource>),
}

impl CompareRoot {
    fn title(&self) -> String {
        match self {
            Self::Paths(target) => target.title(),
            Self::Revisions(source) => source.title(),
            Self::RevisionPair(source) => source.title(),
            Self::RevisionFile(source) => source.title(),
        }
    }

    /// The path line under the title.
    fn subtitle(&self) -> String {
        match self {
            Self::Paths(target) => target
                .paths()
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("  ↔  "),
            Self::Revisions(source) => source.root.display().to_string(),
            Self::RevisionPair(source) => source.root.display().to_string(),
            Self::RevisionFile(source) => format!(
                "{} — {}",
                source.source.root.display(),
                source.paths.join("  ↔  ")
            ),
        }
    }

    fn hint_path(&self) -> Option<&Path> {
        match self {
            Self::Paths(target) => match target {
                CompareTarget::TwoWayFiles { left, .. }
                | CompareTarget::ThreeWayFiles { left, .. }
                | CompareTarget::TwoWayFolders { left, .. }
                | CompareTarget::ThreeWayFolders { left, .. } => Some(left.as_path()),
            },
            Self::Revisions(source) => Some(source.root.as_path()),
            Self::RevisionPair(source) => Some(source.root.as_path()),
            Self::RevisionFile(source) => Some(source.source.root.as_path()),
        }
    }

    fn revisions(&self) -> Option<&RepoThreeWaySource> {
        match self {
            Self::Revisions(source) => Some(source),
            Self::RevisionFile(source) => Some(&source.source),
            Self::Paths(_) | Self::RevisionPair(_) => None,
        }
    }

    fn pair(&self) -> Option<&RepoTwoWaySource> {
        match self {
            Self::RevisionPair(source) => Some(source),
            _ => None,
        }
    }

    /// True for a comparison with three sides, which the mode toggle governs.
    fn is_three_way(&self) -> bool {
        match self {
            Self::Paths(CompareTarget::ThreeWayFiles { .. })
            | Self::Paths(CompareTarget::ThreeWayFolders { .. }) => true,
            Self::Revisions(_) | Self::RevisionFile(_) => true,
            Self::Paths(_) | Self::RevisionPair(_) => false,
        }
    }

    /// The mode a tab starts in: a merge preview reads the middle as the base,
    /// while a compare picked by revision or history treats the sides as
    /// peers.
    fn default_mode(&self) -> ThreeWayCompareMode {
        match self {
            Self::Revisions(_) | Self::RevisionFile(_) => ThreeWayCompareMode::NoBase,
            _ => ThreeWayCompareMode::MiddleBase,
        }
    }
}

/// Classification and real-conflict flag for one row of a revision tree.
#[derive(Debug, Clone, Copy)]
struct RepoTreeMark {
    /// How the path changed relative to the middle side when it is the base.
    change: Option<ThreeWayTreeChange>,
    /// How the path relates to its neighbours when the sides are peers.
    peer: Option<ThreeWayPeerStatus>,
    conflicting: bool,
}

/// Counts the peer filter chips show for one revision tree.
#[derive(Debug, Clone, Copy, Default)]
struct PeerTreeCounts {
    total: usize,
    left_middle: usize,
    middle_right: usize,
    all_different: usize,
    missing: usize,
}

impl PeerTreeCounts {
    fn record(&mut self, peer: ThreeWayPeerStatus) {
        self.total += 1;
        match peer {
            ThreeWayPeerStatus::Same => {}
            ThreeWayPeerStatus::LeftMiddleDiffer => {
                self.left_middle += 1;
            }
            ThreeWayPeerStatus::MiddleRightDiffer => {
                self.middle_right += 1;
            }
            ThreeWayPeerStatus::AllDifferent => {
                self.left_middle += 1;
                self.middle_right += 1;
                self.all_different += 1;
            }
            ThreeWayPeerStatus::MissingSomeSide => {
                self.missing += 1;
            }
        }
    }
}

/// Which side should become the base after a swap in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseChoice {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompareValidationError {
    MissingRequired,
    WrongArgCount(usize),
    PathNotFound(PathBuf),
    NotFileOrFolder(PathBuf),
    MixedTypes,
    /// Repository revisions were requested with no repository tab active.
    NoRepository,
    /// A typed revision spec names nothing the repository can read.
    UnknownRevision(String),
}

impl CompareValidationError {
    pub(crate) fn message(&self) -> Option<String> {
        match self {
            CompareValidationError::MissingRequired => None,
            CompareValidationError::WrongArgCount(count) => {
                Some(t!("compare.validation.wrong_count", count = *count).to_string())
            }
            CompareValidationError::PathNotFound(path) => Some(
                t!(
                    "compare.validation.not_found",
                    path = path.display().to_string()
                )
                .to_string(),
            ),
            CompareValidationError::NotFileOrFolder(path) => Some(
                t!(
                    "compare.validation.invalid_kind",
                    path = path.display().to_string()
                )
                .to_string(),
            ),
            CompareValidationError::MixedTypes => {
                Some(t!("compare.validation.mixed_types").to_string())
            }
            CompareValidationError::NoRepository => Some(t!("compare.no_repository").to_string()),
            CompareValidationError::UnknownRevision(spec) => {
                Some(t!("compare.validation.unknown_revision", spec = spec.clone()).to_string())
            }
        }
    }
}

/// One choice the revision picker offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RevisionOption {
    /// What validation reads back: a branch or tag name, an object id, or a
    /// localized side name.
    pub spec: String,
    /// What the field shows once the user picks it: the short id and summary
    /// for a commit, the ref name for a branch or tag, or the side's name.
    pub display: String,
    /// What the picker row reads, which for a commit adds its summary.
    pub label: String,
    pub side: RepoCompareSide,
}

/// The revisions a repository tab can offer the Compare dialog, and the reader
/// that resolves typed specs against the same repository.
#[derive(Clone, Default)]
pub(crate) struct RevisionOptions {
    pub reader: Option<Arc<dyn RepoReader>>,
    pub options: Vec<RevisionOption>,
}

impl RevisionOptions {
    pub(crate) fn has_repository(&self) -> bool {
        self.reader.is_some()
    }
}

/// A side the dialog resolved, ready for the workspace to build a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DialogRevision {
    pub side: RepoCompareSide,
    /// A branch, tag or side name the picker knew; empty for a typed object id.
    pub label: String,
}

/// What the dialog wants to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompareDialogRequest {
    Paths(CompareTarget),
    Revisions(Vec<DialogRevision>),
}

/// Validates the dialog's three revision fields (`left`, `middle`, optional
/// `right` order is `left | middle | right`), resolving each to a side.
pub(crate) fn validate_revision_inputs(
    left: &str,
    right: &str,
    middle: &str,
    options: &RevisionOptions,
) -> Result<Vec<DialogRevision>, CompareValidationError> {
    let left = left.trim();
    let right = right.trim();
    let middle = middle.trim();
    if left.is_empty() || right.is_empty() {
        return Err(CompareValidationError::MissingRequired);
    }
    if !options.has_repository() {
        return Err(CompareValidationError::NoRepository);
    }
    let specs = if middle.is_empty() {
        vec![left, right]
    } else {
        vec![left, middle, right]
    };
    specs
        .into_iter()
        .map(|spec| resolve_revision(spec, options))
        .collect()
}

/// A full object id, which the picker stores as a spec but never shows.
fn is_full_oid(spec: &str) -> bool {
    spec.len() == 40 && spec.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn resolve_revision(
    spec: &str,
    options: &RevisionOptions,
) -> Result<DialogRevision, CompareValidationError> {
    if let Some(option) = options
        .options
        .iter()
        .find(|option| option.spec == spec || option.display == spec || option.label == spec)
    {
        return Ok(DialogRevision {
            side: option.side,
            // A commit's display text is the short id plus its summary, but a
            // compare pane labels a commit by the branch or tag that points at
            // it, which the caller looks up from the oid when this is empty.
            label: if is_full_oid(&option.spec) {
                String::new()
            } else {
                option.spec.clone()
            },
        });
    }
    let Some(reader) = options.reader.as_ref() else {
        return Err(CompareValidationError::NoRepository);
    };
    match reader.resolve_commit(spec) {
        Ok(Some(oid)) => Ok(DialogRevision {
            side: RepoCompareSide::Commit(oid),
            label: String::new(),
        }),
        Ok(None) | Err(_) => Err(CompareValidationError::UnknownRevision(spec.to_owned())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    File,
    Folder,
}

fn classify_path(path: &Path) -> Result<PathKind, CompareValidationError> {
    let meta =
        fs::metadata(path).map_err(|_| CompareValidationError::PathNotFound(path.to_path_buf()))?;
    if meta.is_file() {
        Ok(PathKind::File)
    } else if meta.is_dir() {
        Ok(PathKind::Folder)
    } else {
        Err(CompareValidationError::NotFileOrFolder(path.to_path_buf()))
    }
}

/// Validates an ordered list of 2 (`[left, right]`) or 3 (`[left, middle, right]`)
/// filesystem paths (such as from `--compare A B [C]`).
pub(crate) fn validate_ordered_paths(
    paths: &[PathBuf],
) -> Result<CompareTarget, CompareValidationError> {
    if !(2..=3).contains(&paths.len()) {
        return Err(CompareValidationError::WrongArgCount(paths.len()));
    }
    let mut kinds = Vec::with_capacity(paths.len());
    for path in paths {
        kinds.push(classify_path(path)?);
    }
    let first_kind = kinds[0];
    if !kinds.iter().all(|kind| *kind == first_kind) {
        return Err(CompareValidationError::MixedTypes);
    }
    match (first_kind, paths.len()) {
        (PathKind::File, 2) => Ok(CompareTarget::TwoWayFiles {
            left: paths[0].clone(),
            right: paths[1].clone(),
        }),
        (PathKind::File, 3) => Ok(CompareTarget::ThreeWayFiles {
            left: paths[0].clone(),
            middle: paths[1].clone(),
            right: paths[2].clone(),
        }),
        (PathKind::Folder, 2) => Ok(CompareTarget::TwoWayFolders {
            left: paths[0].clone(),
            right: paths[1].clone(),
        }),
        (PathKind::Folder, 3) => Ok(CompareTarget::ThreeWayFolders {
            left: paths[0].clone(),
            middle: paths[1].clone(),
            right: paths[2].clone(),
        }),
        _ => unreachable!(),
    }
}

/// Validates the setup dialog's three text fields (`left`, `right`, and optional
/// `middle`). When `middle` is blank, performs a 2-way comparison (`left` vs
/// `right`); when filled, compares `left | middle | right`.
pub(crate) fn validate_dialog_inputs(
    left: &str,
    right: &str,
    middle: &str,
) -> Result<CompareTarget, CompareValidationError> {
    let left = left.trim();
    let right = right.trim();
    let middle = middle.trim();
    if left.is_empty() || right.is_empty() {
        return Err(CompareValidationError::MissingRequired);
    }
    let paths = if middle.is_empty() {
        vec![PathBuf::from(left), PathBuf::from(right)]
    } else {
        vec![
            PathBuf::from(left),
            PathBuf::from(middle),
            PathBuf::from(right),
        ]
    };
    validate_ordered_paths(&paths)
}

// ---------------------------------------------------------------------------
// Compare setup dialog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum DialogField {
    Left,
    Right,
    Middle,
}

/// The dialog's dismiss button. Cancel is never the action the dialog wants
/// the user to take, so it stays the ordinary kind even when Compare is
/// disabled and the primary button is not available.
fn dialog_cancel_button() -> ui::UiButton {
    ui::button("compare-dialog-cancel", t!("common.cancel").to_string())
        .kind(ButtonKind::Normal)
        .size(ButtonSize::Medium)
        .on_click(|_, window, cx| window.close_dialog(cx))
}

/// Opens the modal Compare setup dialog over the workspace window.
pub(crate) fn open_compare_dialog(
    workspace: Entity<Workspace>,
    options: RevisionOptions,
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let dialog = cx.new(|cx| CompareDialog::new(workspace.downgrade(), options, window, cx));
    let focus = dialog.read(cx).left_focus(cx);
    let content = dialog.clone();
    let submit_dialog = dialog.clone();
    let ok_dialog = dialog;
    window.open_dialog(cx, move |card, _, cx| {
        let content = content.clone();
        let submit_dialog = submit_dialog.clone();
        let ok_dialog = ok_dialog.clone();
        let can_submit = content.read(cx).validated(cx).is_ok();
        let zoom = Settings::get(cx).appearance.zoom();
        card.title(t!("compare.dialog.title").to_string())
            .w(px(640.0 * zoom))
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                DialogFooter::new().child(dialog_cancel_button()).child(
                    ui::button(
                        "compare-dialog-submit",
                        t!("compare.dialog.compare").to_string(),
                    )
                    .size(ButtonSize::Medium)
                    .disabled(!can_submit)
                    .on_click(move |_, window, cx| {
                        submit_dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                    }),
                ),
            )
            .on_ok(move |_, window, cx| {
                ok_dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                false
            })
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

/// Which kind of comparison the dialog is set up for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogMode {
    Paths,
    Revisions,
}

struct CompareDialog {
    workspace: WeakEntity<Workspace>,
    mode: DialogMode,
    left: Entity<InputState>,
    right: Entity<InputState>,
    middle: Entity<InputState>,
    rev_left: Entity<InputState>,
    rev_right: Entity<InputState>,
    rev_middle: Entity<InputState>,
    options: RevisionOptions,
    _subscriptions: Vec<Subscription>,
}

impl CompareDialog {
    fn new(
        workspace: WeakEntity<Workspace>,
        options: RevisionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let left = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("compare.dialog.left_placeholder").to_string())
        });
        let right = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("compare.dialog.right_placeholder").to_string())
        });
        let middle = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("compare.dialog.middle_placeholder").to_string())
        });
        let revision_placeholder = t!("compare.dialog.revision_placeholder").to_string();
        let rev_left =
            cx.new(|cx| InputState::new(window, cx).placeholder(revision_placeholder.clone()));
        let rev_right =
            cx.new(|cx| InputState::new(window, cx).placeholder(revision_placeholder.clone()));
        let rev_middle =
            cx.new(|cx| InputState::new(window, cx).placeholder(revision_placeholder.clone()));

        let mode = DialogMode::Paths;
        let mut subscriptions = Vec::new();
        for input in [&left, &right, &middle, &rev_left, &rev_right, &rev_middle] {
            let sub =
                cx.subscribe_in(
                    input,
                    window,
                    |this, _, event: &InputEvent, window, cx| match event {
                        InputEvent::Change => cx.notify(),
                        InputEvent::PressEnter { .. } => this.submit(window, cx),
                        _ => {}
                    },
                );
            subscriptions.push(sub);
        }

        Self {
            workspace,
            mode,
            left,
            right,
            middle,
            rev_left,
            rev_right,
            rev_middle,
            options,
            _subscriptions: subscriptions,
        }
    }

    fn left_focus(&self, cx: &App) -> FocusHandle {
        self.left.focus_handle(cx)
    }

    fn current_inputs(&self) -> [&Entity<InputState>; 3] {
        match self.mode {
            DialogMode::Paths => [&self.left, &self.right, &self.middle],
            DialogMode::Revisions => [&self.rev_left, &self.rev_right, &self.rev_middle],
        }
    }

    fn validated(&self, cx: &App) -> Result<CompareDialogRequest, CompareValidationError> {
        match self.mode {
            DialogMode::Paths => validate_dialog_inputs(
                self.left.read(cx).value().as_ref(),
                self.right.read(cx).value().as_ref(),
                self.middle.read(cx).value().as_ref(),
            )
            .map(CompareDialogRequest::Paths),
            DialogMode::Revisions => validate_revision_inputs(
                self.rev_left.read(cx).value().as_ref(),
                self.rev_right.read(cx).value().as_ref(),
                self.rev_middle.read(cx).value().as_ref(),
                &self.options,
            )
            .map(CompareDialogRequest::Revisions),
        }
    }

    fn set_mode(&mut self, mode: DialogMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        cx.notify();
    }

    fn browse(&mut self, field: DialogField, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = match field {
            DialogField::Left => t!("compare.dialog.browse_left").to_string(),
            DialogField::Right => t!("compare.dialog.browse_right").to_string(),
            DialogField::Middle => t!("compare.dialog.browse_middle").to_string(),
        };
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        let input = match field {
            DialogField::Left => self.left.clone(),
            DialogField::Right => self.right.clone(),
            DialogField::Middle => self.middle.clone(),
        };
        let entity = cx.weak_entity();
        window
            .spawn(cx, async move |cx| {
                let Ok(Ok(Some(paths))) = receiver.await else {
                    return;
                };
                let Some(chosen) = paths.into_iter().next() else {
                    return;
                };
                let text = chosen.display().to_string();
                let _ = cx.update(|window, cx| {
                    input.update(cx, |state, cx| state.set_value(text, window, cx));
                    let _ = entity.update(cx, |_, cx| cx.notify());
                });
            })
            .detach();
    }

    fn swap_left_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let [left, right, _] = self.current_inputs();
        let a = left.read(cx).value().to_string();
        let b = right.read(cx).value().to_string();
        left.update(cx, |state, cx| state.set_value(b, window, cx));
        right.update(cx, |state, cx| state.set_value(a, window, cx));
        cx.notify();
    }

    fn swap_right_middle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let [_, right, middle] = self.current_inputs();
        let b = right.read(cx).value().to_string();
        let c = middle.read(cx).value().to_string();
        right.update(cx, |state, cx| state.set_value(c, window, cx));
        middle.update(cx, |state, cx| state.set_value(b, window, cx));
        cx.notify();
    }

    fn clear_middle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let [_, _, middle] = self.current_inputs();
        middle.update(cx, |state, cx| state.set_value(String::new(), window, cx));
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(request) = self.validated(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| match request {
            CompareDialogRequest::Paths(target) => {
                workspace.open_compare_target(target, window, cx);
            }
            CompareDialogRequest::Revisions(revisions) => {
                workspace.open_compare_revisions(revisions, window, cx);
            }
        });
    }

    fn path_row(
        &self,
        id: &'static str,
        label: String,
        input: &Entity<InputState>,
        field: DialogField,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let clearable = matches!(field, DialogField::Middle);
        let has_value = !input.read(cx).value().trim().is_empty();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(label),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(input).w_full().bg(design.field)),
                    )
                    .when(clearable && has_value, |row| {
                        row.child(
                            ui::icon_button(format!("{id}-clear"), IconName::X)
                                .ghost()
                                .size(ButtonSize::Small)
                                .tooltip(t!("compare.dialog.clear_middle").to_string())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.clear_middle(window, cx);
                                })),
                        )
                    })
                    .child(
                        ui::button(
                            format!("{id}-browse"),
                            t!("compare.dialog.browse").to_string(),
                        )
                        .kind(ButtonKind::Normal)
                        .size(ButtonSize::Small)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.browse(field, window, cx);
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    fn revision_row(
        &self,
        id: &'static str,
        label: String,
        input: &Entity<InputState>,
        clearable: bool,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_value = !input.read(cx).value().trim().is_empty();
        let choices = self.options.options.clone();
        let pick_input = input.clone();
        let pick_entity = cx.weak_entity();
        let picker_design = design.clone();
        let tooltip = t!("compare.dialog.revision_picker").to_string();
        let picker = Popover::new(format!("compare-revision-picker-{id}"))
            .anchor(Anchor::TopRight)
            .offset(px(4.0))
            .trigger(
                ui::icon_button(format!("{id}-pick"), IconName::ChevronDown)
                    .size(ButtonSize::Small)
                    .ghost()
                    .tooltip(tooltip),
            )
            .content(move |_, _window, cx| {
                let popover = cx.entity();
                let mut list = div()
                    .id(ElementId::Name(
                        format!("compare-revision-list-{id}").into(),
                    ))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .w(px(320.0))
                    .max_h(px(300.0))
                    .overflow_y_scroll();
                for (index, choice) in choices.iter().enumerate() {
                    let choice = choice.clone();
                    let input = pick_input.clone();
                    let entity = pick_entity.clone();
                    let dismiss = popover.clone();
                    list = list.child(
                        ui::list_row(
                            ElementId::Name(format!("compare-revision-{id}-{index}").into()),
                            ui::RowState::NORMAL,
                            &picker_design,
                        )
                        .h(px(26.0))
                        .mx(px(0.0))
                        .w_full()
                        .cursor_pointer()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(choice.label.clone()),
                        )
                        .on_click(move |_, window, cx| {
                            dismiss.update(cx, |state, cx| state.dismiss(window, cx));
                            input.update(cx, |state, cx| {
                                // The field shows the short id and summary; the
                                // full commit stays in the option's resolved
                                // side, so the comparison opens the same commit.
                                state.set_value(choice.display.clone(), window, cx);
                            });
                            let _ = entity.update(cx, |_, cx| cx.notify());
                        }),
                    );
                }
                list
            });

        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(label),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(input).w_full().bg(design.field)),
                    )
                    .when(clearable && has_value, |row| {
                        row.child(
                            ui::icon_button(format!("{id}-clear"), IconName::X)
                                .ghost()
                                .size(ButtonSize::Small)
                                .tooltip(t!("compare.dialog.clear_middle").to_string())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.clear_middle(window, cx);
                                })),
                        )
                    })
                    .child(picker),
            )
            .into_any_element()
    }
}

impl Render for CompareDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let validation = self.validated(cx);
        let error_message = validation.err().and_then(|err| err.message());
        let mode = self.mode;
        let has_repository = self.options.has_repository();

        let mut column = div().flex().flex_col().gap_3().w_full();
        if has_repository {
            column = column.child(
                ui::segmented_control("compare-dialog-mode", &design)
                    .segment(
                        "compare-dialog-mode-paths",
                        t!("compare.dialog.mode_paths").to_string(),
                        mode == DialogMode::Paths,
                        cx.listener(|this, _, _, cx| this.set_mode(DialogMode::Paths, cx)),
                    )
                    .segment(
                        "compare-dialog-mode-revisions",
                        t!("compare.dialog.mode_revisions").to_string(),
                        mode == DialogMode::Revisions,
                        cx.listener(|this, _, _, cx| this.set_mode(DialogMode::Revisions, cx)),
                    ),
            );
        }

        let (left_label, right_label, middle_label) = match mode {
            DialogMode::Paths => (
                t!("compare.dialog.left_label").to_string(),
                t!("compare.dialog.right_label").to_string(),
                t!("compare.dialog.middle_label").to_string(),
            ),
            DialogMode::Revisions => (
                t!("compare.dialog.revision_left_label").to_string(),
                t!("compare.dialog.revision_right_label").to_string(),
                t!("compare.dialog.revision_middle_label").to_string(),
            ),
        };
        let (left_input, right_input, middle_input) = match mode {
            DialogMode::Paths => (&self.left, &self.right, &self.middle),
            DialogMode::Revisions => (&self.rev_left, &self.rev_right, &self.rev_middle),
        };

        let row = |id: &'static str,
                   label: String,
                   input: &Entity<InputState>,
                   field: DialogField,
                   cx: &mut Context<Self>| match mode {
            DialogMode::Paths => self.path_row(id, label, input, field, &design, cx),
            DialogMode::Revisions => self.revision_row(
                id,
                label,
                input,
                matches!(field, DialogField::Middle),
                &design,
                cx,
            ),
        };

        column
            .child(row(
                "compare-left",
                left_label,
                left_input,
                DialogField::Left,
                cx,
            ))
            .child(
                div().flex().items_center().justify_end().gap_1p5().child(
                    ui::button(
                        "compare-swap-lr",
                        t!("compare.dialog.swap_left_right").to_string(),
                    )
                    .ghost()
                    .size(ButtonSize::Tiny)
                    .icon(IconName::Replace)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.swap_left_right(window, cx);
                    })),
                ),
            )
            .child(row(
                "compare-right",
                right_label,
                right_input,
                DialogField::Right,
                cx,
            ))
            .child(
                div().flex().items_center().justify_end().gap_1p5().child(
                    ui::button(
                        "compare-swap-rm",
                        t!("compare.dialog.swap_right_middle").to_string(),
                    )
                    .ghost()
                    .size(ButtonSize::Tiny)
                    .icon(IconName::Replace)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.swap_right_middle(window, cx);
                    })),
                ),
            )
            .child(row(
                "compare-middle",
                middle_label,
                middle_input,
                DialogField::Middle,
                cx,
            ))
            .when(mode == DialogMode::Revisions, |col| {
                col.child(
                    div()
                        .text_size(px(design.tiny()))
                        .text_color(design.muted)
                        .child(t!("compare.dialog.revision_hint").to_string()),
                )
            })
            .when_some(error_message, |col, msg| {
                col.child(ui::banner(ui::BannerKind::Danger, msg, &design))
            })
    }
}

// ---------------------------------------------------------------------------
// Compare workspace tab view
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FolderFilter {
    All,
    Different,
    OnlyLeft,
    OnlyMiddle,
    OnlyRight,
    Identical,
    TypeMismatch,
    /// Peer-mode chips: which adjacency differs across three equal sides.
    LeftMiddleDiffer,
    MiddleRightDiffer,
    AllDifferent,
    MissingSomeSide,
}

impl FolderFilter {
    fn matches(self, status: FolderEntryStatus) -> bool {
        match self {
            FolderFilter::All => true,
            FolderFilter::Different => status == FolderEntryStatus::Different,
            FolderFilter::OnlyLeft => status == FolderEntryStatus::OnlyLeft,
            FolderFilter::OnlyMiddle => status == FolderEntryStatus::OnlyMiddle,
            FolderFilter::OnlyRight => status == FolderEntryStatus::OnlyRight,
            FolderFilter::Identical => status == FolderEntryStatus::Identical,
            FolderFilter::TypeMismatch => status == FolderEntryStatus::TypeMismatch,
            FolderFilter::LeftMiddleDiffer
            | FolderFilter::MiddleRightDiffer
            | FolderFilter::AllDifferent
            | FolderFilter::MissingSomeSide => false,
        }
    }

    fn matches_peer(self, peer: ThreeWayPeerStatus) -> bool {
        match self {
            FolderFilter::All => true,
            FolderFilter::LeftMiddleDiffer => matches!(
                peer,
                ThreeWayPeerStatus::LeftMiddleDiffer | ThreeWayPeerStatus::AllDifferent
            ),
            FolderFilter::MiddleRightDiffer => matches!(
                peer,
                ThreeWayPeerStatus::MiddleRightDiffer | ThreeWayPeerStatus::AllDifferent
            ),
            FolderFilter::AllDifferent => peer == ThreeWayPeerStatus::AllDifferent,
            FolderFilter::MissingSomeSide => peer == ThreeWayPeerStatus::MissingSomeSide,
            FolderFilter::Different
            | FolderFilter::OnlyLeft
            | FolderFilter::OnlyMiddle
            | FolderFilter::OnlyRight
            | FolderFilter::Identical
            | FolderFilter::TypeMismatch => false,
        }
    }
}

enum CompareState {
    Loading,
    Error(String),
    TwoWayFile(Box<TwoWayFileCompare>),
    ThreeWayFile(Box<ThreeWayFileCompare>),
    Folder(FolderCompareResult),
}

enum BackgroundOutput {
    TwoWayFile(Result<Option<TwoWayFileCompare>, String>),
    ThreeWayFile(Result<Option<ThreeWayFileCompare>, String>),
    Folder(Result<Option<FolderCompareResult>, String>),
    RepoTree(Result<Option<ThreeWayTreeCompare>, String>),
    RepoPairTree(Result<Option<RepoCompareTree>, String>),
}

pub(crate) struct CompareView {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    active: bool,
    root: CompareRoot,
    respect_gitignore: bool,
    state: CompareState,
    /// Cached folder comparison result preserved while inspecting one file so
    /// clicking "Back" returns immediately without rescanning.
    folder_cache: Option<FolderCompareResult>,
    /// The revision tree the current folder list came from, kept so "Back"
    /// restores the change classification without re-reading the trees.
    repo_tree: Option<Box<ThreeWayTreeCompare>>,
    /// Classification per folder-list entry index while a revision tree is
    /// shown.
    repo_marks: Option<Rc<Vec<RepoTreeMark>>>,
    /// Relative path inside `folder_cache` when drilled into a file.
    drilled_relative_path: Option<String>,
    cancel_flag: Option<Arc<AtomicBool>>,
    generation: u64,
    // Two-way diff state
    diff_context: DiffContext,
    diff_rows: Rc<Vec<DiffRow>>,
    two_way_scroll: UniformListScrollHandle,
    image_state: image_diff::State,
    // Three-way diff state
    three_way_scroll: UniformListScrollHandle,
    /// One horizontal offset shared by all three text panes, so a wide line
    /// scrolls to the same column in every pane.
    three_way_h_scroll: ScrollHandle,
    /// Widest line of the current three-way comparison, in font columns.
    three_way_columns: usize,
    three_way_image_state: image_diff::State,
    three_way_svg_rendered: bool,
    active_change: Option<usize>,
    // Folder list state
    folder_scroll: UniformListScrollHandle,
    folder_filter: FolderFilter,
    filtered_indices: Vec<usize>,
    selected_folder_entry: Option<usize>,
    /// Counts of the peer statuses of the current revision tree.
    peer_counts: PeerTreeCounts,
    /// How a three-way comparison reads its middle side, kept per tab.
    mode: ThreeWayCompareMode,
}

impl Drop for CompareView {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl CompareView {
    pub(crate) fn new(
        target: CompareTarget,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mode = CompareRoot::Paths(target.clone()).default_mode();
        Self::new_with_root(CompareRoot::Paths(target), mode, workspace, cx)
    }

    /// A comparison of three repository sides. Reads go through the source's
    /// reader on the background executor, like a filesystem compare.
    pub(crate) fn new_revisions(
        source: RepoThreeWaySource,
        mode: ThreeWayCompareMode,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_root(
            CompareRoot::Revisions(Box::new(source)),
            mode,
            workspace,
            cx,
        )
    }

    /// A comparison of two repository sides.
    pub(crate) fn new_revision_pair(
        source: RepoTwoWaySource,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_root(
            CompareRoot::RevisionPair(Box::new(source)),
            ThreeWayCompareMode::MiddleBase,
            workspace,
            cx,
        )
    }

    /// One file of three repository sides, where each side may know the file
    /// under a different path because a history walk followed renames.
    pub(crate) fn new_revision_file(
        source: RepoThreeWayFileSource,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_root(
            CompareRoot::RevisionFile(Box::new(source)),
            ThreeWayCompareMode::NoBase,
            workspace,
            cx,
        )
    }

    fn new_with_root(
        root: CompareRoot,
        mode: ThreeWayCompareMode,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            active: true,
            root,
            respect_gitignore: true,
            state: CompareState::Loading,
            folder_cache: None,
            repo_tree: None,
            repo_marks: None,
            drilled_relative_path: None,
            cancel_flag: None,
            generation: 0,
            diff_context: DiffContext::default(),
            diff_rows: Rc::new(Vec::new()),
            two_way_scroll: UniformListScrollHandle::default(),
            image_state: image_diff::State::new(),
            three_way_scroll: UniformListScrollHandle::default(),
            three_way_h_scroll: ScrollHandle::default(),
            three_way_columns: 0,
            three_way_image_state: image_diff::State::new(),
            three_way_svg_rendered: false,
            active_change: None,
            folder_scroll: UniformListScrollHandle::default(),
            folder_filter: FolderFilter::All,
            filtered_indices: Vec::new(),
            selected_folder_entry: None,
            peer_counts: PeerTreeCounts::default(),
            mode,
        };
        view.start_root(cx);
        view
    }

    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn view_focus(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub(crate) fn tab_title(&self) -> String {
        match &self.drilled_relative_path {
            Some(rel) => format!("{} — {}", path_label(Path::new(rel)), self.root.title()),
            None => self.root.title(),
        }
    }

    pub(crate) fn window_title(&self) -> String {
        format!("{} — {}", t!("compare.tab.prefix"), self.tab_title())
    }

    pub(crate) fn hint_path(&self) -> Option<&Path> {
        self.root.hint_path()
    }

    /// The three pane labels while a repository comparison is shown.
    #[cfg(test)]
    pub(crate) fn pane_labels(&self) -> Option<Vec<String>> {
        self.root
            .revisions()
            .map(|source| source.pane_labels().to_vec())
    }

    pub(crate) fn is_loading(&self) -> bool {
        matches!(self.state, CompareState::Loading)
    }

    /// Horizontal offset of the three-way text panes, for tests that drive the
    /// shared scroll handle.
    #[cfg(test)]
    pub(crate) fn horizontal_scroll_offset(&self) -> f32 {
        f32::from(self.three_way_h_scroll.offset().x)
    }

    /// Vertical offset of the three-way row list. Tests use this to tell a
    /// horizontal pan apart from the rows moving.
    #[cfg(test)]
    pub(crate) fn vertical_scroll_offset(&self) -> f32 {
        f32::from(self.three_way_scroll.0.borrow().base_handle.offset().y)
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(flag) = self.cancel_flag.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }

    fn start_root(&mut self, cx: &mut Context<Self>) {
        let job = match &self.root {
            CompareRoot::Paths(target) => CompareJob::from_target(target, self.respect_gitignore),
            CompareRoot::Revisions(source) => CompareJob::RepoTree((**source).clone()),
            CompareRoot::RevisionPair(source) => CompareJob::RepoPairTree((**source).clone()),
            CompareRoot::RevisionFile(source) => CompareJob::RepoFileSides {
                source: source.source.clone(),
                paths: source.paths.clone(),
            },
        };
        self.start_job(job, cx);
    }

    /// The mode the three-way panes currently use.
    #[cfg(test)]
    pub(crate) fn mode(&self) -> ThreeWayCompareMode {
        self.mode
    }

    /// Whether the file panes show a three-way comparison, for tests that open
    /// one directly from history.
    #[cfg(test)]
    pub(crate) fn is_three_way_file_view(&self) -> bool {
        matches!(self.state, CompareState::ThreeWayFile(_))
    }

    /// Switches how a three-way comparison reads its middle side.
    ///
    /// No re-read is needed: the loaded rows and entries carry both the
    /// base-relative and the peer classification, so only the presentation and
    /// the filters change.
    pub(crate) fn set_mode(&mut self, mode: ThreeWayCompareMode, cx: &mut Context<Self>) {
        if self.mode == mode || !self.root.is_three_way() {
            return;
        }
        self.mode = mode;
        self.active_change = None;
        self.folder_filter = FolderFilter::All;
        self.rebuild_folder_filter();
        cx.notify();
    }

    fn start_job(&mut self, job: CompareJob, cx: &mut Context<Self>) {
        self.cancel();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(Arc::clone(&cancel));
        self.state = CompareState::Loading;
        self.diff_context.reset();
        self.diff_rows = Rc::new(Vec::new());
        self.active_change = None;
        self.three_way_svg_rendered = false;
        self.three_way_h_scroll = ScrollHandle::default();
        self.three_way_columns = 0;
        self.repo_marks = None;
        self.peer_counts = PeerTreeCounts::default();
        self.refresh_workspace_tab_strip(cx);
        cx.notify();

        let background = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let output = background.spawn(async move { job.run(&cancel) }).await;
            let _ = this.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                view.cancel_flag = None;
                view.apply_output(output, cx);
            });
        })
        .detach();
    }

    fn refresh_workspace_tab_strip(&self, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        // Defer so callers constructing CompareView inside a Workspace update do not double-lease Workspace.
        cx.defer(move |cx| {
            if let Some(ws) = workspace.upgrade() {
                ws.update(cx, |_, cx| cx.notify());
            }
        });
    }

    fn apply_output(&mut self, output: BackgroundOutput, cx: &mut Context<Self>) {
        match output {
            BackgroundOutput::TwoWayFile(Ok(Some(res))) => {
                let rows = self.diff_context.rows_parts(
                    &res.diff,
                    &res.left_text,
                    &res.right_text,
                    &res.gaps,
                );
                self.diff_rows = Rc::new(rows);
                self.state = CompareState::TwoWayFile(Box::new(res));
            }
            BackgroundOutput::ThreeWayFile(Ok(Some(res))) => {
                let tab_width = usize::from(Settings::get(cx).appearance.tab_width.max(1));
                self.three_way_columns = three_way_max_columns(
                    &res.left_lines,
                    &res.middle_lines,
                    &res.right_lines,
                    tab_width,
                );
                self.state = CompareState::ThreeWayFile(Box::new(res));
            }
            BackgroundOutput::Folder(Ok(Some(res))) => {
                self.peer_counts = peer_counts_from_entries(&res.entries);
                self.folder_cache = Some(res.clone());
                self.state = CompareState::Folder(res);
                self.rebuild_folder_filter();
            }
            BackgroundOutput::RepoTree(Ok(Some(tree))) => {
                self.apply_repo_tree(tree);
            }
            BackgroundOutput::RepoPairTree(Ok(Some(tree))) => {
                self.apply_repo_pair_tree(tree);
            }
            BackgroundOutput::TwoWayFile(Ok(None))
            | BackgroundOutput::ThreeWayFile(Ok(None))
            | BackgroundOutput::Folder(Ok(None))
            | BackgroundOutput::RepoTree(Ok(None))
            | BackgroundOutput::RepoPairTree(Ok(None)) => {
                // Cancelled; keep existing or loading state superseded by the next job.
            }
            BackgroundOutput::TwoWayFile(Err(err))
            | BackgroundOutput::ThreeWayFile(Err(err))
            | BackgroundOutput::Folder(Err(err))
            | BackgroundOutput::RepoTree(Err(err))
            | BackgroundOutput::RepoPairTree(Err(err)) => {
                self.state = CompareState::Error(err);
            }
        }
        self.refresh_workspace_tab_strip(cx);
        cx.notify();
    }

    fn apply_repo_tree(&mut self, tree: ThreeWayTreeCompare) {
        let Some(source) = self.root.revisions().cloned() else {
            return;
        };
        let folder = folder_result_from_tree(&tree, &source);
        self.set_repo_marks(&tree);
        self.repo_tree = Some(Box::new(tree));
        self.folder_cache = Some(folder.clone());
        self.state = CompareState::Folder(folder);
        self.rebuild_folder_filter();
    }

    fn apply_repo_pair_tree(&mut self, tree: RepoCompareTree) {
        let folder = folder_result_from_pair(&tree);
        self.repo_tree = None;
        self.repo_marks = None;
        self.peer_counts = PeerTreeCounts::default();
        self.folder_cache = Some(folder.clone());
        self.state = CompareState::Folder(folder);
        self.rebuild_folder_filter();
    }

    fn set_repo_marks(&mut self, tree: &ThreeWayTreeCompare) {
        let marks: Vec<RepoTreeMark> = tree
            .entries
            .iter()
            .map(|entry| RepoTreeMark {
                change: Some(entry.change),
                peer: Some(entry.peer),
                conflicting: entry.conflicting,
            })
            .collect();
        let mut counts = PeerTreeCounts::default();
        for mark in &marks {
            if let Some(peer) = mark.peer {
                counts.record(peer);
            }
        }
        self.peer_counts = counts;
        self.repo_marks = Some(Rc::new(marks));
    }

    fn rebuild_folder_filter(&mut self) {
        let CompareState::Folder(res) = &self.state else {
            self.filtered_indices.clear();
            return;
        };
        let peer_mode = self.mode == ThreeWayCompareMode::NoBase && self.root.is_three_way();
        let marks = self.repo_marks.clone();
        self.filtered_indices = res
            .entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let matched = if peer_mode {
                    let peer = marks
                        .as_ref()
                        .and_then(|marks| marks.get(idx))
                        .and_then(|mark| mark.peer)
                        .unwrap_or(entry.peer);
                    self.folder_filter.matches_peer(peer)
                } else {
                    self.folder_filter.matches(entry.status)
                };
                matched.then_some(idx)
            })
            .collect();
        self.selected_folder_entry = None;
    }

    pub(crate) fn open_folder_entry(
        &mut self,
        entry_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(folder) = self.folder_cache.as_ref() else {
            return;
        };
        let Some(entry) = folder.entries.get(entry_index) else {
            return;
        };
        if entry.kind != FolderEntryKind::File {
            return;
        }
        let rel = entry.relative_path.clone();
        self.selected_folder_entry = Some(entry_index);

        if let Some(source) = self.root.revisions().cloned() {
            self.drilled_relative_path = Some(rel.clone());
            self.start_job(CompareJob::RepoFile { source, path: rel }, cx);
            self.sync_window_title(window, cx);
            return;
        }

        if let Some(source) = self.root.pair().cloned() {
            self.drilled_relative_path = Some(rel.clone());
            self.start_job(CompareJob::RepoPairFile { source, path: rel }, cx);
            self.sync_window_title(window, cx);
            return;
        }

        let side_path = |side_idx: usize| -> Option<PathBuf> {
            let side = entry.sides.get(side_idx)?.as_ref()?;
            if side.kind != FolderEntryKind::File {
                return None;
            }
            Some(folder.roots.get(side_idx)?.join(&rel))
        };
        let job = if folder.roots.len() == 2 {
            CompareJob::TwoWayOptionalFiles {
                left: side_path(0),
                right: side_path(1),
            }
        } else {
            CompareJob::ThreeWayOptionalFiles {
                left: side_path(0),
                middle: side_path(1),
                right: side_path(2),
            }
        };
        self.drilled_relative_path = Some(rel);
        self.start_job(job, cx);
        self.sync_window_title(window, cx);
    }

    pub(crate) fn back_to_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cached) = self.folder_cache.clone() else {
            return;
        };
        self.cancel();
        self.generation = self.generation.wrapping_add(1);
        self.drilled_relative_path = None;
        self.diff_rows = Rc::new(Vec::new());
        self.active_change = None;
        self.state = CompareState::Folder(cached);
        if let Some(tree) = &self.repo_tree {
            let tree = tree.clone();
            self.set_repo_marks(&tree);
        }
        self.rebuild_folder_filter();
        self.refresh_workspace_tab_strip(cx);
        self.sync_window_title(window, cx);
        cx.notify();
    }

    fn toggle_gitignore(&mut self, cx: &mut Context<Self>) {
        self.respect_gitignore = !self.respect_gitignore;
        self.folder_cache = None;
        self.drilled_relative_path = None;
        self.start_root(cx);
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(rel) = self.drilled_relative_path.clone()
            && let Some(folder) = self.folder_cache.as_ref()
            && let Some(entry_idx) = folder
                .entries
                .iter()
                .position(|entry| entry.relative_path == rel)
        {
            self.open_folder_entry(entry_idx, window, cx);
            return;
        }
        self.start_root(cx);
    }

    /// Swaps the middle and the chosen side, so the comparison is re-read with
    /// the other commit as the base.
    fn use_side_as_base(&mut self, side: BaseChoice, window: &mut Window, cx: &mut Context<Self>) {
        {
            let CompareRoot::Revisions(source) = &mut self.root else {
                return;
            };
            let already = match side {
                BaseChoice::Left => source.middle.side == source.left.side,
                BaseChoice::Right => source.middle.side == source.right.side,
            };
            if already {
                return;
            }
            match side {
                BaseChoice::Left => std::mem::swap(&mut source.left, &mut source.middle),
                BaseChoice::Right => std::mem::swap(&mut source.right, &mut source.middle),
            }
        }
        self.folder_cache = None;
        self.repo_tree = None;
        self.repo_marks = None;
        self.drilled_relative_path = None;
        self.selected_folder_entry = None;
        self.start_root(cx);
        self.sync_window_title(window, cx);
    }

    /// Re-applies this tab's window title after drilling into a file or going
    /// back, through the workspace so an inactive tab never replaces it.
    ///
    /// Deferred: the workspace reads this view's title, which it cannot do
    /// while the drill-in update still holds it.
    fn sync_window_title(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.refresh_window_title(window, cx);
            });
        });
    }

    fn expand_two_way_gap(&mut self, gap: usize, action: GapAction, cx: &mut Context<Self>) {
        let CompareState::TwoWayFile(res) = &self.state else {
            return;
        };
        if self
            .diff_context
            .expand_parts(gap, action, &res.gaps, &res.left_text, &res.right_text)
        {
            let rows =
                self.diff_context
                    .rows_parts(&res.diff, &res.left_text, &res.right_text, &res.gaps);
            self.diff_rows = Rc::new(rows);
            self.active_change = None;
            cx.notify();
        } else if self.diff_context.notice().is_some() {
            cx.notify();
        }
    }

    fn toggle_two_way_whole_file(&mut self, cx: &mut Context<Self>) {
        let CompareState::TwoWayFile(res) = &self.state else {
            return;
        };
        if self
            .diff_context
            .toggle_whole_file_parts(&res.left_text, &res.right_text)
        {
            let rows =
                self.diff_context
                    .rows_parts(&res.diff, &res.left_text, &res.right_text, &res.gaps);
            self.diff_rows = Rc::new(rows);
            self.active_change = None;
            cx.notify();
        } else if self.diff_context.notice().is_some() {
            cx.notify();
        }
    }

    fn jump_two_way_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        let targets = diff_view::hunk_scroll_targets(&self.diff_rows, cx);
        if targets.is_empty() {
            return;
        }
        let next = step_index(self.active_change, targets.len(), forward);
        self.active_change = Some(next);
        self.two_way_scroll
            .scroll_to_item(targets[next], ScrollStrategy::Center);
        cx.notify();
    }

    fn jump_three_way_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        let CompareState::ThreeWayFile(res) = &self.state else {
            return;
        };
        let rows: Vec<usize> = res
            .change_regions()
            .filter_map(|region| (!region.row_range.is_empty()).then_some(region.row_range.start))
            .collect();
        if rows.is_empty() {
            return;
        }
        let next = step_index(self.active_change, rows.len(), forward);
        self.active_change = Some(next);
        self.three_way_scroll
            .scroll_to_item(rows[next], ScrollStrategy::Center);
        cx.notify();
    }

    fn header(&self, design: &Design, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = self.tab_title();
        // The paths are too wide to sit beside the title at any useful window
        // size, so they only appear in the tooltip.
        let path = self.root.subtitle();
        let has_back = self.drilled_relative_path.is_some();
        let fit = CompareHeaderFit::new(
            f32::from(window.viewport_size().width),
            has_back,
            design.zoom,
        );

        let back = has_back.then(|| {
            let label = t!("compare.back_to_folder").to_string();
            let button = if fit.back_label {
                ui::button("compare-back-folder", label.clone())
                    .ghost()
                    .size(ButtonSize::Small)
                    .icon(IconName::ArrowLeft)
            } else {
                // The arrow keeps the way back; the tooltip names where it
                // leads.
                ui::icon_button("compare-back-folder", IconName::ArrowLeft)
                    .ghost()
                    .size(ButtonSize::Small)
            };
            button
                .tooltip(label)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.back_to_folder(window, cx);
                }))
                .into_any_element()
        });

        let new_compare = if fit.new_compare_label {
            ui::button("compare-new", t!("compare.new_compare").to_string())
                .ghost()
                .size(ButtonSize::Small)
                .icon(IconName::Replace)
        } else {
            ui::icon_button("compare-new", IconName::Replace)
                .ghost()
                .size(ButtonSize::Small)
        };

        let actions = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(design.px(COMPARE_BUTTON_GAP))
            .occlude()
            .debug_selector(|| "compare-header-controls".to_string())
            .child(
                ui::icon_button("compare-refresh", IconName::RefreshCw)
                    .ghost()
                    .size(ButtonSize::Small)
                    .tooltip(t!("compare.refresh").to_string())
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
            .child(
                new_compare
                    .tooltip(t!("compare.new_compare").to_string())
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(ws) = this.workspace.upgrade() {
                            ws.update(cx, |ws, cx| ws.open_compare_dialog(window, cx));
                        }
                    })),
            );

        let tooltip_title: SharedString = title.clone().into();
        let tooltip_path: SharedString = path.into();
        let title_element = div()
            .id("compare-title")
            .flex_1()
            .min_w_0()
            .truncate()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_size(px(design.ui))
            .child(title)
            .tooltip(move |window, cx| {
                let title = tooltip_title.clone();
                let path = tooltip_path.clone();
                Tooltip::element(move |_, _| {
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(title.clone()),
                        )
                        .child(div().child(path.clone()))
                        .into_any_element()
                })
                .build(window, cx)
            });

        TitleBar::new()
            .h(design.px(style::TITLE_BAR_HEIGHT))
            .bg(design.chrome)
            .border_color(design.border)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(design.px(COMPARE_ROW_GAP))
                    .debug_selector(|| "compare-header".to_string())
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(design.px(7.0))
                            .child(style::app_mark(design.scale(style::BRAND_MARK)))
                            .when(fit.wordmark, |mark| {
                                mark.child(style::wordmark(
                                    design.scale(style::BRAND_WORDMARK),
                                    design.text,
                                ))
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(1.0))
                            .h(design.px(18.0))
                            .mx_1()
                            .bg(design.border),
                    )
                    .when_some(back, |row, back| row.child(back))
                    .child(
                        Icon::new(IconName::Replace)
                            .flex_none()
                            .size(design.px(14.0))
                            .text_color(design.text),
                    )
                    .child(title_element)
                    .child(actions),
            )
            .into_any_element()
    }

    /// The "No base" / "Middle as base" choice every three-way view carries.
    fn mode_toggle(&self, design: &Design, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.mode;
        ui::segmented_control("compare-three-way-mode", design)
            .segment(
                "compare-mode-no-base",
                t!("compare.three_way.mode_no_base").to_string(),
                mode == ThreeWayCompareMode::NoBase,
                cx.listener(|this, _, _, cx| {
                    this.set_mode(ThreeWayCompareMode::NoBase, cx);
                }),
            )
            .tooltip(t!("compare.three_way.mode_no_base_tooltip").to_string())
            .segment(
                "compare-mode-middle-base",
                t!("compare.three_way.mode_middle_base").to_string(),
                mode == ThreeWayCompareMode::MiddleBase,
                cx.listener(|this, _, _, cx| {
                    this.set_mode(ThreeWayCompareMode::MiddleBase, cx);
                }),
            )
            .tooltip(t!("compare.three_way.mode_middle_base_tooltip").to_string())
            .into_any_element()
    }

    /// The mode choice with the two swap-in-a-base actions beside it.
    ///
    /// It leads the row under the tab strip: the panes below follow this
    /// control, so it belongs next to them rather than up in the title bar
    /// where the window's own actions live.
    fn mode_choice(&self, design: &Design, cx: &mut Context<Self>) -> AnyElement {
        let mut group = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(design.px(4.0))
            .child(self.mode_toggle(design, cx));

        if self.mode == ThreeWayCompareMode::MiddleBase
            && let Some(source) = self.root.revisions()
        {
            let base_is_left = source.middle.side == source.left.side;
            let base_is_right = source.middle.side == source.right.side;
            group = group
                .child(
                    ui::button(
                        "compare-use-left-base",
                        t!("compare.three_way.use_left_as_base").to_string(),
                    )
                    .ghost()
                    .size(ButtonSize::Tiny)
                    .icon(IconName::Replace)
                    .disabled(base_is_left)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.use_side_as_base(BaseChoice::Left, window, cx);
                    })),
                )
                .child(
                    ui::button(
                        "compare-use-right-base",
                        t!("compare.three_way.use_right_as_base").to_string(),
                    )
                    .ghost()
                    .size(ButtonSize::Tiny)
                    .icon(IconName::Replace)
                    .disabled(base_is_right)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.use_side_as_base(BaseChoice::Right, window, cx);
                    })),
                );
        }

        group.into_any_element()
    }

    fn render_two_way(
        &self,
        res: &TwoWayFileCompare,
        names: Option<[String; 2]>,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let encoding = style::encoding_label_for(res.left_encoding, res.right_encoding);
        let notify = image_diff::notify_entity(cx.entity());
        let svg_toggle =
            image_diff::svg_toggle_for(&self.image_state, res.image.as_ref(), notify.clone());
        let hunk_count = res.diff.hunks.len();

        let subheader = div()
            .flex()
            .items_center()
            .gap_2()
            .h(design.px(38.0))
            .flex_none()
            .px_3p5()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .text_size(design.px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(design.text)
                    .child(match names {
                        Some([left, right]) => format!(
                            "{}  ↔  {}  ·  {}",
                            left,
                            right,
                            display_or_missing(&res.left_path)
                        ),
                        None => format!(
                            "{}  ↔  {}",
                            display_or_missing(&res.left_path),
                            display_or_missing(&res.right_path)
                        ),
                    }),
            )
            .when_some(encoding, |bar, label| {
                bar.child(ui::status_badge(label, BadgeTone::Neutral, design))
            })
            .when_some(svg_toggle, |bar, btn| bar.child(btn))
            .when(!res.diff.binary && !res.too_large, |bar| {
                let whole = self.diff_context.is_whole_file();
                bar.child(
                    div()
                        .flex_none()
                        .text_size(design.px(11.0))
                        .text_color(design.muted)
                        .child(
                            t!(
                                &crate::i18n::count_key("compare.hunks_count", hunk_count),
                                count = hunk_count
                            )
                            .to_string(),
                        ),
                )
                .when(hunk_count > 0, |bar| {
                    bar.child(
                        ui::icon_button("compare-prev-hunk", IconName::ChevronUp)
                            .ghost()
                            .size(ButtonSize::Tiny)
                            .tooltip(t!("compare.prev_change").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.jump_two_way_change(false, cx);
                            })),
                    )
                    .child(
                        ui::icon_button("compare-next-hunk", IconName::ChevronDown)
                            .ghost()
                            .size(ButtonSize::Tiny)
                            .tooltip(t!("compare.next_change").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.jump_two_way_change(true, cx);
                            })),
                    )
                })
                .child(
                    ui::button("compare-whole-file", t!("history.whole_file").to_string())
                        .size(ButtonSize::Tiny)
                        .ghost()
                        .selected(whole)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_two_way_whole_file(cx);
                        })),
                )
            });

        let body: AnyElement = if res.too_large {
            ui::empty_state(t!("compare.file_too_large").to_string(), design).into_any_element()
        } else if let Some(image) = res.image.as_ref()
            && let Some(image_body) =
                image_diff::body_for(&self.image_state, image, None, design, notify, cx)
        {
            image_body
        } else if res.diff.binary {
            ui::empty_state(t!("compare.binary_files").to_string(), design).into_any_element()
        } else if self.diff_rows.is_empty() {
            ui::empty_state(t!("compare.files_identical").to_string(), design).into_any_element()
        } else {
            let entity = cx.weak_entity();
            let on_gap: diff_view::GapHandler = Rc::new(move |gap, action, _window, cx| {
                let _ = entity.update(cx, |this, cx| this.expand_two_way_gap(gap, action, cx));
            });
            let colors = DiffColors::from_design(design);
            let highlights = DiffHighlights {
                old: Arc::clone(&res.left_highlights),
                new: Arc::clone(&res.right_highlights),
            };
            diff_view::diff_list_with_highlights(
                "compare-two-way-diff",
                Rc::clone(&self.diff_rows),
                Some(highlights),
                colors,
                design.mono.clone(),
                &self.two_way_scroll,
                Some(on_gap),
                cx,
            )
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(subheader)
            .when_some(self.diff_context.notice(), |col, notice| {
                col.child(ui::banner(ui::BannerKind::Info, notice.to_owned(), design))
            })
            .child(body)
            .into_any_element()
    }

    fn render_three_way(
        &self,
        res: &ThreeWayFileCompare,
        design: &Design,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mode = self.mode;
        let total_changes = match mode {
            ThreeWayCompareMode::MiddleBase => res.counts().total_changes(),
            ThreeWayCompareMode::NoBase => res.peer_counts().total_changes(),
        };
        let style = DiffStyle::from_settings(cx, &design.mono);
        let diff_colors = DiffColors::from_design(design);
        // Every pane reserves the width of the widest line in the comparison,
        // not just its own, so one shared scroll offset can never be clamped
        // by a pane with shorter lines.
        let content_width =
            crate::style::monospace_advance(window, style.mono(), px(style.font_size()))
                * self.three_way_columns as f32
                + style.px(24.0);
        // One pane's code area: the row is split evenly, and each cell spends
        // the line-number gutter and its border before the code starts. The
        // bar is only drawn when the widest line is wider than that, which is
        // exactly when the panes can scroll horizontally.
        let pane_width = px((f32::from(window.viewport_size().width) - 4.0) / 3.0
            - f32::from(style.px(38.0))
            - f32::from(design.px(2.0)))
        .max(px(1.0));
        let show_h_scrollbar = content_width > pane_width;

        let encoding_badge = [res.left_encoding, res.middle_encoding, res.right_encoding]
            .into_iter()
            .find(|e| !e.is_utf8())
            .map(|e| e.label());

        let is_svg = res
            .images
            .as_ref()
            .is_some_and(|img| img.kind == ImageKind::Svg);

        // The mode choice leads, then the badges, and the change count with
        // its arrows ends the row. Badges are the first to go when the row
        // narrows: they repeat counts the panes already show, while the two
        // ends are controls.
        let mut badges: Vec<(String, BadgeTone)> = three_way_badges(res, mode);
        if let Some(label) = encoding_badge {
            badges.push((label.to_owned(), BadgeTone::Neutral));
        }
        let reserved = COMPARE_TOOLBAR_FIXED_WIDTH
            + if self.mode == ThreeWayCompareMode::MiddleBase && self.root.revisions().is_some() {
                COMPARE_BASE_ACTIONS_WIDTH
            } else {
                0.0
            };
        let shown = visible_badges(
            badges.len(),
            f32::from(window.viewport_size().width),
            design.zoom,
            reserved,
        );

        let subheader = div()
            .flex()
            .items_center()
            .gap_2()
            .h(design.px(38.0))
            .flex_none()
            .px_3p5()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .text_size(design.px(12.0))
            .debug_selector(|| "compare-toolbar".to_string())
            .when(self.root.is_three_way(), |bar| {
                bar.child(self.mode_choice(design, cx))
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1p5()
                    .overflow_hidden()
                    .children(
                        badges
                            .into_iter()
                            .take(shown)
                            .map(|(label, tone)| ui::status_badge(label, tone, design)),
                    ),
            )
            .when(is_svg, |bar| {
                bar.child(
                    ui::button("compare-3way-svg", t!("image_diff.rendered").to_string())
                        .size(ButtonSize::Tiny)
                        .ghost()
                        .selected(self.three_way_svg_rendered)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.three_way_svg_rendered = !this.three_way_svg_rendered;
                            cx.notify();
                        })),
                )
            })
            .when(!res.binary && !res.too_large && total_changes > 0, |bar| {
                let current_label = self
                    .active_change
                    .map(|idx| format!("{} / {}", idx + 1, total_changes))
                    .unwrap_or_else(|| {
                        t!(
                            &crate::i18n::count_key("compare.changes_count", total_changes),
                            count = total_changes
                        )
                        .to_string()
                    });
                bar.child(
                    div()
                        .flex_none()
                        .text_size(design.px(11.0))
                        .text_color(design.muted)
                        .child(current_label),
                )
                .child(
                    ui::icon_button("compare-3way-prev", IconName::ChevronUp)
                        .ghost()
                        .size(ButtonSize::Tiny)
                        .tooltip(t!("compare.prev_change").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.jump_three_way_change(false, cx);
                        })),
                )
                .child(
                    ui::icon_button("compare-3way-next", IconName::ChevronDown)
                        .ghost()
                        .size(ButtonSize::Tiny)
                        .tooltip(t!("compare.next_change").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.jump_three_way_change(true, cx);
                        })),
                )
            });

        let pane_headers = div()
            .flex()
            .w_full()
            .h(design.px(28.0))
            .flex_none()
            .bg(design.panel)
            .border_b_1()
            .border_color(design.border)
            .child(three_way_pane_header(
                t!("compare.side.left").to_string(),
                &res.left_name,
                design,
                true,
            ))
            .child(three_way_pane_header(
                match mode {
                    ThreeWayCompareMode::MiddleBase => t!("compare.side.middle_base").to_string(),
                    ThreeWayCompareMode::NoBase => t!("compare.side.middle").to_string(),
                },
                &res.middle_name,
                design,
                true,
            ))
            .child(three_way_pane_header(
                t!("compare.side.right").to_string(),
                &res.right_name,
                design,
                false,
            ));

        let show_images = res
            .images
            .as_ref()
            .is_some_and(|images| images.kind == ImageKind::Raster || self.three_way_svg_rendered);

        let body: AnyElement = if res.too_large {
            ui::empty_state(t!("compare.file_too_large").to_string(), design).into_any_element()
        } else if show_images && let Some(images) = res.images.as_ref() {
            render_three_way_images(&self.three_way_image_state, images, design)
        } else if res.binary {
            ui::empty_state(t!("compare.binary_files").to_string(), design).into_any_element()
        } else if res.rows.is_empty() {
            ui::empty_state(t!("compare.files_identical").to_string(), design).into_any_element()
        } else {
            let rows = Arc::new(res.rows.clone());
            let left_lines = Arc::clone(&res.left_lines);
            let middle_lines = Arc::clone(&res.middle_lines);
            let right_lines = Arc::clone(&res.right_lines);
            let left_hl = Arc::clone(&res.left_highlights);
            let middle_hl = Arc::clone(&res.middle_highlights);
            let right_hl = Arc::clone(&res.right_highlights);
            let design_copy = design.clone();
            let h_scroll = self.three_way_h_scroll.clone();
            let count = rows.len();

            // A scroller with only vertical overflow copies a pure horizontal
            // wheel delta onto Y. The panes already consume that delta, and
            // they do not stop it, so the rows would walk unless this list
            // refuses the other axis. The helper that sets this lives on
            // stateful elements; a uniform list is not one, but the flag is
            // the same style field.
            let mut list = uniform_list(
                "compare-three-way-list",
                count,
                move |range: std::ops::Range<usize>, _window, _cx| {
                    range
                        .map(|idx| {
                            let row = rows[idx];
                            render_three_way_row(
                                row,
                                mode,
                                &left_lines,
                                &middle_lines,
                                &right_lines,
                                &left_hl,
                                &middle_hl,
                                &right_hl,
                                &diff_colors,
                                &style,
                                &design_copy,
                                content_width,
                                &h_scroll,
                            )
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.three_way_scroll);
            list.style().restrict_scroll_to_axis = Some(true);
            list.with_scrollbar(&self.three_way_scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(subheader)
            .child(pane_headers)
            .child(body)
            .when(show_h_scrollbar, |col| {
                col.child(ui::h_scrollbar(
                    "compare-3way-hscroll",
                    &self.three_way_h_scroll,
                    pane_width,
                    content_width,
                    design,
                ))
            })
            .into_any_element()
    }

    fn render_folder(
        &self,
        res: &FolderCompareResult,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let revisions = self.root.revisions().is_some() || self.root.pair().is_some();
        let is_three_way = self.root.is_three_way();
        let peer_mode = self.mode == ThreeWayCompareMode::NoBase && self.root.is_three_way();
        let mut filter_bar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1p5()
            .px_3p5()
            .py_2()
            .flex_none()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border);
        if is_three_way {
            filter_bar = filter_bar.child(self.mode_choice(design, cx)).child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(design.px(16.0))
                    .bg(design.border),
            );
        }
        for (id, filter, label, count) in self.folder_filter_specs(res) {
            filter_bar = filter_bar.child(self.filter_chip(id, filter, label, count, design, cx));
        }
        filter_bar = filter_bar.child(div().flex_1()).when(!revisions, |bar| {
            bar.child(
                ui::checkbox("compare-respect-gitignore", self.respect_gitignore)
                    .label(t!("compare.folder.respect_gitignore").to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_gitignore(cx))),
            )
        });

        let list: AnyElement = if self.filtered_indices.is_empty() {
            ui::empty_state(t!("compare.folder.empty").to_string(), design).into_any_element()
        } else {
            let entries = Arc::new(res.entries.clone());
            let indices = Arc::new(self.filtered_indices.clone());
            let marks = self.repo_marks.clone();
            let selected = self.selected_folder_entry;
            let design_copy = design.clone();
            let entity = cx.weak_entity();
            let count = indices.len();

            uniform_list(
                "compare-folder-entries",
                count,
                move |range: std::ops::Range<usize>, _window, _cx| {
                    range
                        .map(|list_idx| {
                            let entry_idx = indices[list_idx];
                            let entry = &entries[entry_idx];
                            let is_selected = selected == Some(entry_idx);
                            let mark = marks
                                .as_ref()
                                .and_then(|marks| marks.get(entry_idx))
                                .copied();
                            render_folder_row(
                                list_idx,
                                entry_idx,
                                entry,
                                is_three_way,
                                peer_mode,
                                revisions,
                                mark,
                                is_selected,
                                &design_copy,
                                entity.clone(),
                            )
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.folder_scroll)
            .with_scrollbar(&self.folder_scroll)
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(filter_bar)
            .child(list)
            .into_any_element()
    }

    /// The chips above a folder or tree list, which follow the mode.
    fn folder_filter_specs(
        &self,
        res: &FolderCompareResult,
    ) -> Vec<(&'static str, FolderFilter, String, usize)> {
        if self.root.is_three_way() && self.mode == ThreeWayCompareMode::NoBase {
            let counts = self.peer_counts;
            return vec![
                (
                    "filter-all",
                    FolderFilter::All,
                    t!("compare.folder.filter_all").to_string(),
                    counts.total,
                ),
                (
                    "filter-left-middle",
                    FolderFilter::LeftMiddleDiffer,
                    t!("compare.three_way.peer.left_middle").to_string(),
                    counts.left_middle,
                ),
                (
                    "filter-middle-right",
                    FolderFilter::MiddleRightDiffer,
                    t!("compare.three_way.peer.middle_right").to_string(),
                    counts.middle_right,
                ),
                (
                    "filter-all-different",
                    FolderFilter::AllDifferent,
                    t!("compare.three_way.peer.all_different").to_string(),
                    counts.all_different,
                ),
                (
                    "filter-missing",
                    FolderFilter::MissingSomeSide,
                    t!("compare.three_way.peer.missing").to_string(),
                    counts.missing,
                ),
            ];
        }

        let counts = &res.counts;
        let mut chips = vec![
            (
                "filter-all",
                FolderFilter::All,
                t!("compare.folder.filter_all").to_string(),
                counts.total,
            ),
            (
                "filter-diff",
                FolderFilter::Different,
                t!("compare.folder.filter_different").to_string(),
                counts.different,
            ),
            (
                "filter-left",
                FolderFilter::OnlyLeft,
                t!("compare.folder.filter_only_left").to_string(),
                counts.only_left,
            ),
        ];
        if self.root.is_three_way() {
            chips.push((
                "filter-middle",
                FolderFilter::OnlyMiddle,
                t!("compare.folder.filter_only_middle").to_string(),
                counts.only_middle,
            ));
        }
        chips.push((
            "filter-right",
            FolderFilter::OnlyRight,
            t!("compare.folder.filter_only_right").to_string(),
            counts.only_right,
        ));
        chips.push((
            "filter-identical",
            FolderFilter::Identical,
            t!("compare.folder.filter_identical").to_string(),
            counts.identical,
        ));
        if counts.type_mismatch > 0 {
            chips.push((
                "filter-mismatch",
                FolderFilter::TypeMismatch,
                t!("compare.folder.filter_mismatch").to_string(),
                counts.type_mismatch,
            ));
        }
        chips
    }

    /// The labels the current filter chips show, for tests that follow the mode.
    #[cfg(test)]
    pub(crate) fn folder_filter_labels(&self) -> Vec<String> {
        let CompareState::Folder(res) = &self.state else {
            return Vec::new();
        };
        self.folder_filter_specs(res)
            .into_iter()
            .map(|(_, _, label, count)| format!("{label} ({count})"))
            .collect()
    }

    /// The labels of the three-way file badges, for tests that follow the mode.
    #[cfg(test)]
    pub(crate) fn three_way_badge_labels(&self) -> Vec<String> {
        let CompareState::ThreeWayFile(res) = &self.state else {
            return Vec::new();
        };
        three_way_badges(res, self.mode)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn filter_chip(
        &self,
        id: &'static str,
        filter: FolderFilter,
        label: String,
        count: usize,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.folder_filter == filter;
        ui::button(id, format!("{label} ({count})"))
            .size(ButtonSize::Tiny)
            .ghost()
            .selected(active)
            .text_color(if active { design.text } else { design.muted })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.folder_filter = filter;
                this.rebuild_folder_filter();
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Focusable for CompareView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CompareView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let title = self.tab_title();
        let loading = self.is_loading();

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(design.background)
            .text_color(design.text)
            .track_focus(&self.focus_handle)
            .child(self.header(&design, window, cx));
        root.text_style().font_fallbacks = Some(style::font_fallbacks());

        if let Some(workspace) = self.workspace.upgrade()
            && let Some(strip) =
                Workspace::render_tab_strip(&workspace, Some(&title), loading, &design, cx)
        {
            root = root.child(strip);
        }

        let body: AnyElement = match &self.state {
            CompareState::Loading => {
                ui::loading_state(t!("compare.loading").to_string(), &design).into_any_element()
            }
            CompareState::Error(error) => div()
                .flex()
                .flex_col()
                .flex_1()
                .p_4()
                .child(ui::banner(ui::BannerKind::Danger, error.clone(), &design))
                .into_any_element(),
            CompareState::TwoWayFile(res) => {
                let names = self.root.pair().map(|pair| pair.pane_labels());
                self.render_two_way(res, names, &design, cx)
            }
            CompareState::ThreeWayFile(res) => self.render_three_way(res, &design, window, cx),
            CompareState::Folder(res) => self.render_folder(res, &design, cx),
        };

        root.child(body)
    }
}

enum CompareJob {
    TwoWayFiles {
        left: PathBuf,
        right: PathBuf,
    },
    TwoWayOptionalFiles {
        left: Option<PathBuf>,
        right: Option<PathBuf>,
    },
    ThreeWayFiles {
        left: PathBuf,
        middle: PathBuf,
        right: PathBuf,
    },
    ThreeWayOptionalFiles {
        left: Option<PathBuf>,
        middle: Option<PathBuf>,
        right: Option<PathBuf>,
    },
    Folders {
        roots: Vec<PathBuf>,
        options: FolderCompareOptions,
    },
    /// The tree-level list of a three-side repository comparison.
    RepoTree(RepoThreeWaySource),
    /// The tree-level list of a two-side repository comparison.
    RepoPairTree(RepoTwoWaySource),
    /// One file of a three-side repository comparison, read at the same path
    /// in each side.
    RepoFile {
        source: RepoThreeWaySource,
        path: String,
    },
    /// One file of a three-side comparison whose sides know it under different
    /// paths, because a history walk followed renames.
    RepoFileSides {
        source: RepoThreeWaySource,
        paths: [String; 3],
    },
    /// One file of a two-side repository comparison.
    RepoPairFile {
        source: RepoTwoWaySource,
        path: String,
    },
}

impl CompareJob {
    fn from_target(target: &CompareTarget, respect_gitignore: bool) -> Self {
        match target {
            CompareTarget::TwoWayFiles { left, right } => Self::TwoWayFiles {
                left: left.clone(),
                right: right.clone(),
            },
            CompareTarget::ThreeWayFiles {
                left,
                middle,
                right,
            } => Self::ThreeWayFiles {
                left: left.clone(),
                middle: middle.clone(),
                right: right.clone(),
            },
            CompareTarget::TwoWayFolders { left, right } => Self::Folders {
                roots: vec![left.clone(), right.clone()],
                options: FolderCompareOptions { respect_gitignore },
            },
            CompareTarget::ThreeWayFolders {
                left,
                middle,
                right,
            } => Self::Folders {
                roots: vec![left.clone(), middle.clone(), right.clone()],
                options: FolderCompareOptions { respect_gitignore },
            },
        }
    }

    fn run(self, cancel: &AtomicBool) -> BackgroundOutput {
        let diff_opts = DiffOptions::default();
        match self {
            Self::TwoWayFiles { left, right } => BackgroundOutput::TwoWayFile(
                compare_two_optional_files(Some(&left), Some(&right), &diff_opts, cancel)
                    .map_err(|e| e.to_string()),
            ),
            Self::TwoWayOptionalFiles { left, right } => BackgroundOutput::TwoWayFile(
                compare_two_optional_files(left.as_deref(), right.as_deref(), &diff_opts, cancel)
                    .map_err(|e| e.to_string()),
            ),
            Self::ThreeWayFiles {
                left,
                middle,
                right,
            } => BackgroundOutput::ThreeWayFile(
                compare_three_optional_files(Some(&left), Some(&middle), Some(&right), cancel)
                    .map_err(|e| e.to_string()),
            ),
            Self::ThreeWayOptionalFiles {
                left,
                middle,
                right,
            } => BackgroundOutput::ThreeWayFile(
                compare_three_optional_files(
                    left.as_deref(),
                    middle.as_deref(),
                    right.as_deref(),
                    cancel,
                )
                .map_err(|e| e.to_string()),
            ),
            Self::Folders { roots, options } => {
                let refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
                BackgroundOutput::Folder(
                    compare_folders(&refs, options, cancel).map_err(|e| e.to_string()),
                )
            }
            Self::RepoTree(source) => BackgroundOutput::RepoTree(
                source
                    .reader
                    .three_way_side_tree(source.sides())
                    .map(Some)
                    .map_err(|error| error.to_string()),
            ),
            Self::RepoPairTree(source) => {
                let sides = [source.left.side, source.right.side];
                let left_files = source.reader.compare_side_files(sides[0]);
                let right_files = source.reader.compare_side_files(sides[1]);
                let output = match (left_files, right_files) {
                    (Ok(left), Ok(right)) => Ok(Some(compare_two_repo_sides(
                        sides[0], &left, sides[1], &right,
                    ))),
                    (Err(error), _) | (_, Err(error)) => Err(error.to_string()),
                };
                BackgroundOutput::RepoPairTree(output)
            }
            Self::RepoFile { source, path } => Self::RepoFileSides {
                source,
                paths: [path.clone(), path.clone(), path],
            }
            .run(cancel),
            Self::RepoFileSides { source, paths } => {
                let sides = source.sides();
                let reads = [
                    source.reader.compare_side_blob(sides[0], &paths[0]),
                    source.reader.compare_side_blob(sides[1], &paths[1]),
                    source.reader.compare_side_blob(sides[2], &paths[2]),
                ];
                let output = match reads {
                    [Ok(left), Ok(middle), Ok(right)] => {
                        let [left_name, middle_name, right_name] = source.pane_labels();
                        Ok(compare_three_sides(
                            ThreeWaySide::new(left_name, Some(paths[0].clone()), left),
                            ThreeWaySide::new(middle_name, Some(paths[1].clone()), middle),
                            ThreeWaySide::new(right_name, Some(paths[2].clone()), right),
                            cancel,
                        ))
                    }
                    [Err(error), ..] | [_, Err(error), _] | [_, _, Err(error)] => {
                        Err(error.to_string())
                    }
                };
                BackgroundOutput::ThreeWayFile(output)
            }
            Self::RepoPairFile { source, path } => {
                let sides = [source.left.side, source.right.side];
                let reads = [
                    source.reader.compare_side_blob(sides[0], &path),
                    source.reader.compare_side_blob(sides[1], &path),
                ];
                let output = match reads {
                    [Ok(left), Ok(right)] => Ok(compare_two_bytes(
                        Path::new(&path),
                        left.as_deref(),
                        Path::new(&path),
                        right.as_deref(),
                        &diff_opts,
                        cancel,
                    )),
                    [Err(error), _] | [_, Err(error)] => Err(error.to_string()),
                };
                BackgroundOutput::TwoWayFile(output)
            }
        }
    }
}

/// Presents a revision tree with the folder list's types, so the same filter
/// chips, rows and drill-in work for both.
fn folder_result_from_tree(
    tree: &ThreeWayTreeCompare,
    source: &RepoThreeWaySource,
) -> FolderCompareResult {
    FolderCompareResult {
        roots: source
            .pane_labels()
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        entries: tree
            .entries
            .iter()
            .map(|entry| FolderCompareEntry {
                relative_path: entry.relative_path.clone(),
                depth: 0,
                kind: entry.kind,
                status: entry.status,
                peer: entry.peer,
                sides: entry.sides.clone(),
            })
            .collect(),
        counts: tree.counts.clone(),
    }
}

/// Presents a two-side revision tree with the folder list's types.
fn folder_result_from_pair(tree: &RepoCompareTree) -> FolderCompareResult {
    FolderCompareResult {
        roots: vec![
            PathBuf::from(tree.left.identity()),
            PathBuf::from(tree.right.identity()),
        ],
        entries: tree.entries.clone(),
        counts: tree.counts.clone(),
    }
}

fn peer_counts_from_entries(entries: &[FolderCompareEntry]) -> PeerTreeCounts {
    let mut counts = PeerTreeCounts::default();
    for entry in entries {
        counts.record(entry.peer);
    }
    counts
}

fn display_or_missing(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        t!("compare.side.missing").to_string()
    } else {
        path_label(path)
    }
}

fn step_index(current: Option<usize>, total: usize, forward: bool) -> usize {
    match current {
        None => {
            if forward {
                0
            } else {
                total.saturating_sub(1)
            }
        }
        Some(idx) => {
            if forward {
                (idx + 1) % total
            } else {
                idx.checked_sub(1).unwrap_or(total - 1)
            }
        }
    }
}

fn display_name_or_missing(name: &str) -> String {
    if name.is_empty() {
        t!("compare.side.missing").to_string()
    } else {
        name.to_owned()
    }
}

fn three_way_pane_header(
    role: String,
    name: &str,
    design: &Design,
    border_right: bool,
) -> AnyElement {
    let tooltip_text: SharedString = name.to_owned().into();
    div()
        .id(SharedString::from(format!("compare-3way-hdr-{role}")))
        .flex()
        .items_center()
        .gap_1p5()
        .flex_1()
        .min_w_0()
        .h_full()
        .px_2p5()
        .text_size(design.px(11.0))
        .when(border_right, |cell| {
            cell.border_r_1().border_color(design.border)
        })
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(design.text)
                .child(role),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(design.muted)
                .child(display_name_or_missing(name)),
        )
        .when(!name.is_empty(), |cell| {
            cell.tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        })
        .into_any_element()
}

fn render_three_way_images(
    state: &image_diff::State,
    images: &ThreeWayImageCompare,
    design: &Design,
) -> AnyElement {
    let side_view = |side: Option<&strata_core::image::ImageSide>, label: String| -> AnyElement {
        if let Some(side) = side {
            image_diff::labelled_picture(state, side, label, design.text, design, false)
        } else {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .child(ui::empty_state(
                    t!("compare.side.missing").to_string(),
                    design,
                ))
                .into_any_element()
        }
    };

    div()
        .flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(side_view(
            images.left.as_ref(),
            t!("compare.side.left").to_string(),
        ))
        .child(div().flex_none().w(px(1.0)).h_full().bg(design.border))
        .child(side_view(
            images.middle.as_ref(),
            t!("compare.side.middle_base").to_string(),
        ))
        .child(div().flex_none().w(px(1.0)).h_full().bg(design.border))
        .child(side_view(
            images.right.as_ref(),
            t!("compare.side.right").to_string(),
        ))
        .into_any_element()
}

/// Wide enough for the widest line of any of the three sides, in columns.
fn three_way_max_columns(
    left: &[String],
    middle: &[String],
    right: &[String],
    tab_width: usize,
) -> usize {
    left.iter()
        .chain(middle.iter())
        .chain(right.iter())
        .map(|line| display_width_with_tabs(line, tab_width))
        .max()
        .unwrap_or(0)
}

/// The badges a three-way file view shows, which follow the mode: base-relative
/// counts in the default mode, one count per neighbour boundary otherwise.
fn three_way_badges(
    res: &ThreeWayFileCompare,
    mode: ThreeWayCompareMode,
) -> Vec<(String, BadgeTone)> {
    match mode {
        ThreeWayCompareMode::MiddleBase => {
            let counts = res.counts();
            vec![
                (
                    t!("compare.three_way.left_count", count = counts.left_changed).to_string(),
                    BadgeTone::Accent,
                ),
                (
                    t!(
                        "compare.three_way.right_count",
                        count = counts.right_changed
                    )
                    .to_string(),
                    BadgeTone::Accent,
                ),
                (
                    t!("compare.three_way.agree_count", count = counts.both_agree).to_string(),
                    BadgeTone::Neutral,
                ),
                (
                    t!(
                        "compare.three_way.conflict_count",
                        count = counts.both_disagree
                    )
                    .to_string(),
                    if counts.both_disagree > 0 {
                        BadgeTone::Danger
                    } else {
                        BadgeTone::Neutral
                    },
                ),
            ]
        }
        ThreeWayCompareMode::NoBase => {
            let counts = res.peer_counts();
            vec![
                (
                    t!(
                        "compare.three_way.left_middle_count",
                        count = counts.left_middle
                    )
                    .to_string(),
                    BadgeTone::Accent,
                ),
                (
                    t!(
                        "compare.three_way.middle_right_count",
                        count = counts.middle_right
                    )
                    .to_string(),
                    BadgeTone::Accent,
                ),
                (
                    t!(
                        "compare.three_way.all_different_count",
                        count = counts.all_different
                    )
                    .to_string(),
                    if counts.all_different > 0 {
                        BadgeTone::Danger
                    } else {
                        BadgeTone::Neutral
                    },
                ),
            ]
        }
    }
}

/// How one pane's cell in a row is painted.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CellPaint {
    /// The side has no line in this row, so the cell is a neutral filler. A
    /// side without a line has nothing that could differ, and painting it
    /// with a neighbour's tint made the empty cell read as content.
    Filler,
    /// A line that matches its neighbour.
    Plain,
    /// A line that differs from a neighbour, in that diff's tint.
    Tinted(Hsla),
}

/// The neutral wash behind a cell whose side has no line.
///
/// It has to read as "nothing here" beside every diff tint, so it is the
/// border grey rather than the field or hunk-header grey, which sit close to
/// the softer tints.
fn filler_background(design: &Design) -> Hsla {
    design.border
}

/// The row's three cell paints and the marker colour on each pane boundary.
struct ThreeWayTints {
    left: CellPaint,
    middle: CellPaint,
    right: CellPaint,
    /// The boundary between the left and middle panes.
    left_boundary: Option<Hsla>,
    /// The boundary between the middle and right panes.
    middle_boundary: Option<Hsla>,
}

#[allow(clippy::too_many_arguments)]
fn render_three_way_row(
    row: ThreeWayRow,
    mode: ThreeWayCompareMode,
    left_lines: &[String],
    middle_lines: &[String],
    right_lines: &[String],
    left_hl: &[strata_core::highlight::LineHighlights],
    middle_hl: &[strata_core::highlight::LineHighlights],
    right_hl: &[strata_core::highlight::LineHighlights],
    diff: &DiffColors,
    style: &DiffStyle,
    design: &Design,
    content_width: gpui::Pixels,
    h_scroll: &ScrollHandle,
) -> AnyElement {
    let tints = three_way_tints(row, mode, design);
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(style.row_height()))
        .font_family(style.mono().clone())
        .text_size(px(style.font_size()))
        .text_color(diff.text)
        .child(three_way_cell(
            row.left,
            left_lines,
            left_hl,
            tints.left,
            tints.left_boundary,
            diff,
            style,
            design,
            0,
            content_width,
            h_scroll,
        ))
        .child(three_way_cell(
            row.middle,
            middle_lines,
            middle_hl,
            tints.middle,
            tints.middle_boundary,
            diff,
            style,
            design,
            1,
            content_width,
            h_scroll,
        ))
        .child(three_way_cell(
            row.right,
            right_lines,
            right_hl,
            tints.right,
            None,
            diff,
            style,
            design,
            2,
            content_width,
            h_scroll,
        ))
        .into_any_element()
}

fn three_way_tints(row: ThreeWayRow, mode: ThreeWayCompareMode, design: &Design) -> ThreeWayTints {
    let (left, middle, right, left_boundary, middle_boundary) =
        if mode == ThreeWayCompareMode::NoBase {
            match row.kind.peer_status() {
                ThreeWayPeerStatus::Same | ThreeWayPeerStatus::MissingSomeSide => {
                    (None, None, None, None, None)
                }
                ThreeWayPeerStatus::LeftMiddleDiffer => (
                    Some(design.accent_soft),
                    Some(design.accent_soft),
                    None,
                    Some(design.accent),
                    None,
                ),
                ThreeWayPeerStatus::MiddleRightDiffer => (
                    None,
                    Some(design.accent_soft),
                    Some(design.accent_soft),
                    None,
                    Some(design.accent),
                ),
                ThreeWayPeerStatus::AllDifferent => (
                    Some(design.diff_removed_bg),
                    Some(design.diff_hunk_header),
                    Some(design.diff_removed_bg),
                    Some(design.accent),
                    Some(design.accent),
                ),
            }
        } else {
            let (left, middle, right) = match row.kind {
                ThreeWayDiffKind::Unchanged => (None, None, None),
                ThreeWayDiffKind::LeftChanged => (Some(design.diff_added_bg), None, None),
                ThreeWayDiffKind::RightChanged => (None, None, Some(design.diff_added_bg)),
                ThreeWayDiffKind::BothAgree => {
                    (Some(design.accent_soft), None, Some(design.accent_soft))
                }
                ThreeWayDiffKind::BothDisagree => (
                    Some(design.diff_removed_bg),
                    Some(design.diff_hunk_header),
                    Some(design.diff_removed_bg),
                ),
            };
            (left, middle, right, None, None)
        };

    ThreeWayTints {
        left: cell_paint(row.left, left),
        middle: cell_paint(row.middle, middle),
        right: cell_paint(row.right, right),
        left_boundary,
        middle_boundary,
    }
}

/// A side without a line in this row is filler, whatever the row's diff says:
/// the tint belongs to content, and the pane beside it carries the change.
fn cell_paint(line: Option<u32>, tint: Option<Hsla>) -> CellPaint {
    match (line, tint) {
        (None, _) => CellPaint::Filler,
        (Some(_), Some(tint)) => CellPaint::Tinted(tint),
        (Some(_), None) => CellPaint::Plain,
    }
}

#[allow(clippy::too_many_arguments)]
fn three_way_cell(
    line: Option<u32>,
    lines: &[String],
    highlights: &[strata_core::highlight::LineHighlights],
    paint: CellPaint,
    boundary: Option<Hsla>,
    diff: &DiffColors,
    style: &DiffStyle,
    design: &Design,
    side: usize,
    content_width: gpui::Pixels,
    h_scroll: &ScrollHandle,
) -> AnyElement {
    let mut cell = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .when_some(
            match paint {
                CellPaint::Filler => Some(filler_background(design)),
                CellPaint::Plain => None,
                CellPaint::Tinted(tint) => Some(tint),
            },
            |cell, tint| cell.bg(tint),
        )
        .when(side < 2, |cell| match boundary {
            // A marked boundary is the row-level cue that this pane differs
            // from its neighbour.
            Some(marker) => cell.border_r(px(2.0)).border_color(marker),
            None => cell.border_r_1().border_color(design.border),
        })
        // The gutter is the same width in every pane, filler or not, so the
        // three columns keep their alignment.
        .child(
            div()
                .flex_none()
                .w(style.px(38.0))
                .pr_1()
                .text_size(px(style.font_size() - style.scale(2.0)))
                .text_color(diff.gutter)
                .text_right()
                .when_some(line, |gutter, number| gutter.child(number.to_string())),
        );

    if let Some(number) = line {
        let text = lines
            .get(number as usize - 1)
            .map(String::as_str)
            .unwrap_or_default();
        let spans = highlights.get(number as usize - 1);
        cell = cell.child(
            // The line numbers stay put; only the code pans, so the three
            // panes can share this handle and keep their columns aligned.
            div()
                .id(("compare-3way-pane", number as usize * 3 + side))
                .debug_selector({
                    let name = match side {
                        0 => "compare-3way-left",
                        1 => "compare-3way-middle",
                        _ => "compare-3way-right",
                    };
                    move || name.to_string()
                })
                .flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .items_center()
                .overflow_x_scroll()
                .restrict_scroll_to_axis()
                .track_scroll(h_scroll)
                .child(
                    div()
                        .flex_none()
                        .w(content_width)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(diff_view::styled_line(text, style, spans, diff)),
                ),
        );
    }
    cell.into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_folder_row(
    list_idx: usize,
    entry_idx: usize,
    entry: &FolderCompareEntry,
    is_three_way: bool,
    peer_mode: bool,
    revisions: bool,
    mark: Option<RepoTreeMark>,
    selected: bool,
    design: &Design,
    entity: WeakEntity<CompareView>,
) -> AnyElement {
    let icon_name = match entry.kind {
        FolderEntryKind::Directory => IconName::Folder,
        FolderEntryKind::Symlink => IconName::Unlink,
        FolderEntryKind::File | FolderEntryKind::Other => IconName::FileText,
    };
    let name = if revisions {
        entry.relative_path.clone()
    } else {
        Path::new(&entry.relative_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.relative_path.clone())
    };
    let peer = if peer_mode {
        Some(mark.and_then(|mark| mark.peer).unwrap_or(entry.peer))
    } else {
        None
    };
    let (status_label, status_tone) = match peer {
        Some(peer) => peer_badge(peer),
        None => match entry.status {
            FolderEntryStatus::Identical => (
                t!("compare.status.identical").to_string(),
                BadgeTone::Neutral,
            ),
            FolderEntryStatus::Different => (
                t!("compare.status.different").to_string(),
                BadgeTone::Warning,
            ),
            FolderEntryStatus::OnlyLeft => (
                t!("compare.status.only_left").to_string(),
                BadgeTone::Accent,
            ),
            FolderEntryStatus::OnlyMiddle => (
                t!("compare.status.only_middle").to_string(),
                BadgeTone::Accent,
            ),
            FolderEntryStatus::OnlyRight => (
                t!("compare.status.only_right").to_string(),
                BadgeTone::Accent,
            ),
            FolderEntryStatus::TypeMismatch => (
                t!("compare.status.type_mismatch").to_string(),
                BadgeTone::Danger,
            ),
        },
    };
    let clickable = entry.kind == FolderEntryKind::File;
    let indent = entry.depth as f32 * 16.0;

    let sizes_text = format_entry_sizes(entry, is_three_way);

    let mut row = ui::list_row(
        ("compare-folder-row", list_idx),
        RowState::NORMAL.selected(selected),
        design,
    )
    .pl(design.px(12.0 + indent))
    .pr(design.px(12.0))
    .gap_2()
    .child(Icon::new(icon_name).size(design.px(14.0)).text_color(
        if entry.kind == FolderEntryKind::Directory {
            design.accent_text
        } else {
            design.muted
        },
    ))
    .child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(design.px(12.0))
            .text_color(design.text)
            .font_weight(if entry.kind == FolderEntryKind::Directory {
                gpui::FontWeight::MEDIUM
            } else {
                gpui::FontWeight::NORMAL
            })
            .child(name),
    )
    .child(
        div()
            .flex_none()
            .text_size(design.px(11.0))
            .text_color(design.muted)
            .child(sizes_text),
    )
    .child(ui::status_badge(status_label, status_tone, design));

    // In peer mode the status badge already names the difference between
    // neighbours, so the base-relative change badge would repeat it.
    if let Some(mark) = mark
        && !peer_mode
        && let Some(change) = mark.change
    {
        let (change_label, change_tone) = match change {
            ThreeWayTreeChange::LeftChanged => (
                t!("compare.three_way.change.left").to_string(),
                BadgeTone::Accent,
            ),
            ThreeWayTreeChange::RightChanged => (
                t!("compare.three_way.change.right").to_string(),
                BadgeTone::Accent,
            ),
            ThreeWayTreeChange::BothAgree => (
                t!("compare.three_way.change.agree").to_string(),
                BadgeTone::Neutral,
            ),
            ThreeWayTreeChange::BothDisagree => (
                t!("compare.three_way.change.disagree").to_string(),
                BadgeTone::Warning,
            ),
        };
        row = row.child(ui::status_badge(change_label, change_tone, design));
        if mark.conflicting {
            row = row.child(ui::status_badge(
                t!("compare.three_way.conflict_badge").to_string(),
                BadgeTone::Danger,
                design,
            ));
        }
    }

    if clickable {
        row = row.cursor_pointer().on_click(move |_, window, cx| {
            let _ = entity.update(cx, |view, cx| view.open_folder_entry(entry_idx, window, cx));
        });
    }

    row.into_any_element()
}

fn peer_badge(peer: ThreeWayPeerStatus) -> (String, BadgeTone) {
    match peer {
        ThreeWayPeerStatus::Same => (
            t!("compare.three_way.peer.same").to_string(),
            BadgeTone::Neutral,
        ),
        ThreeWayPeerStatus::LeftMiddleDiffer => (
            t!("compare.three_way.peer.left_middle").to_string(),
            BadgeTone::Accent,
        ),
        ThreeWayPeerStatus::MiddleRightDiffer => (
            t!("compare.three_way.peer.middle_right").to_string(),
            BadgeTone::Accent,
        ),
        ThreeWayPeerStatus::AllDifferent => (
            t!("compare.three_way.peer.all_different").to_string(),
            BadgeTone::Warning,
        ),
        ThreeWayPeerStatus::MissingSomeSide => (
            t!("compare.three_way.peer.missing").to_string(),
            BadgeTone::Accent,
        ),
    }
}

fn format_entry_sizes(entry: &FolderCompareEntry, is_three_way: bool) -> String {
    if entry.kind != FolderEntryKind::File {
        return String::new();
    }
    let fmt = |idx: usize| -> String {
        match entry.sides.get(idx).and_then(|s| s.as_ref()) {
            Some(side) => side.size.map(format_size).unwrap_or_else(|| "—".to_owned()),
            None => "—".to_owned(),
        }
    };
    if is_three_way {
        format!("{} | {} | {}", fmt(0), fmt(1), fmt(2))
    } else {
        format!("{} | {}", fmt(0), fmt(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn dialog_validation_accepts_two_or_three_files_or_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let file_a = tmp.path().join("a.txt");
        let file_b = tmp.path().join("b.txt");
        let file_c = tmp.path().join("c.txt");
        let dir_a = tmp.path().join("dir_a");
        let dir_b = tmp.path().join("dir_b");
        let dir_c = tmp.path().join("dir_c");
        fs::write(&file_a, "a").unwrap();
        fs::write(&file_b, "b").unwrap();
        fs::write(&file_c, "c").unwrap();
        fs::create_dir(&dir_a).unwrap();
        fs::create_dir(&dir_b).unwrap();
        fs::create_dir(&dir_c).unwrap();

        assert_eq!(
            validate_dialog_inputs(&file_a.to_string_lossy(), &file_b.to_string_lossy(), ""),
            Ok(CompareTarget::TwoWayFiles {
                left: file_a.clone(),
                right: file_b.clone(),
            })
        );
        assert_eq!(
            validate_dialog_inputs(
                &file_a.to_string_lossy(),
                &file_c.to_string_lossy(),
                &file_b.to_string_lossy(),
            ),
            Ok(CompareTarget::ThreeWayFiles {
                left: file_a.clone(),
                middle: file_b.clone(),
                right: file_c.clone(),
            })
        );
        assert_eq!(
            validate_dialog_inputs(&dir_a.to_string_lossy(), &dir_b.to_string_lossy(), " "),
            Ok(CompareTarget::TwoWayFolders {
                left: dir_a.clone(),
                right: dir_b.clone(),
            })
        );
        assert_eq!(
            validate_dialog_inputs(
                &dir_a.to_string_lossy(),
                &dir_c.to_string_lossy(),
                &dir_b.to_string_lossy(),
            ),
            Ok(CompareTarget::ThreeWayFolders {
                left: dir_a.clone(),
                middle: dir_b.clone(),
                right: dir_c.clone(),
            })
        );
    }

    #[test]
    fn dialog_validation_rejects_missing_nonexistent_or_mixed_paths() {
        let tmp = TempDir::new().expect("tempdir");
        let file_a = tmp.path().join("a.txt");
        let dir_b = tmp.path().join("dir_b");
        let missing = tmp.path().join("missing.txt");
        fs::write(&file_a, "a").unwrap();
        fs::create_dir(&dir_b).unwrap();

        assert_eq!(
            validate_dialog_inputs("", &file_a.to_string_lossy(), ""),
            Err(CompareValidationError::MissingRequired)
        );
        assert_eq!(
            validate_dialog_inputs(&file_a.to_string_lossy(), "", ""),
            Err(CompareValidationError::MissingRequired)
        );
        assert_eq!(
            validate_dialog_inputs(&file_a.to_string_lossy(), &missing.to_string_lossy(), ""),
            Err(CompareValidationError::PathNotFound(missing.clone()))
        );
        assert_eq!(
            validate_dialog_inputs(&file_a.to_string_lossy(), &dir_b.to_string_lossy(), ""),
            Err(CompareValidationError::MixedTypes)
        );
        assert_eq!(
            validate_dialog_inputs(
                &file_a.to_string_lossy(),
                &file_a.to_string_lossy(),
                &dir_b.to_string_lossy(),
            ),
            Err(CompareValidationError::MixedTypes)
        );
    }

    fn repo_revision_options(repo: &strata_core::test_support::TestRepo) -> (RevisionOptions, Oid) {
        let opened = repo.open();
        let head = opened.head().expect("head");
        let head_oid = match head {
            strata_core::HeadInfo::Branch {
                target: Some(target),
                ..
            } => target,
            strata_core::HeadInfo::Detached { target } => target,
            _ => panic!("the fixture is on a commit"),
        };
        let reader: Arc<dyn RepoReader> = Arc::new(opened);
        let options = RevisionOptions {
            reader: Some(reader),
            options: vec![
                RevisionOption {
                    spec: "main".to_owned(),
                    display: "main".to_owned(),
                    label: "main".to_owned(),
                    side: RepoCompareSide::Commit(head_oid),
                },
                RevisionOption {
                    spec: t!("compare.working_tree").to_string(),
                    display: t!("compare.working_tree").to_string(),
                    label: t!("compare.working_tree").to_string(),
                    side: RepoCompareSide::Worktree,
                },
                RevisionOption {
                    spec: t!("compare.staged").to_string(),
                    display: t!("compare.staged").to_string(),
                    label: t!("compare.staged").to_string(),
                    side: RepoCompareSide::Index,
                },
            ],
        };
        (options, head_oid)
    }

    #[test]
    fn revision_validation_resolves_picker_choices_and_typed_specs() {
        let repo = strata_core::test_support::TestRepo::new();
        repo.write("a.txt", "one\n");
        let first = repo.commit("first");
        repo.write("a.txt", "two\n");
        repo.commit("second");
        let (options, head) = repo_revision_options(&repo);

        let sides = validate_revision_inputs("main", "Working tree", "", &options)
            .expect("two sides resolve");
        assert_eq!(sides.len(), 2);
        assert_eq!(sides[0].side, RepoCompareSide::Commit(head));
        assert_eq!(sides[0].label, "main");
        assert_eq!(sides[1].side, RepoCompareSide::Worktree);

        let typed = first.to_string();
        let sides = validate_revision_inputs(&typed, "main", "Staged", &options)
            .expect("a typed object id and three sides resolve");
        assert_eq!(sides.len(), 3);
        assert_eq!(sides[0].side, RepoCompareSide::Commit(first));
        assert_eq!(sides[1].side, RepoCompareSide::Index);
        assert_eq!(sides[2].side, RepoCompareSide::Commit(head));

        assert_eq!(
            validate_revision_inputs("no-such-revision", "main", "", &options),
            Err(CompareValidationError::UnknownRevision(
                "no-such-revision".to_owned()
            ))
        );
        assert_eq!(
            validate_revision_inputs("", "main", "", &options),
            Err(CompareValidationError::MissingRequired)
        );
        assert_eq!(
            validate_revision_inputs("main", "main", "", &RevisionOptions::default()),
            Err(CompareValidationError::NoRepository)
        );
    }

    #[test]
    fn a_picked_commit_shows_a_short_label_but_resolves_to_the_full_id() {
        let repo = strata_core::test_support::TestRepo::new();
        repo.write("a.txt", "one\n");
        let first = repo.commit("first");
        let short = crate::data::short_oid(first);
        let display = format!("{short} first");
        let working_tree = t!("compare.working_tree").to_string();
        let options = RevisionOptions {
            reader: Some(Arc::new(repo.open())),
            options: vec![
                RevisionOption {
                    spec: first.to_string(),
                    display: display.clone(),
                    label: display.clone(),
                    side: RepoCompareSide::Commit(first),
                },
                RevisionOption {
                    spec: working_tree.clone(),
                    display: working_tree.clone(),
                    label: working_tree.clone(),
                    side: RepoCompareSide::Worktree,
                },
            ],
        };

        let sides = validate_revision_inputs(&display, &working_tree, "", &options)
            .expect("the short label resolves through the picker choice");
        assert_eq!(sides[0].side, RepoCompareSide::Commit(first));
        assert!(
            sides[0].label.is_empty(),
            "a commit label is looked up from the full id, not the field text"
        );
        assert_eq!(sides[1].side, RepoCompareSide::Worktree);
    }

    #[gpui::test]
    fn three_way_mode_toggle_switches_badges(cx: &mut gpui::TestAppContext) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let left = tmp.path().join("left.txt");
        let middle = tmp.path().join("middle.txt");
        let right = tmp.path().join("right.txt");
        fs::write(&left, "same\nleft\nend\n").unwrap();
        fs::write(&middle, "same\nbase\nend\n").unwrap();
        fs::write(&right, "same\nbase\nmid-right\n").unwrap();
        let target = CompareTarget::ThreeWayFiles {
            left,
            middle,
            right,
        };

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            crate::actions::bind(cx);
        });
        let workspace_cell: std::rc::Rc<std::cell::RefCell<Option<Entity<Workspace>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let workspace_cell_init = workspace_cell.clone();
        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), move |window, cx| {
                    let workspace = cx.new(|cx| Workspace::new_with_compare(target, window, cx));
                    *workspace_cell_init.borrow_mut() = Some(workspace.clone());
                    cx.new(|cx| gpui_component::Root::new(workspace, window, cx))
                })
            })
            .expect("open window");
        let mut visual_cx = gpui::VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();

        let workspace = workspace_cell.borrow().clone().expect("workspace");
        let view = visual_cx.update(|_, cx| {
            workspace
                .read(cx)
                .active_compare_view()
                .expect("the compare tab")
        });
        visual_cx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.mode(), ThreeWayCompareMode::MiddleBase);
            let labels = view.three_way_badge_labels();
            assert!(
                labels.contains(&t!("compare.three_way.left_count", count = 1).to_string()),
                "the default mode counts changes against the base: {labels:?}"
            );
            assert!(
                labels.contains(&t!("compare.three_way.right_count", count = 1).to_string()),
                "the right side differs from the base on its own line: {labels:?}"
            );
        });

        view.update(&mut visual_cx, |view, cx| {
            view.set_mode(ThreeWayCompareMode::NoBase, cx);
        });
        visual_cx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.mode(), ThreeWayCompareMode::NoBase);
            let labels = view.three_way_badge_labels();
            assert_eq!(
                labels[0],
                t!("compare.three_way.left_middle_count", count = 1).to_string()
            );
            assert_eq!(
                labels[1],
                t!("compare.three_way.middle_right_count", count = 1).to_string()
            );
            assert_eq!(
                labels[2],
                t!("compare.three_way.all_different_count", count = 0).to_string()
            );
        });
    }

    #[gpui::test]
    fn three_way_folder_filter_follows_the_mode(cx: &mut gpui::TestAppContext) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let left = tmp.path().join("left");
        let middle = tmp.path().join("middle");
        let right = tmp.path().join("right");
        for dir in [&left, &middle, &right] {
            fs::create_dir(dir).unwrap();
        }
        fs::write(left.join("changed.txt"), "left\n").unwrap();
        fs::write(middle.join("changed.txt"), "base\n").unwrap();
        fs::write(right.join("changed.txt"), "base\n").unwrap();
        fs::write(left.join("outer.txt"), "same\n").unwrap();
        fs::write(middle.join("outer.txt"), "middle\n").unwrap();
        fs::write(right.join("outer.txt"), "same\n").unwrap();
        let target = CompareTarget::ThreeWayFolders {
            left,
            middle,
            right,
        };

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            crate::actions::bind(cx);
        });
        let workspace_cell: std::rc::Rc<std::cell::RefCell<Option<Entity<Workspace>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let workspace_cell_init = workspace_cell.clone();
        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), move |window, cx| {
                    let workspace = cx.new(|cx| Workspace::new_with_compare(target, window, cx));
                    *workspace_cell_init.borrow_mut() = Some(workspace.clone());
                    cx.new(|cx| gpui_component::Root::new(workspace, window, cx))
                })
            })
            .expect("open window");
        let mut visual_cx = gpui::VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();
        let workspace = workspace_cell.borrow().clone().expect("workspace");
        let view = visual_cx.update(|_, cx| {
            workspace
                .read(cx)
                .active_compare_view()
                .expect("the compare tab")
        });

        visual_cx.update(|_, cx| {
            let labels = view.read(cx).folder_filter_labels();
            assert!(
                labels.iter().any(|label| label.starts_with("Only Left (")),
                "the default mode lists presence relative to the base: {labels:?}"
            );
        });
        view.update(&mut visual_cx, |view, cx| {
            view.set_mode(ThreeWayCompareMode::NoBase, cx);
        });
        visual_cx.update(|_, cx| {
            let labels = view.read(cx).folder_filter_labels();
            assert!(
                labels.contains(&format!("{} (2)", t!("compare.three_way.peer.left_middle"))),
                "peer mode counts the left/middle boundary: {labels:?}"
            );
            assert!(
                labels.contains(&format!(
                    "{} (1)",
                    t!("compare.three_way.peer.all_different")
                )),
                "the middle differs from both agreeing outer sides: {labels:?}"
            );
        });
    }

    #[test]
    fn dialog_cancel_is_a_secondary_button() {
        assert_eq!(dialog_cancel_button().button_kind(), ButtonKind::Normal);
    }

    #[test]
    fn shared_three_way_width_covers_the_widest_side() {
        let left = vec!["short".to_owned()];
        let middle = vec!["\tindented".to_owned()];
        let right = vec!["日本語".to_owned(), "x".to_owned()];

        // Four columns before the tab stop, plus four columns of indent.
        assert_eq!(three_way_max_columns(&left, &middle, &right, 4), 12);
        assert_eq!(three_way_max_columns(&left, &middle, &right, 8), 16);
        assert_eq!(
            three_way_max_columns(&left, &middle, &right, 1),
            display_width_with_tabs("\tindented", 1)
        );
    }

    #[test]
    fn ordered_path_validation_checks_arg_count() {
        let tmp = TempDir::new().expect("tempdir");
        let file = tmp.path().join("a.txt");
        fs::write(&file, "a").unwrap();

        assert_eq!(
            validate_ordered_paths(std::slice::from_ref(&file)),
            Err(CompareValidationError::WrongArgCount(1))
        );
        assert_eq!(
            validate_ordered_paths(&[file.clone(), file.clone(), file.clone(), file]),
            Err(CompareValidationError::WrongArgCount(4))
        );
    }

    #[test]
    fn the_compare_title_bar_fits_every_window_at_every_zoom() {
        for width in (style::MIN_WINDOW_SIZE.0 as u32..=1920).map(|width| width as f32) {
            for zoom in [1.0, 1.5] {
                for has_back in [false, true] {
                    let fit = CompareHeaderFit::new(width, has_back, zoom);
                    assert!(
                        fit.used() <= width + 0.01,
                        "zoom {zoom}, has_back {has_back}: the title bar needs {} px of a {width} px window, which pushes the window controls off screen",
                        fit.used()
                    );
                }
            }
        }
    }

    #[test]
    fn the_compare_title_bar_sheds_its_details_in_order() {
        let wide = CompareHeaderFit::new(1600.0, true, 1.0);
        assert!(wide.wordmark);
        assert!(wide.back_label);
        assert!(wide.new_compare_label);

        let no_wordmark = CompareHeaderFit::new(1000.0, true, 1.0);
        assert!(!no_wordmark.wordmark, "the wordmark goes first");
        assert!(no_wordmark.back_label);
        assert!(no_wordmark.new_compare_label);

        let icons = CompareHeaderFit::new(850.0, true, 1.0);
        assert!(!icons.wordmark);
        assert!(icons.back_label, "the arrow's label outlives the action's");
        assert!(!icons.new_compare_label, "New Compare drops its label next");

        let minimum = CompareHeaderFit::new(style::MIN_WINDOW_SIZE.0, true, 1.0);
        assert!(!minimum.wordmark);
        assert!(!minimum.back_label);
        assert!(!minimum.new_compare_label);
        assert!(
            minimum.used() <= style::MIN_WINDOW_SIZE.0,
            "the minimum window keeps the title's own width: {}",
            minimum.used()
        );

        // A window with nothing to go back to never reserves the arrow.
        let no_back = CompareHeaderFit::new(1600.0, false, 1.0);
        assert!(!no_back.back_label);
        assert!(no_back.used() < wide.used());
    }

    #[test]
    fn the_compare_title_bar_scales_with_the_zoom() {
        let fit_1x = CompareHeaderFit::new(1360.0, true, 1.0);
        let fit_15x = CompareHeaderFit::new(1360.0 * 1.5, true, 1.5);
        assert_eq!(fit_1x.wordmark, fit_15x.wordmark);
        assert_eq!(fit_1x.back_label, fit_15x.back_label);
        assert_eq!(fit_1x.new_compare_label, fit_15x.new_compare_label);
        assert!((fit_15x.used() - fit_1x.used() * 1.5).abs() < 0.01);
    }

    #[test]
    fn a_side_without_a_line_is_filler_and_never_a_diff_tint() {
        let design = Design::default();
        // The middle has no line where the right does: the change lives on the
        // right cell, and the middle|right boundary marks it.
        let row = ThreeWayRow {
            kind: ThreeWayDiffKind::RightChanged,
            left: None,
            middle: None,
            right: Some(1),
        };

        let peers = three_way_tints(row, ThreeWayCompareMode::NoBase, &design);
        assert_eq!(peers.left, CellPaint::Filler);
        assert_eq!(peers.middle, CellPaint::Filler);
        assert_eq!(peers.right, CellPaint::Tinted(design.accent_soft));
        assert_eq!(peers.middle_boundary, Some(design.accent));
        assert_eq!(peers.left_boundary, None);

        // The base mode has the same rule: only the side with content carries
        // the tint.
        let base = three_way_tints(row, ThreeWayCompareMode::MiddleBase, &design);
        assert_eq!(base.left, CellPaint::Filler);
        assert_eq!(base.middle, CellPaint::Filler);
        assert_eq!(base.right, CellPaint::Tinted(design.diff_added_bg));

        // A row where the left is empty but the middle has a line marks the
        // left|middle boundary instead.
        let left_empty = ThreeWayRow {
            kind: ThreeWayDiffKind::LeftChanged,
            left: None,
            middle: Some(2),
            right: Some(2),
        };
        let tints = three_way_tints(left_empty, ThreeWayCompareMode::NoBase, &design);
        assert_eq!(tints.left, CellPaint::Filler);
        assert_eq!(tints.middle, CellPaint::Tinted(design.accent_soft));
        assert_eq!(tints.left_boundary, Some(design.accent));
        assert_eq!(tints.middle_boundary, None);
    }

    #[test]
    fn the_filler_stays_apart_from_every_diff_tint() {
        let design = Design::default();
        let filler = filler_background(&design);
        for tint in [
            design.accent_soft,
            design.diff_added_bg,
            design.diff_removed_bg,
            design.diff_hunk_header,
        ] {
            assert_ne!(filler, tint, "the filler must not read as a diff tint");
        }
    }

    #[gpui::test]
    fn the_compare_header_keeps_its_controls_inside_the_minimum_window(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::{Bounds, WindowBounds, WindowOptions, point, size};

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let name = "a-very-long-source-file-name-that-needs-truncating.rs";
        let left = tmp.path().join(name);
        let middle = tmp.path().join(name);
        let right = tmp.path().join(name);
        for path in [&left, &middle, &right] {
            fs::write(path, "one\ntwo\n").unwrap();
        }
        let target = CompareTarget::ThreeWayFiles {
            left,
            middle,
            right,
        };

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            crate::actions::bind(cx);
        });

        let window = cx
            .update(|cx| {
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(0.0), px(0.0)),
                            size(px(crate::style::MIN_WINDOW_SIZE.0), px(600.0)),
                        ))),
                        ..Default::default()
                    },
                    move |window, cx| {
                        let workspace =
                            cx.new(|cx| Workspace::new_with_compare(target, window, cx));
                        cx.new(|cx| gpui_component::Root::new(workspace, window, cx))
                    },
                )
            })
            .expect("open window");
        let mut visual_cx = gpui::VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();
        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        let controls = visual_cx
            .debug_bounds("compare-header-controls")
            .expect("the header controls are painted");
        let header = visual_cx
            .debug_bounds("compare-header")
            .expect("the header is painted");
        assert!(
            controls.right() <= header.right() && controls.left() >= header.left(),
            "the controls have to stay visible at the minimum width: {controls:?} in {header:?}"
        );
        // One row of controls is at most the title bar's own height; the
        // second row this bar used to grow sits well past it.
        assert!(
            header.size.height <= px(crate::style::TITLE_BAR_HEIGHT),
            "the title bar stays one row tall: {header:?}"
        );
    }
}
