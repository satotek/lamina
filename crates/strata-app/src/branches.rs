//! Branch management: the sidebar's ref sections, their context menus, and the
//! create/delete dialogs.
//!
//! Nothing here runs git. Every write is an [`Operation`] handed to the
//! window's [`RepoView`], which gives it to the session worker; the sections
//! only read the snapshot the view has already published. The helpers that do
//! not need a window (name validation, upstream labels, remote-to-local name
//! mapping) are unit tested without a repository.
//!
//! The submodule and worktree sections are flattened here too, because they are
//! part of the same virtualised list and share its folding and scroll anchor.
//! Their data comes from the view's last submodule/worktree answer, not from the
//! snapshot, so the list's cache key carries that answer's generation.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Div, ElementId, Entity,
    FocusHandle, Focusable as _, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, Point, Render, ScrollStrategy, Size, Stateful,
    StatefulInteractiveElement as _, Styled, UniformListScrollHandle, Window, actions, anchored,
    deferred, div, hsla, point, px, uniform_list,
};
use gpui_component::{
    Icon, Sizable as _,
    checkbox::Checkbox,
    input::{Input, InputState},
    menu::{PopupMenu, PopupMenuItem},
    tooltip::Tooltip,
};
use gpui_kit_assets::IconName;
use strata_core::repo::{SubmoduleInfo, SubmoduleStatus, WorktreeInfo};
use strata_core::session::{HistoryScope, Operation, OperationId, RemoteBranch};
use strata_core::{HeadInfo, RefInfo, RefKind, RemoteInfo, UpstreamInfo};

use crate::data::{SidebarData, Source};
use crate::pull_requests::BranchPullRequests;
use crate::repo_view::RepoView;
use crate::repo_view::rebase_onto::rebase_menu_item;
use crate::scrollbar::WithScrollbar as _;
use crate::style::{self, Design};
use crate::ui::RowTipExt as _;

/// Key context the dialog sets while it is open, so `Enter` and `Escape` mean
/// confirm and cancel only there.
const BRANCH_DIALOG: &str = "BranchDialog";

/// Height of one line of the sidebar list. `uniform_list` lays every item out
/// at the height it measured once, so headers, rows and placeholders must all
/// be this tall; it matches [`crate::ui::list_row`]'s row height.
const ITEM_HEIGHT: f32 = 28.0;

actions!(
    strata,
    [
        CreateBranchFromHead,
        ConfirmBranchDialog,
        CancelBranchDialog,
    ]
);

/// Binds the branch commands. Their handlers live on `RepoView`'s root
/// element, which is on the focus path whenever the history view has focus.
pub fn bind(cx: &mut App) {
    crate::keymap::bind_group(cx, crate::keymap::Group::Branches);
}

/// The colours the sections and dialogs paint with, taken from the window
/// chrome's palette so the sidebar reads as one surface.
#[derive(Clone)]
pub(crate) struct Colors {
    pub(crate) design: Design,
    pub(crate) sidebar_foreground: Hsla,
    pub(crate) muted_foreground: Hsla,
    pub(crate) border: Hsla,
    pub(crate) foreground: Hsla,
    pub(crate) danger: Hsla,
    pub(crate) overlay: Hsla,
}

/// One collapsible section of the sidebar.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum Section {
    Local,
    Remote,
    Tags,
    Stashes,
    Submodules,
    Worktrees,
}

impl Section {
    pub(crate) const ALL: [Section; 6] = [
        Section::Local,
        Section::Remote,
        Section::Tags,
        Section::Stashes,
        Section::Submodules,
        Section::Worktrees,
    ];

    pub(crate) fn index(self) -> usize {
        match self {
            Section::Local => 0,
            Section::Remote => 1,
            Section::Tags => 2,
            Section::Stashes => 3,
            Section::Submodules => 4,
            Section::Worktrees => 5,
        }
    }

    /// Stable id for the section's elements when it is drawn outside the list,
    /// in the collapsed rail.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Section::Local => "local",
            Section::Remote => "remote",
            Section::Tags => "tags",
            Section::Stashes => "stashes",
            Section::Submodules => "submodules",
            Section::Worktrees => "worktrees",
        }
    }

    pub(crate) fn title(self) -> String {
        match self {
            Section::Local => t!("branch.section.branches").to_string(),
            Section::Remote => t!("branch.section.remotes").to_string(),
            Section::Tags => t!("branch.section.tags").to_string(),
            Section::Stashes => t!("branch.section.stashes").to_string(),
            Section::Submodules => t!("branch.section.submodules").to_string(),
            Section::Worktrees => t!("branch.section.worktrees").to_string(),
        }
    }

    pub(crate) fn icon(self) -> &'static [u8] {
        match self {
            Section::Local => style::glyph::BRANCH,
            Section::Remote => style::glyph::CLOUD,
            Section::Tags => style::glyph::TAG,
            Section::Stashes => style::glyph::ARCHIVE,
            Section::Submodules => style::glyph::SUBMODULE,
            Section::Worktrees => style::glyph::WORKTREE,
        }
    }

    /// How many entries the section lists.
    fn count(self, data: &SidebarData, extras: &SidebarExtras<'_>) -> usize {
        match self {
            Section::Local => data.local.len(),
            Section::Remote => data.remote.len(),
            Section::Tags => data.tags.len(),
            Section::Stashes => data.stashes.len(),
            Section::Submodules => extras.submodules.len(),
            Section::Worktrees => extras.worktrees.len(),
        }
    }

    /// Whether `history` settings hide this section.
    fn hidden(self, design: &Design) -> bool {
        match self {
            Section::Local => false,
            Section::Remote => !design.show_remote_branches,
            Section::Tags => !design.show_tags,
            Section::Stashes => !design.show_stashes,
            Section::Submodules | Section::Worktrees => false,
        }
    }

    /// Whether the repository has anything for this section.
    ///
    /// Only the submodule and worktree sections hide when empty: a repository
    /// without submodules, or with nothing but its main worktree, has nothing
    /// to fold, while the ref sections keep their heading and count.
    fn present(self, extras: &SidebarExtras<'_>) -> bool {
        match self {
            Section::Submodules => !extras.submodules.is_empty(),
            Section::Worktrees => extras.worktrees.len() > 1,
            Section::Local | Section::Remote | Section::Tags | Section::Stashes => true,
        }
    }
}

/// What the submodule and worktree sections list, and the generation of the
/// answer they came from.
///
/// Neither list is part of a snapshot, so the caller reads them from the view's
/// cached answer. The generation is empty for `--fake` and for the frames before
/// the first answer arrives.
#[derive(Default)]
pub(crate) struct SidebarExtras<'a> {
    pub(crate) submodules: &'a [SubmoduleInfo],
    pub(crate) worktrees: &'a [WorktreeInfo],
    /// This window's own worktree, marked like the HEAD branch.
    pub(crate) current_worktree: Option<&'a Path>,
    /// Generation of the last submodule/worktree answer; part of the list's
    /// cache key, because a new answer changes the rows without a new snapshot.
    pub(crate) revision: u64,
}

/// One section of the sidebar as the collapsed rail lists it.
pub(crate) struct SectionSummary {
    pub(crate) section: Section,
    pub(crate) title: String,
    pub(crate) count: usize,
    /// The section's own glyph, so the rail reads like the expanded headings.
    pub(crate) icon: &'static [u8],
}

/// Where a new branch should start. The label is user-facing, e.g. `from main`
/// or `from commit 4f2a1c9`.
pub(crate) struct BranchStart {
    pub(crate) point: Option<String>,
    pub(crate) label: String,
}

/// The sidebar entry whose tip the history last jumped to.
///
/// It stays highlighted until another ref is revealed or a commit row is
/// selected; the highlight says where the view came from, not what is checked
/// out, which is why it is kept apart from HEAD.
#[derive(Clone, Default)]
pub(crate) struct RevealState {
    revealed: Option<RevealedRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RevealedRef {
    Ref(RefKind, String),
    Stash(usize),
}

impl RevealState {
    fn reveal_ref(&mut self, kind: RefKind, short_name: &str) {
        self.revealed = Some(RevealedRef::Ref(kind, short_name.to_owned()));
    }

    fn reveal_stash(&mut self, index: usize) {
        self.revealed = Some(RevealedRef::Stash(index));
    }

    fn clear(&mut self) {
        self.revealed = None;
    }

    fn is_ref(&self, kind: RefKind, short_name: &str) -> bool {
        matches!(
            &self.revealed,
            Some(RevealedRef::Ref(revealed_kind, name))
                if *revealed_kind == kind && name == short_name
        )
    }

    fn is_stash(&self, index: usize) -> bool {
        matches!(self.revealed, Some(RevealedRef::Stash(revealed)) if revealed == index)
    }
}

/// One line of the sidebar's virtualised list.
///
/// The list owns its strings, so drawing a frame holds nothing borrowed from
/// the snapshot; the parts that change without the refs (the revealed row,
/// HEAD, busy) stay out and are read while the visible rows are built.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarItem {
    SectionHeader {
        section: Section,
        count: usize,
        open: bool,
    },
    /// A remote's own collapsible heading, above its branch list.
    RemoteGroup {
        name: String,
        /// Shown as the header's tooltip, for a remote that was never fetched.
        fetch_url: Option<String>,
        count: usize,
        collapsed: bool,
    },
    /// A folder built from a path segment shared by several refs.
    Folder {
        /// Section and path the fold is stored under; see [`folder_key`].
        key: String,
        /// The last path segment, which is the row's label.
        segment: String,
        /// How many refs sit below the folder.
        count: usize,
        open: bool,
        depth: usize,
        /// The section the folder belongs to, for its indent under a remote.
        section: Section,
    },
    LocalBranch {
        name: String,
        /// The row's label: the last path segment, or the whole name while a
        /// filter is active.
        label: String,
        /// Position in the section's natural order, for the element id.
        index: usize,
        depth: usize,
        /// `↑2 ↓1`, `?`, or nothing when level with or missing the upstream.
        upstream: Option<String>,
        /// Whether the tracked upstream branch was deleted on the remote.
        upstream_gone: bool,
        /// Why the upstream shows a marker, for its tooltip.
        upstream_note: Option<String>,
    },
    RemoteBranch {
        name: String,
        /// The row's label: the path below the remote, or its last segment
        /// when a folder provides the rest.
        label: String,
        index: usize,
        depth: usize,
        /// The local branch `origin/foo` stands for; `None` for `origin/HEAD`.
        local_name: Option<String>,
        /// Whether that local branch exists already.
        existing: bool,
    },
    Tag {
        name: String,
        label: String,
        index: usize,
        depth: usize,
    },
    Stash {
        /// `stash@{N}: message`, as the snapshot lists it.
        name: String,
        index: usize,
    },
    /// One entry of `.gitmodules` and how its checkout compares with HEAD.
    Submodule {
        index: usize,
        name: String,
        /// Repository-relative path, which is what an operation addresses.
        path: String,
        /// Path, URL and both commits, for the row's tooltip.
        detail: String,
        status: SubmoduleStatus,
    },
    /// One checkout registered under `worktrees/`, the main one included.
    Worktree {
        index: usize,
        /// Branch name, or `detached <short id>`.
        label: String,
        path: String,
        /// Path and lock reason, for the row's tooltip.
        detail: String,
        /// The lock reason alone, for the t!("branch.worktree.locked").to_string() badge's tooltip.
        lock_reason: Option<String>,
        is_main: bool,
        locked: bool,
        prunable: bool,
        /// This window's own worktree, drawn like the HEAD branch.
        current: bool,
    },
    /// The line a remote with no fetched branches shows under its heading.
    Placeholder { text: String, remote: String },
}

/// Identity of a list position that survives a snapshot refresh, so the row
/// under the viewport can be found again by what it is rather than by index.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ItemKey {
    Section(Section),
    RemoteGroup(String),
    Folder(String),
    LocalBranch(String),
    RemoteBranch(String),
    Tag(String),
    Stash(usize),
    Submodule(String),
    Worktree(String),
    RemoteEmpty(String),
}

impl SidebarItem {
    fn key(&self) -> ItemKey {
        match self {
            SidebarItem::SectionHeader { section, .. } => ItemKey::Section(*section),
            SidebarItem::RemoteGroup { name, .. } => ItemKey::RemoteGroup(name.clone()),
            SidebarItem::Folder { key, .. } => ItemKey::Folder(key.clone()),
            SidebarItem::LocalBranch { name, .. } => ItemKey::LocalBranch(name.clone()),
            SidebarItem::RemoteBranch { name, .. } => ItemKey::RemoteBranch(name.clone()),
            SidebarItem::Tag { name, .. } => ItemKey::Tag(name.clone()),
            SidebarItem::Stash { index, .. } => ItemKey::Stash(*index),
            SidebarItem::Submodule { path, .. } => ItemKey::Submodule(path.clone()),
            SidebarItem::Worktree { path, .. } => ItemKey::Worktree(path.clone()),
            SidebarItem::Placeholder { remote, .. } => ItemKey::RemoteEmpty(remote.clone()),
        }
    }

    /// Whether this item is the row `key` names. Compared without building a
    /// key, so finding the scroll anchor does not allocate per entry.
    fn is(&self, key: &ItemKey) -> bool {
        match (self, key) {
            (SidebarItem::SectionHeader { section, .. }, ItemKey::Section(key)) => section == key,
            (SidebarItem::RemoteGroup { name, .. }, ItemKey::RemoteGroup(key)) => name == key,
            (SidebarItem::Folder { key: folder, .. }, ItemKey::Folder(key)) => folder == key,
            (SidebarItem::LocalBranch { name, .. }, ItemKey::LocalBranch(key)) => name == key,
            (SidebarItem::RemoteBranch { name, .. }, ItemKey::RemoteBranch(key)) => name == key,
            (SidebarItem::Tag { name, .. }, ItemKey::Tag(key)) => name == key,
            (SidebarItem::Stash { index, .. }, ItemKey::Stash(key)) => index == key,
            (SidebarItem::Submodule { path, .. }, ItemKey::Submodule(key)) => path == key,
            (SidebarItem::Worktree { path, .. }, ItemKey::Worktree(key)) => path == key,
            (SidebarItem::Placeholder { remote, .. }, ItemKey::RemoteEmpty(key)) => remote == key,
            _ => false,
        }
    }
}

/// The flattened list, rebuilt only when its inputs change, so a frame costs
/// the rows on screen rather than the size of the repository.
#[derive(Default)]
struct ItemCache {
    key: Option<ItemsKey>,
    items: Rc<Vec<SidebarItem>>,
}

#[derive(PartialEq, Eq)]
struct ItemsKey {
    /// Snapshot generation; every publish bumps it.
    revision: u64,
    /// Generation of the last submodule/worktree answer, which arrives outside
    /// the snapshot and changes those rows on its own.
    lists: u64,
    /// Bumped when a section, a remote group or a folder folds.
    local: u64,
    /// The ref a "show only" scope limits the list to; a different filter
    /// lists different rows without a new snapshot.
    filter: Option<String>,
    show_remote_branches: bool,
    show_tags: bool,
    show_stashes: bool,
}

/// The list drawn last frame, so a refresh can put the same row back under
/// the viewport instead of the same index.
#[derive(Default)]
struct ScrollAnchor {
    items: Option<Rc<Vec<SidebarItem>>>,
}

/// A selectable submodule or worktree row in the sidebar.
///
/// Selecting one highlights it and lets `Enter` open it in the current window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExtraSelection {
    Submodule(String),
    Worktree(PathBuf),
}

/// The fold key of a folder: the section, the remote whose branches it groups,
/// and the path inside that section. Two remotes can both have a `t3` folder,
/// and a tag and a branch share a segment without sharing a fold.
fn folder_key(section: Section, remote: Option<&str>, path: &str) -> String {
    match remote {
        Some(remote) => format!("{}:{remote}:{path}", section.id()),
        None => format!("{}:{path}", section.id()),
    }
}

/// The folder keys above `name`, e.g. `a`, `a/b` for `a/b/c`.
fn folder_ancestors(section: Section, remote: Option<&str>, name: &str) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut end = 0;
    while let Some(offset) = name[end..].find('/') {
        end += offset;
        ancestors.push(folder_key(section, remote, &name[..end]));
        end += 1;
    }
    ancestors
}

/// One node of the tree a section's ref names build.
#[derive(Debug, PartialEq, Eq)]
enum RefNode<T> {
    Leaf {
        /// The row's label: the last path segment, or the whole remaining path
        /// when this leaf stands in for a folder that would have split it.
        label: String,
        /// The path inside the section.
        path: String,
        value: T,
    },
    Folder {
        /// The path inside the section, e.g. `t3`.
        path: String,
        /// The last path segment, the folder row's label.
        segment: String,
        children: Vec<RefNode<T>>,
    },
}

impl<T> RefNode<T> {
    /// How many refs sit below the node.
    fn leaf_count(&self) -> usize {
        match self {
            RefNode::Leaf { .. } => 1,
            RefNode::Folder { children, .. } => children.iter().map(RefNode::leaf_count).sum(),
        }
    }
}

/// A branch row being dragged onto another branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DraggedBranch {
    pub(crate) kind: RefKind,
    pub(crate) name: String,
}

/// What a branch drop menu can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropAction {
    Merge,
    Rebase,
    FastForward,
}

/// One entry of the menu a branch dropped on another opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DropEntry {
    pub(crate) action: DropAction,
    pub(crate) label: String,
    /// Why the entry is disabled, drawn after the label.
    pub(crate) blocked: Option<String>,
}

/// The menu a branch dropped on another branch opens, at the drop point.
struct DropMenu {
    position: Point<Pixels>,
    source: DraggedBranch,
    target: String,
    entries: Vec<DropEntry>,
}

/// Everything the branch UI owns between frames.
#[derive(Default)]
pub(crate) struct Branches {
    collapsed: [bool; Section::ALL.len()],
    /// Remote name -> collapsed, so each remote's branch list folds on its own.
    remote_groups: HashMap<String, bool>,
    /// The ref revealed last, drawn with the neutral tint until a commit is
    /// selected.
    reveal: RevealState,
    /// Selected submodule or worktree row in the sidebar.
    selected_extra: Option<ExtraSelection>,
    /// The ref history is limited to, as its short name, while a "show only"
    /// scope is active. The scope itself lives in the session.
    scope: Option<String>,
    /// What the sidebar list is limited to. "Show only" sets it to the same
    /// ref as the scope; a filter on its own hides every ref that does not
    /// match and draws the matches by their whole path.
    filter: Option<String>,
    /// The folders the user folded, keyed by [`folder_key`]. A folder that is
    /// not in the set is open, so a folder seen for the first time shows its
    /// refs; the set lives with the view, like the sections' own folds.
    folders: RefCell<HashSet<String>>,
    /// The branch the folders were last laid out for, so a branch switch opens
    /// the folder the new branch lives in.
    head_seen: RefCell<Option<String>>,
    /// The folder the left and right arrows act on: the one clicked or hovered
    /// last, since the list has no row cursor of its own.
    folder_focus: RefCell<Option<String>>,
    /// The menu a branch dropped on another branch opened.
    drop_menu: Option<DropMenu>,
    dialog: Option<Dialog>,
    /// Operations this module started, keyed by the id the session returned.
    /// The label is what the status bar prefixes a failure with.
    running: HashMap<OperationId, Running>,
    /// Last viewport size seen, so a dialog opened from an operation result can
    /// still paint a window-sized backdrop.
    viewport: Size<Pixels>,
    /// Focus target for the sidebar section list, so `Enter` can open a
    /// selected submodule or worktree row.
    list_focus: Option<FocusHandle>,
    /// Focus target for dialog cards. Created once by the view because a
    /// `FocusHandle` needs an `App`, and reused: only one dialog is open and it
    /// is the only element tracking the handle while it lives.
    dialog_focus: Option<FocusHandle>,
    /// A destructive operation git refused for a reason the stronger form can
    /// override. The view offers it through [`crate::ui::confirm`], which needs
    /// a window the session event does not have.
    force_retry: Option<Retry>,
    /// The section list's scroll position, shared by the list and its scrollbar.
    scroll: UniformListScrollHandle,
    /// A section the collapsed rail asked for: the next drawn list scrolls
    /// its heading into view.
    pending_scroll: RefCell<Option<Section>>,
    cache: RefCell<ItemCache>,
    /// Bumped by every fold, so the cached list cannot outlive a toggle.
    local_revision: u64,
    anchor: RefCell<ScrollAnchor>,
}

/// What git refused, and the stronger form of the same operation.
///
/// Only these two are offered again: git's refusal is what says the user has to
/// decide whether losing the work is acceptable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// `git branch -d` refused a branch that is not fully merged.
    DeleteBranch { name: String, forced: bool },
    /// `git worktree remove` refused a checkout with modified or untracked files.
    RemoveWorktree { path: PathBuf, forced: bool },
}

impl Retry {
    /// Whether `error` is the refusal this retry exists for.
    fn refused_by(&self, error: &str) -> bool {
        match self {
            Retry::DeleteBranch { .. } => is_not_fully_merged(error),
            Retry::RemoveWorktree { .. } => is_dirty_worktree(error),
        }
    }

    /// The same operation with the flag that overrides the refusal, or `None`
    /// when it already carried it: a forced retry that fails again is reported
    /// instead of offered a third time.
    fn stronger(&self) -> Option<Self> {
        match self {
            Retry::DeleteBranch {
                name,
                forced: false,
            } => Some(Retry::DeleteBranch {
                name: name.clone(),
                forced: true,
            }),
            Retry::RemoveWorktree {
                path,
                forced: false,
            } => Some(Retry::RemoveWorktree {
                path: path.clone(),
                forced: true,
            }),
            Retry::DeleteBranch { forced: true, .. }
            | Retry::RemoveWorktree { forced: true, .. } => None,
        }
    }
}

struct Running {
    label: String,
    /// Set for an unforced delete or removal, which can be offered again with
    /// the force flag.
    retry: Option<Retry>,
}

struct Dialog {
    kind: DialogKind,
    viewport: Size<Pixels>,
    /// Focused while the dialog is open, so its `Enter` and `Escape` bindings
    /// apply and the keys no longer reach the commit list.
    focus: Option<FocusHandle>,
}

enum DialogKind {
    Create(CreateForm),
    Rename(RenameForm),
    Upstream(UpstreamForm),
    AddRemote(AddRemoteForm),
    RenameRemote(RenameRemoteForm),
    EditRemote(EditRemoteForm),
}

struct CreateForm {
    input: Entity<InputState>,
    start: BranchStart,
    checkout: bool,
    error: Option<String>,
}

/// The rename dialog: the current name in an input, and, for a branch that
/// has an upstream, the option to rename that remote branch too.
struct RenameForm {
    input: Entity<InputState>,
    branch: String,
    /// The branch's configured remote branch, when it has one. This is what a
    /// remote-side rename deletes and re-points.
    upstream: Option<RemoteBranch>,
    /// `origin/topic`, for the dialog's "current upstream" line.
    upstream_label: Option<String>,
    rename_remote: bool,
    error: Option<String>,
}

/// One remote branch the set-upstream dialog can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpstreamChoice {
    /// `origin/main`, as the sidebar labels it.
    pub(crate) label: String,
    pub(crate) remote: String,
    pub(crate) remote_branch: String,
}

struct UpstreamForm {
    branch: String,
    choices: Vec<UpstreamChoice>,
    selected: Option<usize>,
    /// The upstream the branch has now, if any.
    current: Option<String>,
    error: Option<String>,
}

struct AddRemoteForm {
    name: Entity<InputState>,
    url: Entity<InputState>,
    fetch: bool,
    error: Option<String>,
}

/// Renaming a remote keeps its URLs and moves its tracking branches; the
/// dialog only needs the new name.
struct RenameRemoteForm {
    input: Entity<InputState>,
    name: String,
    error: Option<String>,
}

/// The edit-URL dialog. The push field only exists for a remote that already
/// pushes somewhere other than it fetches.
struct EditRemoteForm {
    name: String,
    fetch: Entity<InputState>,
    push: Option<Entity<InputState>>,
    error: Option<String>,
}

/// The validated contents of the create dialog.
pub(crate) struct CreateRequest {
    pub(crate) name: String,
    pub(crate) start_point: Option<String>,
    pub(crate) checkout: bool,
}

/// The validated contents of the rename dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenameRequest {
    pub(crate) name: String,
    pub(crate) new_name: String,
    /// The remote branch to rename along with the local one, or `None` when
    /// the user left that unchecked (or the branch has no upstream).
    pub(crate) rename_upstream: Option<RemoteBranch>,
}

impl RenameRequest {
    /// Validates the typed name and drops the remote-side rename when there is
    /// no upstream to rename.
    fn parse(
        name: &str,
        typed: &str,
        upstream: Option<&RemoteBranch>,
        rename_remote: bool,
    ) -> Result<Self, String> {
        let new_name = typed.trim();
        if new_name == name {
            return Err(t!("branch.validation.name_differs").to_string().to_owned());
        }
        validate_branch_name(new_name)?;
        Ok(Self {
            name: name.to_owned(),
            new_name: new_name.to_owned(),
            rename_upstream: if rename_remote {
                upstream.cloned()
            } else {
                None
            },
        })
    }
}

/// The validated contents of the add-remote dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddRemoteRequest {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) fetch: bool,
}

impl AddRemoteRequest {
    fn parse(typed_name: &str, typed_url: &str, fetch: bool) -> Result<Self, String> {
        let name = typed_name.trim();
        validate_remote_name(name)?;
        let url = validate_remote_url(typed_url)?;
        Ok(Self {
            name: name.to_owned(),
            url: url.to_owned(),
            fetch,
        })
    }
}

/// The validated contents of the edit-URL dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditRemoteUrlRequest {
    pub(crate) name: String,
    pub(crate) fetch_url: String,
    pub(crate) push_url: Option<String>,
}

impl EditRemoteUrlRequest {
    fn parse(name: &str, typed_fetch: &str, typed_push: Option<&str>) -> Result<Self, String> {
        let fetch_url = validate_remote_url(typed_fetch)?;
        let push_url = match typed_push {
            Some(push) => Some(
                validate_remote_url(push)
                    .map_err(|_| t!("branch.validation.push_url_hint").to_string().to_owned())?
                    .to_owned(),
            ),
            None => None,
        };
        Ok(Self {
            name: name.to_owned(),
            fetch_url: fetch_url.to_owned(),
            push_url,
        })
    }
}

/// What the open dialog submits, once its fields validate.
pub(crate) enum DialogRequest {
    Create(CreateRequest),
    Rename(RenameRequest),
    SetUpstream {
        branch: String,
        remote: String,
        remote_branch: String,
    },
    AddRemote(AddRemoteRequest),
    RenameRemote {
        name: String,
        new_name: String,
    },
    EditRemoteUrl(EditRemoteUrlRequest),
}

impl Branches {
    /// Renders the virtualised ref sections and, when one is open, the dialog
    /// on top of the window.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &self,
        data: SidebarData,
        revision: u64,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
        extras: &SidebarExtras<'_>,
        pull_requests: &BranchPullRequests,
        enabled: bool,
        busy: bool,
        hidden_refs: &[String],
        colors: &Colors,
        entity: &Entity<RepoView>,
    ) -> AnyElement {
        let head = data.head_branch.clone();
        let items = self.items(data, revision, upstreams, remotes, extras, &colors.design);
        self.restore_scroll(&items);
        self.scroll_to_requested_section(&items);

        let reveal = self.reveal.clone();
        let selected_extra = self.selected_extra.clone();
        let list_colors = colors.clone();
        let list_entity = entity.clone();
        let list_pull_requests = pull_requests.clone();
        let list_hidden = hidden_refs.to_vec();
        let list = uniform_list("sidebar-refs", items.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    item_element(
                        &items[index],
                        head.as_deref(),
                        &reveal,
                        selected_extra.as_ref(),
                        &list_pull_requests,
                        enabled,
                        busy,
                        &list_hidden,
                        &list_colors,
                        list_entity.clone(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        .with_scrollbar(&self.scroll)
        .flex_1()
        .min_h_0();

        let mut column = div().flex().flex_col().flex_1().min_h_0().min_w_0();
        if let Some(focus) = &self.list_focus {
            let key_entity = entity.clone();
            column = column
                .track_focus(focus)
                .on_key_down(move |event, window, cx| {
                    let modifiers = event.keystroke.modifiers;
                    let plain = !modifiers.alt
                        && !modifiers.control
                        && !modifiers.platform
                        && !modifiers.shift
                        && !modifiers.function;
                    if !plain {
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "enter" => {
                            let had_selection = key_entity.update(cx, |view, cx| {
                                if view.branches.selected_extra().is_some() {
                                    view.activate_selected_extra(window, cx);
                                    true
                                } else {
                                    false
                                }
                            });
                            if had_selection {
                                cx.stop_propagation();
                            }
                        }
                        "escape" => {
                            let cleared = key_entity.update(cx, |view, cx| {
                                if view.branches.selected_extra().is_some() {
                                    view.branches.clear_selected_extra();
                                    cx.notify();
                                    true
                                } else {
                                    false
                                }
                            });
                            if cleared {
                                cx.stop_propagation();
                            }
                        }
                        // A folder folds and unfolds like a section; the folder
                        // last clicked or hovered is the one the arrows act on.
                        "left" | "right" => {
                            let open = event.keystroke.key == "right";
                            let changed = key_entity.update(cx, |view, cx| {
                                if view.branches.dialog_open() {
                                    return false;
                                }
                                let Some(folder) = view.branches.focused_folder() else {
                                    return false;
                                };
                                let current = !view.branches.folder_folded(&folder);
                                if current == open {
                                    return false;
                                }
                                let changed = view.branches.set_folder_open(&folder, open);
                                if changed {
                                    cx.notify();
                                }
                                changed
                            });
                            if changed {
                                cx.stop_propagation();
                            }
                        }
                        _ => {}
                    }
                });
        }
        column = column.child(list);
        if let Some(menu) = self.drop_menu_overlay(colors, entity) {
            column = column.child(menu);
        }
        if let Some(dialog) = self.dialog_overlay(colors, entity) {
            column = column.child(dialog);
        }
        column.into_any_element()
    }

    /// The flattened list, rebuilt only when the snapshot or a fold changes.
    ///
    /// `revision` is the snapshot's generation, which every publish bumps; that
    /// is what lets the cached strings outlive the frame they were copied in.
    fn items(
        &self,
        data: SidebarData,
        revision: u64,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
        extras: &SidebarExtras<'_>,
        design: &Design,
    ) -> Rc<Vec<SidebarItem>> {
        let key = ItemsKey {
            revision,
            lists: extras.revision,
            local: self.local_revision,
            filter: self.filter.clone(),
            show_remote_branches: design.show_remote_branches,
            show_tags: design.show_tags,
            show_stashes: design.show_stashes,
        };
        let mut cache = self.cache.borrow_mut();
        if cache.key.as_ref() == Some(&key) {
            return cache.items.clone();
        }
        let items = Rc::new(self.flatten(data, upstreams, remotes, extras, design));
        cache.key = Some(key);
        cache.items = items.clone();
        items
    }

    /// Every visible line of the section list, in drawing order: a heading for
    /// each shown section, then its rows, grouped into folders where their
    /// paths share a segment. A collapsed section or folder contributes its
    /// row alone, and a section with nothing in it is just its heading.
    fn flatten(
        &self,
        data: SidebarData,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
        extras: &SidebarExtras<'_>,
        design: &Design,
    ) -> Vec<SidebarItem> {
        // Counted before the lists are destructured: the heading's number and
        // the rail's are the same number.
        let counts = Section::ALL.map(|section| section.count(&data, extras));
        let SidebarData {
            local,
            remote,
            mut tags,
            mut stashes,
            head_branch,
            ..
        } = data;
        let mut items = Vec::with_capacity(
            local.len()
                + remote.len()
                + tags.len()
                + stashes.len()
                + extras.submodules.len()
                + extras.worktrees.len()
                + 10,
        );
        self.open_head_folder(head_branch.as_deref());

        // The folder the history was last revealed from stays open for as long
        // as its ref row is highlighted, so the tint is never hidden in a fold.
        let mut forced = HashSet::new();
        if let Some(head) = head_branch.as_deref() {
            forced.extend(folder_ancestors(Section::Local, None, head));
        }
        if let Some(RevealedRef::Ref(kind, name)) = self.reveal.revealed.as_ref() {
            match kind {
                RefKind::LocalBranch => {
                    forced.extend(folder_ancestors(Section::Local, None, name));
                }
                RefKind::Tag => {
                    forced.extend(folder_ancestors(Section::Tags, None, name));
                }
                RefKind::RemoteBranch => {
                    if let Some((remote_name, path)) = name.split_once('/') {
                        forced.extend(folder_ancestors(Section::Remote, Some(remote_name), path));
                    }
                }
            }
        }
        let folded = self.folders.borrow();
        // The "show only" scope is the sidebar's filter: while it is on, the
        // list shows the refs it matches by their whole path and cannot group
        // them into folders, which would hide the match behind a row.
        let filter = self
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|filter| !filter.is_empty());

        for section in Section::ALL {
            if section.hidden(design) || !section.present(extras) {
                continue;
            }
            let count = counts[section.index()];
            let open = !self.collapsed[section.index()];
            items.push(SidebarItem::SectionHeader {
                section,
                count,
                open,
            });
            if !open {
                continue;
            }

            match section {
                Section::Local => {
                    let entries: Vec<(String, LocalLeaf)> = natural_sorted(&local)
                        .into_iter()
                        .enumerate()
                        .map(|(index, name)| {
                            (
                                name.to_owned(),
                                LocalLeaf {
                                    name: name.to_owned(),
                                    index,
                                },
                            )
                        })
                        .collect();
                    let mut leaf = |value: LocalLeaf, label: &str, depth: usize| {
                        let upstream = upstreams.get(&value.name);
                        SidebarItem::LocalBranch {
                            name: value.name,
                            label: label.to_owned(),
                            index: value.index,
                            depth,
                            upstream: upstream.and_then(upstream_label),
                            upstream_gone: upstream.is_some_and(|info| info.gone),
                            upstream_note: upstream.and_then(upstream_note),
                        }
                    };
                    match filter {
                        Some(filter) => {
                            for (path, entry) in entries {
                                if matches_ref(filter, &entry.name) {
                                    items.push(leaf(entry, &path, 0));
                                }
                            }
                        }
                        None => push_ref_nodes(
                            &mut items,
                            ref_tree(entries),
                            0,
                            Section::Local,
                            None,
                            &folded,
                            &forced,
                            &mut leaf,
                        ),
                    }
                }
                Section::Tags => {
                    let entries: Vec<(String, TagLeaf)> =
                        natural_sorted_owned(std::mem::take(&mut tags))
                            .into_iter()
                            .enumerate()
                            .map(|(index, name)| (name.clone(), TagLeaf { name, index }))
                            .collect();
                    let mut leaf = |value: TagLeaf, label: &str, depth: usize| SidebarItem::Tag {
                        name: value.name,
                        label: label.to_owned(),
                        index: value.index,
                        depth,
                    };
                    match filter {
                        Some(filter) => {
                            for (path, entry) in entries {
                                if matches_ref(filter, &entry.name) {
                                    items.push(leaf(entry, &path, 0));
                                }
                            }
                        }
                        None => push_ref_nodes(
                            &mut items,
                            ref_tree(entries),
                            0,
                            Section::Tags,
                            None,
                            &folded,
                            &forced,
                            &mut leaf,
                        ),
                    }
                }
                // Stash rows keep `stash@{N}` order: the index is the identity.
                Section::Stashes => {
                    for (index, name) in std::mem::take(&mut stashes).into_iter().enumerate() {
                        items.push(SidebarItem::Stash { name, index });
                    }
                }
                // The submodule list is already sorted by path, which is the
                // order a person reads it in; the name is the row's label.
                Section::Submodules => {
                    for (index, submodule) in extras.submodules.iter().enumerate() {
                        items.push(SidebarItem::Submodule {
                            index,
                            name: submodule.name.clone(),
                            path: submodule.path.clone(),
                            detail: submodule_detail(submodule),
                            status: submodule.status,
                        });
                    }
                }
                // `git worktree list` order is the main worktree, then the
                // registered ones; that is also the order that keeps the one
                // the user is looking at on top.
                Section::Worktrees => {
                    for (index, worktree) in extras.worktrees.iter().enumerate() {
                        items.push(SidebarItem::Worktree {
                            index,
                            label: worktree_label(worktree),
                            path: worktree.path.display().to_string(),
                            detail: worktree_detail(worktree),
                            lock_reason: worktree.lock_reason.clone(),
                            is_main: worktree.is_main,
                            locked: worktree.locked,
                            prunable: worktree.prunable,
                            current: extras
                                .current_worktree
                                .is_some_and(|current| same_path(current, &worktree.path)),
                        });
                    }
                }
                // Remote branches are grouped under their remote, which is the
                // first path component of `origin/main`. A configured remote is
                // listed even before anything was fetched from it.
                Section::Remote => {
                    if remote.is_empty() && remotes.is_empty() {
                        continue;
                    }
                    let groups = remote_groups(&remote, remotes);
                    // Row ids keep the whole section's natural order even when
                    // the filter hides the groups before this one.
                    let mut indices = HashMap::new();
                    let mut next_index = 0;
                    for group in &groups {
                        for name in &group.branches {
                            indices.insert(name.clone(), next_index);
                            next_index += 1;
                        }
                    }
                    for group in groups {
                        let count = match filter {
                            Some(filter) => group
                                .branches
                                .iter()
                                .filter(|name| matches_ref(filter, name))
                                .count(),
                            None => group.branches.len(),
                        };
                        if filter.is_some() && count == 0 {
                            continue;
                        }
                        let collapsed = self
                            .remote_groups
                            .get(&group.name)
                            .copied()
                            .unwrap_or(false);
                        items.push(SidebarItem::RemoteGroup {
                            name: group.name.clone(),
                            fetch_url: group.fetch_url.clone(),
                            count,
                            collapsed,
                        });
                        if collapsed {
                            continue;
                        }
                        if group.branches.is_empty() {
                            items.push(SidebarItem::Placeholder {
                                text: t!("branch.empty.no_branches_fetched").to_string(),
                                remote: group.name,
                            });
                            continue;
                        }
                        let prefix = format!("{}/", group.name);
                        let entries: Vec<(String, RemoteLeaf)> = group
                            .branches
                            .into_iter()
                            .map(|name| {
                                let local_name = local_name_for_remote(&name).map(str::to_owned);
                                let existing = local_name.as_deref().is_some_and(|local_name| {
                                    local.iter().any(|branch| branch == local_name)
                                });
                                let path = name.strip_prefix(&prefix).unwrap_or(&name).to_owned();
                                let index = indices.get(&name).copied().unwrap_or(0);
                                (
                                    path,
                                    RemoteLeaf {
                                        name,
                                        index,
                                        local_name,
                                        existing,
                                    },
                                )
                            })
                            .collect();
                        let mut leaf = |value: RemoteLeaf, label: &str, depth: usize| {
                            SidebarItem::RemoteBranch {
                                name: value.name,
                                label: label.to_owned(),
                                index: value.index,
                                depth,
                                local_name: value.local_name,
                                existing: value.existing,
                            }
                        };
                        match filter {
                            Some(filter) => {
                                for (path, entry) in entries {
                                    if matches_ref(filter, &entry.name) {
                                        items.push(leaf(entry, &path, 0));
                                    }
                                }
                            }
                            None => push_ref_nodes(
                                &mut items,
                                ref_tree(entries),
                                0,
                                Section::Remote,
                                Some(group.name.as_str()),
                                &folded,
                                &forced,
                                &mut leaf,
                            ),
                        }
                    }
                }
            }
        }
        items
    }

    /// Opens the folder the checked-out branch lives in whenever HEAD moves,
    /// so switching branches never leaves the current one hidden in a fold.
    fn open_head_folder(&self, head: Option<&str>) {
        let mut seen = self.head_seen.borrow_mut();
        if seen.as_deref() == head {
            return;
        }
        *seen = head.map(str::to_owned);
        let Some(head) = head else {
            return;
        };
        let mut folded = self.folders.borrow_mut();
        for key in folder_ancestors(Section::Local, None, head) {
            folded.remove(&key);
        }
    }

    /// The sections the collapsed rail lists, in the sidebar's own order.
    ///
    /// The rail shows what the expanded sidebar shows: a section the history
    /// settings hide, or one the repository has nothing for, keeps its icon out
    /// of the rail too.
    pub(crate) fn sections(
        &self,
        data: &SidebarData,
        extras: &SidebarExtras<'_>,
        design: &Design,
    ) -> Vec<SectionSummary> {
        Section::ALL
            .iter()
            .filter(|section| !section.hidden(design) && section.present(extras))
            .map(|section| SectionSummary {
                section: *section,
                title: section.title(),
                count: section.count(data, extras),
                icon: section.icon(),
            })
            .collect()
    }

    /// Unfolds `section` and scrolls its heading into view.
    ///
    /// While the view is collapsed the list is not drawn, so the scroll waits
    /// for the first frame that draws it again; by then the unfolded section is
    /// in the list the heading is found in.
    pub(crate) fn reveal_section(&mut self, section: Section) {
        if self.collapsed[section.index()] {
            self.collapsed[section.index()] = false;
            self.local_revision += 1;
        }
        *self.pending_scroll.borrow_mut() = Some(section);
    }

    /// Brings the heading the rail asked for into view, then forgets it.
    fn scroll_to_requested_section(&self, items: &[SidebarItem]) {
        let Some(section) = self.pending_scroll.borrow_mut().take() else {
            return;
        };
        if let Some(index) = items.iter().position(|item| {
            matches!(item, SidebarItem::SectionHeader { section: found, .. } if *found == section)
        }) {
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        }
    }

    /// Puts the row that was at the top of the viewport back where it was when
    /// a refresh inserts or removes entries above it. The row is remembered by
    /// its key, not its index, so adding a tag above the viewport does not
    /// scroll the list by a row.
    fn restore_scroll(&self, items: &Rc<Vec<SidebarItem>>) {
        let mut anchor = self.anchor.borrow_mut();
        let Some(previous) = anchor.items.replace(items.clone()) else {
            return;
        };
        // Nothing was rebuilt, so the index and the identity agree already.
        if Rc::ptr_eq(&previous, items) || previous.is_empty() {
            return;
        }

        let base = self.scroll.0.borrow().base_handle.clone();
        let height = px(ITEM_HEIGHT);
        let scroll_top = -base.offset().y;
        if scroll_top <= Pixels::ZERO {
            return;
        }

        let mut index = (scroll_top / height).floor() as usize;
        let mut within = scroll_top - height * index as f32;
        while let Some(key) = previous.get(index).map(SidebarItem::key) {
            if let Some(new_index) = items.iter().position(|item| item.is(&key)) {
                if new_index != index {
                    let offset = base.offset();
                    base.set_offset(point(offset.x, -(height * new_index as f32 + within)));
                }
                return;
            }
            // The anchored row left with a section that collapsed; the next
            // surviving row down is the closest place to stay.
            index += 1;
            within = Pixels::ZERO;
        }
    }

    /// The menu a branch dropped on another branch opened, at the drop point.
    ///
    /// It is a small card rather than a context menu because it is not the
    /// consequence of a right-click; the rows are the same list rows the rest
    /// of the sidebar uses. Clicking anywhere else dismisses it.
    fn drop_menu_overlay(&self, colors: &Colors, entity: &Entity<RepoView>) -> Option<AnyElement> {
        let menu = self.drop_menu.as_ref()?;
        let design = &colors.design;
        let dismiss = {
            let entity = entity.clone();
            move |_: &gpui::MouseDownEvent, window: &mut Window, cx: &mut App| {
                entity.update(cx, |view, cx| view.dismiss_drop_menu(window, cx));
            }
        };
        let backdrop = deferred(
            anchored().position(point(px(0.0), px(0.0))).child(
                div()
                    .w(self.viewport.width)
                    .h(self.viewport.height)
                    .occlude()
                    .on_mouse_down(MouseButton::Left, dismiss),
            ),
        )
        .with_priority(1);

        let mut card = div()
            .id("branch-drop-menu")
            .flex()
            .flex_col()
            .gap(px(1.0))
            .min_w(px(220.0))
            .max_w(px(340.0))
            .p(px(4.0))
            .rounded(px(8.0))
            .bg(design.panel)
            .border_1()
            .border_color(design.border)
            .text_size(px(design.small()))
            .text_color(design.text)
            .occlude();
        if let Some(focus) = &self.dialog_focus {
            let close_entity = entity.clone();
            card = card
                .track_focus(focus)
                .on_key_down(move |event, window, cx| {
                    if event.keystroke.key == "escape" {
                        close_entity.update(cx, |view, cx| view.dismiss_drop_menu(window, cx));
                        cx.stop_propagation();
                    }
                });
        }
        for (index, entry) in menu.entries.iter().enumerate() {
            let label = crate::ui::menu_label(entry.label.clone(), entry.blocked.as_ref());
            let mut row = crate::ui::list_row(
                ("branch-drop-entry", index),
                crate::ui::RowState::NORMAL,
                design,
            )
            .h(px(26.0))
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .mx(px(0.0))
            .px_2()
            .rounded(px(5.0))
            .text_size(px(design.small()))
            .text_color(design.text)
            .child(div().min_w_0().truncate().child(label));
            match entry.blocked {
                Some(_) => row = row.opacity(0.6),
                None => {
                    let action = entry.action;
                    let click_entity = entity.clone();
                    row = row.cursor_pointer().on_click(move |_, window, cx| {
                        click_entity
                            .update(cx, |view, cx| view.run_drop_action(action, window, cx));
                    });
                }
            }
            card = card.child(row);
        }

        let card = deferred(anchored().position(menu.position).child(card)).with_priority(2);
        Some(div().child(backdrop).child(card).into_any_element())
    }

    fn dialog_overlay(&self, colors: &Colors, entity: &Entity<RepoView>) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let card = match &dialog.kind {
            DialogKind::Create(form) => create_card(form, colors, entity, dialog.focus.as_ref()),
            DialogKind::Rename(form) => rename_card(form, colors, entity, dialog.focus.as_ref()),
            DialogKind::Upstream(form) => {
                upstream_card(form, colors, entity, dialog.focus.as_ref())
            }
            DialogKind::AddRemote(form) => {
                add_remote_card(form, colors, entity, dialog.focus.as_ref())
            }
            DialogKind::RenameRemote(form) => {
                rename_remote_card(form, colors, entity, dialog.focus.as_ref())
            }
            DialogKind::EditRemote(form) => {
                edit_remote_card(form, colors, entity, dialog.focus.as_ref())
            }
        };
        let viewport = dialog.viewport;

        // A deferred layer so the dialog is not clipped by the sidebar. The app
        // has no `gpui_component::Root` view, so a modal window layer is not
        // available; painting in window coordinates is enough for a dialog.
        Some(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .w(viewport.width)
                        .h(viewport.height)
                        .flex()
                        .items_center()
                        .justify_center()
                        .occlude()
                        .bg(hsla(0.0, 0.0, 0.0, 0.35))
                        .child(card),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    pub(crate) fn start(&mut self, id: OperationId, label: String, retry: Option<Retry>) {
        self.running.insert(id, Running { label, retry });
    }

    /// Takes the operation to offer again with its force flag, if git refused
    /// an unforced one.
    pub(crate) fn take_force_retry(&mut self) -> Option<Retry> {
        self.force_retry.take()
    }

    /// Records that an operation finished. Returns the label it was started
    /// with so the caller can name the operation in the status bar, and opens
    /// the force offer when git refused an unforced delete or removal.
    pub(crate) fn finish(&mut self, id: OperationId, error: Option<&str>) -> Option<String> {
        let running = self.running.remove(&id)?;
        if let (Some(error), Some(retry)) = (error, running.retry)
            && retry.refused_by(error)
            && let Some(stronger) = retry.stronger()
        {
            self.force_retry = Some(stronger);
        }
        Some(running.label)
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.running.is_empty()
    }

    pub(crate) fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// Marks the ref the history jumped to, replacing the previous highlight.
    pub(crate) fn reveal_ref(&mut self, kind: RefKind, short_name: &str) {
        self.selected_extra = None;
        self.reveal.reveal_ref(kind, short_name);
    }

    /// Like [`Self::reveal_ref`] for `stash@{index}`.
    pub(crate) fn reveal_stash(&mut self, index: usize) {
        self.selected_extra = None;
        self.reveal.reveal_stash(index);
    }

    /// Drops the reveal highlight, e.g. once a commit row is selected.
    pub(crate) fn clear_revealed(&mut self) {
        self.reveal.clear();
    }

    pub(crate) fn select_extra(&mut self, selection: ExtraSelection) {
        self.reveal.clear();
        self.selected_extra = Some(selection);
    }

    pub(crate) fn selected_extra(&self) -> Option<&ExtraSelection> {
        self.selected_extra.as_ref()
    }

    pub(crate) fn clear_selected_extra(&mut self) {
        self.selected_extra = None;
    }

    /// The ref history is limited to, as its short name, while "show only" is
    /// active. `None` means the whole history.
    pub(crate) fn scope_name(&self) -> Option<String> {
        self.scope.clone()
    }

    pub(crate) fn set_scope(&mut self, name: Option<String>) {
        self.set_filter(name.clone());
        self.scope = name;
    }

    /// Limits the sidebar list to refs matching `filter`, without scoping the
    /// history. `None` lists every ref again.
    pub(crate) fn set_filter(&mut self, filter: Option<String>) {
        self.filter = filter;
    }

    pub(crate) fn toggle(&mut self, index: usize) {
        if let Some(collapsed) = self.collapsed.get_mut(index) {
            *collapsed = !*collapsed;
            self.local_revision = self.local_revision.wrapping_add(1);
        }
    }

    pub(crate) fn toggle_remote_group(&mut self, group: String) {
        let collapsed = self.remote_groups.entry(group).or_insert(false);
        *collapsed = !*collapsed;
        self.local_revision = self.local_revision.wrapping_add(1);
    }

    /// Folds or unfolds one folder. The fold is remembered for this view, so it
    /// survives a snapshot refresh, the way a section's fold does.
    pub(crate) fn toggle_folder(&mut self, key: String) {
        let mut folded = self.folders.borrow_mut();
        if !folded.remove(&key) {
            folded.insert(key.clone());
        }
        drop(folded);
        *self.folder_focus.borrow_mut() = Some(key);
        self.local_revision = self.local_revision.wrapping_add(1);
    }

    /// Whether `key` is folded right now.
    fn folder_folded(&self, key: &str) -> bool {
        self.folders.borrow().contains(key)
    }

    /// Folds or unfolds `key` from the arrow keys: `open` picks the direction,
    /// and `None` means the key does not apply to the current state.
    fn set_folder_open(&mut self, key: &str, open: bool) -> bool {
        let mut folded = self.folders.borrow_mut();
        let changed = if open {
            folded.remove(key)
        } else {
            folded.insert(key.to_owned())
        };
        drop(folded);
        if changed {
            self.local_revision = self.local_revision.wrapping_add(1);
        }
        changed
    }

    /// Remembers the folder the arrow keys act on.
    pub(crate) fn focus_folder(&self, key: String) {
        *self.folder_focus.borrow_mut() = Some(key);
    }

    /// The folder the left/right arrows fold or unfold.
    pub(crate) fn focused_folder(&self) -> Option<String> {
        self.folder_focus.borrow().clone()
    }

    pub(crate) fn set_viewport(&mut self, viewport: Size<Pixels>) {
        self.viewport = viewport;
    }

    pub(crate) fn set_list_focus(&mut self, focus: FocusHandle) {
        self.list_focus = Some(focus);
    }

    pub(crate) fn focus_list(&self, window: &mut Window, cx: &mut App) {
        if let Some(focus) = &self.list_focus {
            window.focus(focus, cx);
        }
    }

    pub(crate) fn set_dialog_focus(&mut self, focus: FocusHandle) {
        self.dialog_focus = Some(focus);
    }

    pub(crate) fn open_create(&mut self, input: Entity<InputState>, start: BranchStart) {
        self.dialog = Some(Dialog {
            kind: DialogKind::Create(CreateForm {
                input,
                start,
                checkout: true,
                error: None,
            }),
            viewport: self.viewport,
            focus: self.dialog_focus.clone(),
        });
    }

    pub(crate) fn close_dialog(&mut self) {
        self.dialog = None;
    }

    pub(crate) fn open_rename(
        &mut self,
        input: Entity<InputState>,
        branch: String,
        upstream: Option<RemoteBranch>,
        upstream_label: Option<String>,
    ) {
        self.dialog = Some(Dialog {
            kind: DialogKind::Rename(RenameForm {
                input,
                branch,
                upstream,
                upstream_label,
                // Renaming the branch on its remote is a push and a delete
                // there, so it is an explicit second step, never the default.
                rename_remote: false,
                error: None,
            }),
            viewport: self.viewport,
            focus: self.dialog_focus.clone(),
        });
    }

    pub(crate) fn open_upstream(
        &mut self,
        branch: String,
        choices: Vec<UpstreamChoice>,
        current: Option<String>,
    ) {
        let selected = current
            .as_ref()
            .and_then(|current| choices.iter().position(|choice| &choice.label == current))
            .or_else(|| (!choices.is_empty()).then_some(0));
        self.dialog = Some(Dialog {
            kind: DialogKind::Upstream(UpstreamForm {
                branch,
                choices,
                selected,
                current,
                error: None,
            }),
            viewport: self.viewport,
            focus: self.dialog_focus.clone(),
        });
    }

    pub(crate) fn open_add_remote(&mut self, name: Entity<InputState>, url: Entity<InputState>) {
        self.dialog = Some(Dialog {
            kind: DialogKind::AddRemote(AddRemoteForm {
                name,
                url,
                fetch: true,
                error: None,
            }),
            viewport: self.viewport,
            focus: self.dialog_focus.clone(),
        });
    }

    pub(crate) fn open_rename_remote(&mut self, input: Entity<InputState>, name: String) {
        self.dialog = Some(Dialog {
            kind: DialogKind::RenameRemote(RenameRemoteForm {
                input,
                name,
                error: None,
            }),
            viewport: self.viewport,
            focus: self.dialog_focus.clone(),
        });
    }

    pub(crate) fn open_edit_remote(
        &mut self,
        name: String,
        fetch: Entity<InputState>,
        push: Option<Entity<InputState>>,
    ) {
        self.dialog = Some(Dialog {
            kind: DialogKind::EditRemote(EditRemoteForm {
                name,
                fetch,
                push,
                error: None,
            }),
            viewport: self.viewport,
            focus: self.dialog_focus.clone(),
        });
    }

    pub(crate) fn set_checkout(&mut self, checkout: bool) {
        if let Some(Dialog {
            kind: DialogKind::Create(form),
            ..
        }) = &mut self.dialog
        {
            form.checkout = checkout;
        }
    }

    pub(crate) fn set_rename_remote(&mut self, rename_remote: bool) {
        if let Some(Dialog {
            kind: DialogKind::Rename(form),
            ..
        }) = &mut self.dialog
        {
            form.rename_remote = rename_remote;
        }
    }

    pub(crate) fn set_fetch_now(&mut self, fetch: bool) {
        if let Some(Dialog {
            kind: DialogKind::AddRemote(form),
            ..
        }) = &mut self.dialog
        {
            form.fetch = fetch;
        }
    }

    /// Picks the remote branch the set-upstream dialog would set.
    pub(crate) fn set_upstream_choice(&mut self, index: usize) {
        if let Some(Dialog {
            kind: DialogKind::Upstream(form),
            ..
        }) = &mut self.dialog
            && index < form.choices.len()
        {
            form.selected = Some(index);
            form.error = None;
        }
    }

    pub(crate) fn set_dialog_error(&mut self, message: String) {
        match self.dialog.as_mut().map(|dialog| &mut dialog.kind) {
            Some(DialogKind::Create(form)) => form.error = Some(message),
            Some(DialogKind::Rename(form)) => form.error = Some(message),
            Some(DialogKind::Upstream(form)) => form.error = Some(message),
            Some(DialogKind::AddRemote(form)) => form.error = Some(message),
            Some(DialogKind::RenameRemote(form)) => form.error = Some(message),
            Some(DialogKind::EditRemote(form)) => form.error = Some(message),
            None => {}
        }
    }

    /// The create dialog's input, validated. `None` when it is not open.
    pub(crate) fn create_request(&self, cx: &App) -> Option<Result<CreateRequest, String>> {
        let form = match self.dialog.as_ref()? {
            Dialog {
                kind: DialogKind::Create(form),
                ..
            } => form,
            _ => return None,
        };
        let name = form.input.read(cx).value().to_string();
        Some(match validate_branch_name(&name) {
            Ok(()) => Ok(CreateRequest {
                name,
                start_point: form.start.point.clone(),
                checkout: form.checkout,
            }),
            Err(message) => Err(message.to_owned()),
        })
    }

    /// Whatever the open dialog submits, validated. `None` when no dialog is
    /// open, so the caller can tell "nothing to submit" from "invalid".
    pub(crate) fn dialog_request(&self, cx: &App) -> Option<Result<DialogRequest, String>> {
        let dialog = self.dialog.as_ref()?;
        Some(match &dialog.kind {
            DialogKind::Create(_) => self.create_request(cx)?.map(DialogRequest::Create),
            DialogKind::Rename(form) => {
                let typed = form.input.read(cx).value().to_string();
                RenameRequest::parse(
                    &form.branch,
                    &typed,
                    form.upstream.as_ref(),
                    form.rename_remote,
                )
                .map(DialogRequest::Rename)
            }
            DialogKind::Upstream(form) => match form.selected.and_then(|index| {
                form.choices
                    .get(index)
                    .map(|choice| (choice.remote.clone(), choice.remote_branch.clone()))
            }) {
                Some((remote, remote_branch)) => Ok(DialogRequest::SetUpstream {
                    branch: form.branch.clone(),
                    remote,
                    remote_branch,
                }),
                None => Err(t!("branch.placeholder.fetch_first").to_string().to_owned()),
            },
            DialogKind::AddRemote(form) => {
                let name = form.name.read(cx).value().to_string();
                let url = form.url.read(cx).value().to_string();
                AddRemoteRequest::parse(&name, &url, form.fetch).map(DialogRequest::AddRemote)
            }
            DialogKind::RenameRemote(form) => {
                let typed = form.input.read(cx).value().to_string();
                let new_name = typed.trim();
                if new_name == form.name {
                    return Some(Err(t!("branch.validation.remote_name_differs")
                        .to_string()
                        .to_owned()));
                }
                match validate_remote_name(new_name) {
                    Ok(()) => Ok(DialogRequest::RenameRemote {
                        name: form.name.clone(),
                        new_name: new_name.to_owned(),
                    }),
                    Err(message) => Err(message.to_owned()),
                }
            }
            DialogKind::EditRemote(form) => {
                let fetch = form.fetch.read(cx).value().to_string();
                let push = form
                    .push
                    .as_ref()
                    .map(|push| push.read(cx).value().to_string());
                EditRemoteUrlRequest::parse(&form.name, &fetch, push.as_deref())
                    .map(DialogRequest::EditRemoteUrl)
            }
        })
    }
}

/// The names in natural order, taking the list so the caller can move each
/// string straight into a list item.
fn natural_sorted_owned(mut names: Vec<String>) -> Vec<String> {
    names.sort_by(|a, b| natural_cmp(a, b));
    names
}

/// Whether a ref passes the sidebar's filter, which is the "show only" scope:
/// the whole name is searched, so a scope names the ref it limits history to.
fn matches_ref(filter: &str, name: &str) -> bool {
    name.to_lowercase().contains(&filter.trim().to_lowercase())
}

/// One local branch before it is placed in the folder tree.
struct LocalLeaf {
    name: String,
    index: usize,
}

/// One remote branch before it is placed in the folder tree.
struct RemoteLeaf {
    name: String,
    index: usize,
    local_name: Option<String>,
    existing: bool,
}

/// One tag before it is placed in the folder tree.
struct TagLeaf {
    name: String,
    index: usize,
}

/// Groups `(path, value)` entries into folders by path segment.
///
/// A folder around a single branch is not worth a row: it is replaced by a leaf
/// that keeps the whole path, so `t3/blame-view` reads as one line unless the
/// repository has another `t3/...` ref beside it.
fn ref_tree<T>(entries: Vec<(String, T)>) -> Vec<RefNode<T>> {
    grouped(entries, "")
}

fn grouped<T>(mut entries: Vec<(String, T)>, prefix: &str) -> Vec<RefNode<T>> {
    entries.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    let mut nodes = Vec::new();
    let mut entries = entries.into_iter().peekable();
    while let Some((path, value)) = entries.next() {
        let Some((segment, rest)) = path.split_once('/') else {
            nodes.push(RefNode::Leaf {
                label: path.clone(),
                path: format!("{prefix}{path}"),
                value,
            });
            continue;
        };
        let segment_prefix = format!("{segment}/");
        let mut children = vec![(rest.to_owned(), value)];
        while let Some((next, value)) =
            entries.next_if(|(next, _)| next.starts_with(&segment_prefix))
        {
            let rest = next
                .strip_prefix(&segment_prefix)
                .unwrap_or(&next)
                .to_owned();
            children.push((rest, value));
        }
        let mut children = grouped(children, &format!("{prefix}{segment}/"));
        // A folder that holds one branch is indistinguishable from that branch
        // at this level, so the leaf takes the path the folder would have split.
        if children.len() == 1 && matches!(children.first(), Some(RefNode::Leaf { .. })) {
            let Some(RefNode::Leaf { path, value, .. }) = children.pop() else {
                unreachable!("a one-child folder's child is a leaf");
            };
            let label = path.strip_prefix(prefix).unwrap_or(&path).to_owned();
            nodes.push(RefNode::Leaf { label, path, value });
            continue;
        }
        nodes.push(RefNode::Folder {
            path: format!("{prefix}{segment}"),
            segment: segment.to_owned(),
            children,
        });
    }
    nodes.sort_by(|a, b| match (a, b) {
        (RefNode::Leaf { label: a, .. }, RefNode::Leaf { label: b, .. }) => natural_cmp(a, b),
        (RefNode::Folder { path: a, .. }, RefNode::Folder { path: b, .. }) => natural_cmp(a, b),
        (RefNode::Leaf { label, .. }, RefNode::Folder { path, .. }) => natural_cmp(label, path),
        (RefNode::Folder { path, .. }, RefNode::Leaf { label, .. }) => natural_cmp(path, label),
    });
    nodes
}

/// Pushes the folder tree's rows: a folder row per branch-point, its leaves
/// below, indented a step further.
#[allow(clippy::too_many_arguments)]
fn push_ref_nodes<T>(
    items: &mut Vec<SidebarItem>,
    nodes: Vec<RefNode<T>>,
    depth: usize,
    section: Section,
    remote: Option<&str>,
    folded: &HashSet<String>,
    forced: &HashSet<String>,
    leaf: &mut impl FnMut(T, &str, usize) -> SidebarItem,
) {
    for node in nodes {
        match node {
            RefNode::Leaf { label, value, .. } => items.push(leaf(value, &label, depth)),
            RefNode::Folder {
                path,
                segment,
                children,
            } => {
                let key = folder_key(section, remote, &path);
                let open = !folded.contains(&key) || forced.contains(&key);
                let count = children.iter().map(RefNode::leaf_count).sum();
                items.push(SidebarItem::Folder {
                    key,
                    segment,
                    count,
                    open,
                    depth,
                    section,
                });
                if open {
                    push_ref_nodes(
                        items,
                        children,
                        depth + 1,
                        section,
                        remote,
                        folded,
                        forced,
                        leaf,
                    );
                }
            }
        }
    }
}

/// Builds the element for one visible list item.
#[allow(clippy::too_many_arguments)]
fn item_element(
    item: &SidebarItem,
    head: Option<&str>,
    reveal: &RevealState,
    selected_extra: Option<&ExtraSelection>,
    pull_requests: &BranchPullRequests,
    enabled: bool,
    busy: bool,
    hidden_refs: &[String],
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    match item {
        SidebarItem::SectionHeader {
            section,
            count,
            open,
        } => section_header(*section, *count, *open, busy, colors, entity),
        SidebarItem::Folder {
            key,
            segment,
            count,
            open,
            depth,
            section,
        } => folder_row(
            key, segment, *count, *open, *depth, *section, colors, entity,
        ),
        SidebarItem::RemoteGroup {
            name,
            fetch_url,
            count,
            collapsed,
        } => remote_group_header(
            name,
            fetch_url.as_deref(),
            *count,
            *collapsed,
            busy,
            colors,
            entity,
        ),
        SidebarItem::LocalBranch {
            name,
            label,
            index,
            depth,
            upstream,
            upstream_gone,
            upstream_note,
        } => local_row(
            name,
            label,
            *index,
            *depth,
            head == Some(name.as_str()),
            upstream.as_deref(),
            *upstream_gone,
            upstream_note.clone(),
            pull_requests.get(name),
            pull_requests.note(),
            reveal.is_ref(RefKind::LocalBranch, name),
            hidden_refs.contains(&ref_full_name(RefKind::LocalBranch, name)),
            enabled,
            busy,
            colors,
            entity,
        ),
        SidebarItem::RemoteBranch {
            name,
            label,
            index,
            depth,
            local_name,
            existing: _,
        } => remote_row(
            name,
            label,
            *index,
            *depth,
            local_name.as_deref(),
            reveal.is_ref(RefKind::RemoteBranch, name),
            hidden_refs.contains(&ref_full_name(RefKind::RemoteBranch, name)),
            enabled,
            busy,
            colors,
            entity,
        ),
        SidebarItem::Tag {
            name,
            label,
            index,
            depth,
        } => tag_row(
            name,
            label,
            *index,
            *depth,
            reveal.is_ref(RefKind::Tag, name),
            enabled,
            busy,
            colors,
            entity,
        ),
        // The stash row and its menu live in `stash`, which can read the
        // entry's age and source branch from the snapshot.
        SidebarItem::Stash { name, index } => crate::stash::sidebar_row(
            name,
            *index,
            reveal.is_stash(*index),
            enabled,
            busy,
            colors,
            entity,
        ),
        SidebarItem::Submodule {
            index,
            name,
            path,
            detail,
            status,
        } => {
            let selected =
                matches!(selected_extra, Some(ExtraSelection::Submodule(sel)) if sel == path);
            submodule_row(
                *index, name, path, detail, *status, selected, enabled, busy, colors, entity,
            )
        }
        SidebarItem::Worktree {
            index,
            label,
            path,
            detail,
            lock_reason,
            is_main,
            locked,
            prunable,
            current,
        } => {
            let selected = matches!(
                selected_extra,
                Some(ExtraSelection::Worktree(sel)) if same_path(sel, Path::new(path))
            );
            worktree_row(
                *index,
                label,
                path,
                detail,
                lock_reason.as_deref(),
                *is_main,
                *locked,
                *prunable,
                *current,
                selected,
                enabled,
                busy,
                colors,
                entity,
            )
        }
        SidebarItem::Placeholder { text, .. } => {
            empty_row_at(text, px(42.0), colors).into_any_element()
        }
    }
}

/// A section's heading: the fold control, the number of entries it holds, and
/// the section's own actions in a context menu. Folding is what a click means,
/// so a menu is what right-click can offer without a second control.
fn section_header(
    section: Section,
    count: usize,
    open: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let index = section.index();
    let click_entity = entity.clone();
    let mut header =
        crate::ui::section_header(("branch-section", index), section.title(), &colors.design)
            .count(count)
            .disclosure(open)
            .icon(style::glyph_icon(
                section.icon(),
                14.0,
                colors.muted_foreground,
            ))
            // A list item is only as tall as the heading measured, so every
            // section heading must match a row's height.
            .h(px(ITEM_HEIGHT))
            .pr(px(18.0))
            .on_click(move |_, cx| {
                click_entity.update(cx, |view, cx| {
                    view.branches.toggle(index);
                    cx.notify();
                });
            });
    if busy && section == Section::Local {
        header = header.trailing(
            div()
                .flex_none()
                .text_size(px(9.0))
                .font_weight(FontWeight::NORMAL)
                .child(t!("branch.working").to_string()),
        );
    }

    match section {
        Section::Remote | Section::Submodules | Section::Worktrees => {
            let menu_entity = entity.clone();
            crate::ui::context_menu(
                ("branch-section-menu", index),
                &colors.design,
                header,
                move |menu, _window, cx| section_menu(menu, section, busy, &menu_entity, cx),
            )
            .w_full()
            .into_any_element()
        }
        _ => header.into_any_element(),
    }
}

/// A folder built from the path segments several refs share. The row folds and
/// unfolds on click; the chevron and the folder glyph say which way it is.
#[allow(clippy::too_many_arguments)]
fn folder_row(
    key: &str,
    segment: &str,
    count: usize,
    open: bool,
    depth: usize,
    section: Section,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let click_key = key.to_owned();
    let hover_key = key.to_owned();
    let hover_entity = entity.clone();
    let base = if section == Section::Remote {
        42.0
    } else {
        0.0
    };
    crate::ui::list_row(
        ElementId::Name(format!("branch-folder-{key}").into()),
        crate::ui::RowState::NORMAL,
        &colors.design,
    )
    .h(px(ITEM_HEIGHT))
    .w_full()
    .min_w_0()
    .overflow_hidden()
    .mx(px(0.0))
    .pr(px(18.0))
    .pl(px(base + depth as f32 * 14.0))
    .rounded(px(7.0))
    .text_size(px(colors.design.small()))
    .text_color(colors.sidebar_foreground)
    .cursor_pointer()
    .child(
        Icon::new(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .with_size(px(12.0))
        .text_color(colors.muted_foreground),
    )
    .child(
        Icon::new(if open {
            IconName::FolderOpen
        } else {
            IconName::FolderClosed
        })
        .with_size(px(13.0))
        .text_color(colors.muted_foreground),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .child(segment.to_owned()),
    )
    .child(
        div()
            .flex_none()
            .text_size(px(colors.design.tiny()))
            .text_color(colors.muted_foreground)
            .child(count.to_string()),
    )
    .on_hover(move |hovered, _, cx| {
        if *hovered {
            hover_entity.update(cx, |view, _| view.branches.focus_folder(hover_key.clone()));
        }
    })
    .on_click(move |_, window, cx| {
        let key = click_key.clone();
        entity.update(cx, |view, cx| {
            view.branches.toggle_folder(key);
            view.branches.focus_list(window, cx);
            cx.notify();
        });
    })
    .into_any_element()
}

/// The chip that follows the pointer while a branch row is dragged.
struct BranchDragChip {
    branch: DraggedBranch,
    design: Design,
}

impl Render for BranchDragChip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("branch-drag-chip")
            .p(px(2.0))
            .rounded_full()
            .bg(self.design.panel)
            .border_1()
            .border_color(self.design.border)
            .child(crate::ui::ref_badge(
                self.branch.kind,
                false,
                self.branch.name.clone(),
                &self.design,
            ))
    }
}

/// Makes a local branch row the drop target of a branch drag. A branch onto
/// itself is not a target, so dropping it there does nothing and it gets no
/// highlight.
fn branch_drop_target(
    row: Stateful<Div>,
    target: &str,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> Stateful<Div> {
    branch_drop_target_with(row, target, colors.design.accent_soft, entity)
}

/// The shape a drop target takes outside a sidebar row, such as a branch chip
/// in the history list, where only the target's name and the accent are needed.
pub(crate) fn branch_drop_target_with(
    row: Stateful<Div>,
    target: &str,
    accent: Hsla,
    entity: Entity<RepoView>,
) -> Stateful<Div> {
    let can_target = target.to_owned();
    let highlight = target.to_owned();
    let drop_target = target.to_owned();
    row.can_drop(move |dragged, _, _| {
        dragged
            .downcast_ref::<DraggedBranch>()
            .is_some_and(|dragged| {
                !(dragged.kind == RefKind::LocalBranch && dragged.name == can_target)
            })
    })
    .drag_over::<DraggedBranch>(move |style, dragged, _, _| {
        if dragged.kind == RefKind::LocalBranch && dragged.name == highlight {
            style
        } else {
            style.bg(accent)
        }
    })
    .on_drop(move |dragged: &DraggedBranch, window, cx| {
        let target = drop_target.clone();
        let source = dragged.clone();
        let position = window.mouse_position();
        entity.update(cx, |view, cx| {
            view.open_branch_drop_menu(source, target, position, window, cx);
        });
    })
}

/// The menu entries a branch dropped on `target` offers, or `None` when the
/// drop means nothing: a local branch onto itself.
///
/// `fast_forward` is whether the remote source can fast-forward the target;
/// `None` means the tips could not be read.
pub(crate) fn drop_entries(
    source: &DraggedBranch,
    target: &str,
    head: Option<&str>,
    busy: bool,
    fast_forward: Option<bool>,
) -> Option<Vec<DropEntry>> {
    if source.kind == RefKind::LocalBranch && source.name == target {
        return None;
    }
    let checked_out = head == Some(target);
    let checkout_first = || t!("branch.drop.checkout_first", branch = target).to_string();
    let blocked = || {
        if busy {
            Some(t!("branch.reason.busy").to_string())
        } else if !checked_out {
            Some(checkout_first())
        } else {
            None
        }
    };
    let mut entries = vec![
        DropEntry {
            action: DropAction::Merge,
            label: t!("branch.drop.merge", source = source.name, target = target).to_string(),
            blocked: blocked(),
        },
        DropEntry {
            action: DropAction::Rebase,
            label: t!("branch.drop.rebase", target = target, source = source.name).to_string(),
            blocked: blocked(),
        },
    ];
    if source.kind == RefKind::RemoteBranch && local_name_for_remote(&source.name) == Some(target) {
        let blocked = if busy {
            Some(t!("branch.reason.busy").to_string())
        } else if !checked_out {
            Some(checkout_first())
        } else {
            match fast_forward {
                Some(true) => None,
                Some(false) => Some(t!("branch.drop.not_fast_forward").to_string()),
                None => Some(t!("branch.drop.cannot_read").to_string()),
            }
        };
        entries.push(DropEntry {
            action: DropAction::FastForward,
            label: t!("branch.drop.fast_forward", target = target).to_string(),
            blocked,
        });
    }
    Some(entries)
}

/// The section-level actions of the two lists git stores outside the refs.
fn section_menu(
    menu: PopupMenu,
    section: Section,
    busy: bool,
    entity: &Entity<RepoView>,
    cx: &App,
) -> PopupMenu {
    match section {
        Section::Remote => {
            let add = {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| view.open_add_remote(window, cx));
                }
            };
            let blocked = busy.then_some(t!("branch.reason.busy").to_string());
            menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.add_remote").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click(add),
            )
        }
        Section::Submodules => {
            let update = {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| view.update_all_submodules(window, cx));
                }
            };
            let blocked = blocked_section(
                busy,
                !entity.read(cx).has_submodules(),
                t!("branch.empty.submodules").to_string(),
            );
            menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.update_all").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click(update),
            )
        }
        Section::Worktrees => {
            let add = {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| view.open_add_worktree(window, cx));
                }
            };
            let prune = {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| view.prune_worktrees(window, cx));
                }
            };
            let add_blocked = busy.then_some(t!("branch.reason.busy").to_string());
            let prune_blocked = blocked_section(
                busy,
                !entity.read(cx).has_prunable_worktree(),
                t!("branch.reason.no_missing_worktrees").to_string(),
            );
            menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.add_worktree").to_string(),
                    add_blocked.as_ref(),
                ))
                .disabled(add_blocked.is_some())
                .on_click(add),
            )
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.prune").to_string(),
                    prune_blocked.as_ref(),
                ))
                .disabled(prune_blocked.is_some())
                .on_click(prune),
            )
        }
        _ => menu,
    }
}

/// Why a section action is disabled: the list's own state says something
/// permanent, so it wins over a running operation.
fn blocked_section(busy: bool, empty: bool, empty_reason: impl Into<String>) -> Option<String> {
    if empty {
        Some(empty_reason.into())
    } else if busy {
        Some(t!("branch.reason.busy").to_string())
    } else {
        None
    }
}

/// A remote's own collapsing header, above its branch list.
#[allow(clippy::too_many_arguments)]
fn remote_group_header(
    name: &str,
    fetch_url: Option<&str>,
    count: usize,
    collapsed: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let click_group = name.to_owned();
    let click_entity = entity.clone();
    let mut header =
        crate::ui::section_header(format!("remote-group-{name}"), name, &colors.design)
            .disclosure(!collapsed)
            .icon(style::glyph_icon(
                style::glyph::CLOUD,
                14.0,
                colors.sidebar_foreground,
            ))
            .text_size(px(colors.design.small()))
            .text_color(colors.sidebar_foreground)
            .h(px(ITEM_HEIGHT))
            .on_click(move |_, cx| {
                let group = click_group.clone();
                click_entity.update(cx, |view, cx| {
                    view.branches.toggle_remote_group(group);
                    cx.notify();
                });
            });
    if count > 0 {
        header = header.count(count);
    }

    let menu_entity = entity.clone();
    let menu_name = name.to_owned();
    let header = crate::ui::context_menu(
        format!("remote-group-menu-{name}"),
        &colors.design,
        header,
        move |menu, _window, _cx| {
            let blocked = busy.then_some(t!("branch.reason.busy").to_string());
            let fetch = {
                let entity = menu_entity.clone();
                let name = menu_name.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let name = name.clone();
                    entity.update(cx, |view, cx| view.fetch_remote(name, window, cx));
                }
            };
            let rename = {
                let entity = menu_entity.clone();
                let name = menu_name.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let name = name.clone();
                    entity.update(cx, |view, cx| view.open_rename_remote(name, window, cx));
                }
            };
            let edit_url = {
                let entity = menu_entity.clone();
                let name = menu_name.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let name = name.clone();
                    entity.update(cx, |view, cx| view.open_edit_remote(name, window, cx));
                }
            };
            let remove = {
                let entity = menu_entity.clone();
                let name = menu_name.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let name = name.clone();
                    entity.update(cx, |view, cx| view.confirm_remove_remote(name, window, cx));
                }
            };
            menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.fetch").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click(fetch),
            )
            .separator()
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.rename").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click(rename),
            )
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.edit_url").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click(edit_url),
            )
            .separator()
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.remove").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click(remove),
            )
        },
    )
    .w_full();

    match fetch_url {
        Some(url) => {
            let url = url.to_owned();
            // The header's own elements consume the click, so the tooltip
            // goes on a wrapper that is as wide as the row.
            div()
                .id(format!("remote-group-url-{name}"))
                .w_full()
                .row_tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(url.clone()).build(window, cx)
                })
                .child(header)
                .into_any_element()
        }
        None => header.into_any_element(),
    }
}

#[allow(clippy::too_many_arguments)]
fn local_row(
    name: &str,
    label: &str,
    index: usize,
    depth: usize,
    active: bool,
    upstream: Option<&str>,
    upstream_gone: bool,
    upstream_note: Option<String>,
    pull_request: Option<&strata_core::forge::PullRequestSummary>,
    pull_request_note: Option<&str>,
    revealed: bool,
    hidden: bool,
    enabled: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let mut row = row_with_icon(
        ("local-branch", index),
        Some(style::glyph::BRANCH),
        label,
        crate::ui::RowState::NORMAL.head(active).revealed(revealed),
        colors,
    )
    .pl(px(depth as f32 * 14.0))
    .cursor_pointer();
    if upstream_gone {
        let mut marker = div()
            .id(("local-branch-upstream-gone", index))
            .flex_none()
            .flex()
            .items_center()
            .child(
                Icon::new(IconName::Unlink)
                    .with_size(px(12.0))
                    .text_color(colors.design.tag_text),
            );
        if let Some(note) = upstream_note.clone() {
            marker =
                marker.row_tooltip(move |window, cx| Tooltip::new(note.clone()).build(window, cx));
        }
        row = row.child(marker);
    } else if let Some(label) = upstream {
        let mut label = div()
            .id(("local-branch-upstream", index))
            .flex_none()
            .text_size(px(colors.design.tiny()))
            .text_color(colors.muted_foreground)
            .child(label.to_owned());
        if let Some(note) = upstream_note.clone() {
            label =
                label.row_tooltip(move |window, cx| Tooltip::new(note.clone()).build(window, cx));
        }
        row = row.child(label);
    }

    if hidden {
        // The row still lists the branch, but the graph no longer walks it:
        // the closed eye is the same icon the menu's "Hide in history" uses.
        row = row.child(
            Icon::new(IconName::EyeOff)
                .with_size(px(12.0))
                .text_color(colors.muted_foreground),
        );
    }

    if let Some(pull_request) = pull_request {
        row = row.child(crate::pull_requests::badge(
            pull_request,
            pull_request_note,
            &colors.design,
        ));
    }

    if !enabled {
        return row.into_any_element();
    }

    // One click shows the branch in the graph; a second one checks it out.
    let click_entity = entity.clone();
    let branch = name.to_owned();
    let drag_design = colors.design.clone();
    row = row
        .on_click(move |event, window, cx| {
            let branch = branch.clone();
            click_entity.update(cx, |view, cx| {
                if event.click_count() < 2 {
                    view.reveal_ref(RefKind::LocalBranch, &branch, window, cx);
                } else if !busy && !active {
                    view.checkout_branch(RefKind::LocalBranch, &branch, window, cx);
                }
            });
        })
        .on_drag(
            DraggedBranch {
                kind: RefKind::LocalBranch,
                name: name.to_owned(),
            },
            move |dragged, _, _, cx| {
                let design = drag_design.clone();
                cx.new(|_| BranchDragChip {
                    branch: dragged.clone(),
                    design,
                })
            },
        );
    row = branch_drop_target(row, name, colors, entity.clone());

    let entity_menu = entity.clone();
    let branch = name.to_owned();
    crate::ui::context_menu(
        ("local-branch-menu", index),
        &colors.design,
        row,
        move |menu, window, cx| {
            // A disabled entry says why, the way the commit menu's entries do.
            let mut variables = entity_menu.read(cx).custom_variables();
            // `${branch}` is the branch this menu acts on, which need not be
            // the checked-out one.
            variables.branch = Some(branch.clone());
            let checkout_blocked =
                branch_blocked(active, t!("branch.reason.checked_out").to_string(), busy);
            let merge_blocked =
                branch_blocked(active, t!("branch.reason.current").to_string(), busy);
            let create_blocked = busy.then_some(t!("branch.reason.busy").to_string());
            let delete_blocked =
                branch_blocked(active, t!("branch.reason.current").to_string(), busy);
            let rebase = entity_menu
                .read(cx)
                .branch_rebase_target(RefKind::LocalBranch, &branch);
            let rename_blocked = busy.then_some(t!("branch.reason.busy").to_string());
            let view = entity_menu.read(cx);
            let upstream = view.branch_upstream(&branch);
            let has_upstream = upstream.is_some();
            let has_remote_branches = !view.upstream_choices().is_empty();
            let set_blocked = if !has_remote_branches {
                Some(t!("branch.empty.no_remote_branches").to_string())
            } else {
                busy.then_some(t!("branch.reason.busy").to_string())
            };
            let unset_blocked = if !has_upstream {
                Some(t!("branch.reason.no_upstream").to_string())
            } else {
                busy.then_some(t!("branch.reason.busy").to_string())
            };
            let head = view.compare_head();
            let compare_blocked = crate::repo_view::compare::with_current_branch_blocked(
                view.session.is_some(),
                active,
                busy,
                head,
            );
            let compare = {
                let entity = entity_menu.clone();
                let branch = branch.clone();
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.compare").to_string(),
                    compare_blocked.as_ref(),
                ))
                .disabled(compare_blocked.is_some())
                .on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let branch = branch.clone();
                        entity.update(cx, |view, cx| {
                            view.open_compare_with_current_branch(&branch, window, cx)
                        });
                    },
                )
            };
            let menu = menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.checkout").to_string(),
                    checkout_blocked.as_ref(),
                ))
                .disabled(checkout_blocked.is_some())
                .on_click({
                    let entity = entity_menu.clone();
                    let branch = branch.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let branch = branch.clone();
                        entity.update(cx, |view, cx| {
                            view.checkout_branch(RefKind::LocalBranch, &branch, window, cx);
                        });
                    }
                }),
            );
            let menu = crate::repo_view::history_filter_menu(
                menu,
                RefKind::LocalBranch,
                &branch,
                active,
                &entity_menu,
                cx,
            );
            let menu = menu
                .separator()
                .item(compare)
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.merge_into_current").to_string(),
                        merge_blocked.as_ref(),
                    ))
                    .disabled(merge_blocked.is_some())
                    .on_click({
                        let entity = entity_menu.clone();
                        let branch = branch.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let branch = branch.clone();
                            entity.update(cx, |view, cx| {
                                view.merge_branch(&branch, window, cx);
                            });
                        }
                    }),
                )
                // One entry rather than a submenu, so the actions around it stay
                // in the same list.
                .when_some(rebase, |menu, (current, target)| {
                    menu.item(rebase_menu_item(
                        target,
                        &current,
                        Some(&branch),
                        None,
                        entity_menu.clone(),
                    ))
                })
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.create_from_here").to_string(),
                        create_blocked.as_ref(),
                    ))
                    .disabled(create_blocked.is_some())
                    .on_click({
                        let entity = entity_menu.clone();
                        let point = branch.clone();
                        let from = t!("branch.op.from", branch = branch).to_string();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let point = point.clone();
                            let from = from.clone();
                            entity.update(cx, |view, cx| {
                                view.open_create_dialog(Some(point), from, window, cx);
                            });
                        }
                    }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.rename").to_string(),
                        rename_blocked.as_ref(),
                    ))
                    .disabled(rename_blocked.is_some())
                    .on_click({
                        let entity = entity_menu.clone();
                        let branch = branch.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let branch = branch.clone();
                            entity.update(cx, |view, cx| {
                                view.rename_branch(&branch, window, cx);
                            });
                        }
                    }),
                )
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.set_upstream").to_string(),
                        set_blocked.as_ref(),
                    ))
                    .disabled(set_blocked.is_some())
                    .on_click({
                        let entity = entity_menu.clone();
                        let branch = branch.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let branch = branch.clone();
                            entity.update(cx, |view, cx| {
                                view.open_set_upstream(branch, window, cx);
                            });
                        }
                    }),
                )
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.unset_upstream").to_string(),
                        unset_blocked.as_ref(),
                    ))
                    .disabled(unset_blocked.is_some())
                    .on_click({
                        let entity = entity_menu.clone();
                        let branch = branch.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let branch = branch.clone();
                            entity.update(cx, |view, cx| {
                                view.unset_branch_upstream(branch, window, cx);
                            });
                        }
                    }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.delete").to_string(),
                        delete_blocked.as_ref(),
                    ))
                    .disabled(delete_blocked.is_some())
                    .on_click({
                        let entity = entity_menu.clone();
                        let branch = branch.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let branch = branch.clone();
                            entity.update(cx, |view, cx| {
                                view.delete_branch_here(&branch, window, cx);
                            });
                        }
                    }),
                )
                .separator();
            crate::commands::submenu(
                menu,
                crate::commands::Context::Branch,
                &variables,
                window,
                cx,
            )
            .when_some(
                crate::github::branch_items(&entity_menu, &branch, cx),
                |menu, items| {
                    menu.separator()
                        .item(items.open)
                        .item(items.create_pr)
                        .when_some(items.open_pr, |menu, item| menu.item(item))
                        .item(items.checkout_pr)
                },
            )
        },
    )
    .w_full()
    .into_any_element()
}

/// Why a branch action is disabled: the branch's own state is permanent, so it
/// wins over an operation that is only running for now.
fn branch_blocked(active: bool, active_reason: impl Into<String>, busy: bool) -> Option<String> {
    if active {
        Some(active_reason.into())
    } else if busy {
        Some(t!("branch.reason.busy").to_string())
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn remote_row(
    name: &str,
    label: &str,
    index: usize,
    depth: usize,
    local_name: Option<&str>,
    revealed: bool,
    hidden: bool,
    enabled: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let mut row_element = row(
        ("remote-branch", index),
        label,
        crate::ui::RowState::NORMAL.revealed(revealed),
        colors,
    )
    .pl(px(42.0 + depth as f32 * 14.0));
    if hidden {
        row_element = row_element.child(
            Icon::new(IconName::EyeOff)
                .with_size(px(12.0))
                .text_color(colors.muted_foreground),
        );
    }
    // A remote ref without a local counterpart (the symbolic `origin/HEAD`)
    // has nothing to check out or merge.
    if local_name.is_none() {
        return row_element.into_any_element();
    }
    if !enabled {
        return row_element.into_any_element();
    }

    let drag_design = colors.design.clone();
    let row_element = row_element.on_drag(
        DraggedBranch {
            kind: RefKind::RemoteBranch,
            name: name.to_owned(),
        },
        move |dragged, _, _, cx| {
            let design = drag_design.clone();
            cx.new(|_| BranchDragChip {
                branch: dragged.clone(),
                design,
            })
        },
    );

    // Double-clicking `origin/foo` switches to `foo` when it exists and
    // creates it from the remote ref otherwise.
    let row_element = {
        let click_entity = entity.clone();
        let remote = name.to_owned();
        row_element
            .cursor_pointer()
            .on_click(move |event, window, cx| {
                let remote = remote.clone();
                click_entity.update(cx, |view, cx| {
                    if event.click_count() < 2 {
                        view.reveal_ref(RefKind::RemoteBranch, &remote, window, cx);
                    } else if !busy {
                        view.checkout_branch(RefKind::RemoteBranch, &remote, window, cx)
                    }
                });
            })
    };

    let entity_menu = entity.clone();
    let remote = name.to_owned();
    crate::ui::context_menu(
        ("remote-branch-menu", index),
        &colors.design,
        row_element,
        move |menu, _window, cx| {
            let blocked = busy.then_some(t!("branch.reason.busy").to_string());
            let rebase = entity_menu
                .read(cx)
                .branch_rebase_target(RefKind::RemoteBranch, &remote);
            let menu = menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.checkout").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click({
                    let entity = entity_menu.clone();
                    let remote = remote.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let remote = remote.clone();
                        entity.update(cx, |view, cx| {
                            view.checkout_branch(RefKind::RemoteBranch, &remote, window, cx)
                        });
                    }
                }),
            );
            let menu = crate::repo_view::history_filter_menu(
                menu,
                RefKind::RemoteBranch,
                &remote,
                false,
                &entity_menu,
                cx,
            );
            menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.merge_into_current").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click({
                    let entity = entity_menu.clone();
                    let remote = remote.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let remote = remote.clone();
                        entity.update(cx, |view, cx| {
                            view.merge_branch(&remote, window, cx);
                        });
                    }
                }),
            )
            .when_some(rebase, |menu, (current, target)| {
                menu.item(rebase_menu_item(
                    target,
                    &current,
                    Some(&remote),
                    None,
                    entity_menu.clone(),
                ))
            })
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.create_from_here").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click({
                    let entity = entity_menu.clone();
                    let remote = remote.clone();
                    let from = t!("branch.op.from", branch = remote).to_string();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let remote = remote.clone();
                        let from = from.clone();
                        entity.update(cx, |view, cx| {
                            view.open_create_dialog(Some(remote), from, window, cx);
                        });
                    }
                }),
            )
            .when_some(
                crate::github::remote_branch_item(&entity_menu, &remote, cx),
                |menu, item| menu.separator().item(item),
            )
        },
    )
    .w_full()
    .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn tag_row(
    name: &str,
    label: &str,
    index: usize,
    depth: usize,
    revealed: bool,
    enabled: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let tag = name.to_owned();
    let menu_entity = entity.clone();
    let menu_tag = tag.clone();
    let row_element = row_with_icon(
        ("tag", index),
        Some(style::glyph::TAG),
        label,
        crate::ui::RowState::NORMAL.revealed(revealed),
        colors,
    )
    .pl(px(depth as f32 * 14.0))
    .cursor_pointer()
    .on_click(move |_, window, cx| {
        entity.update(cx, |view, cx| {
            view.reveal_ref(RefKind::Tag, &tag, window, cx)
        });
    });
    if !enabled {
        return row_element.into_any_element();
    }
    crate::ui::context_menu(
        ("tag-menu", index),
        &colors.design,
        row_element,
        move |menu, _window, _cx| {
            let blocked = busy.then_some(t!("branch.reason.busy").to_string());
            menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.show_only_tag").to_string(),
                    blocked.as_ref(),
                ))
                .disabled(blocked.is_some())
                .on_click({
                    let entity = menu_entity.clone();
                    let tag = menu_tag.clone();
                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                        let tag = tag.clone();
                        entity.update(cx, |view, cx| view.show_only_ref(RefKind::Tag, &tag, cx));
                    }
                }),
            )
        },
    )
    .w_full()
    .into_any_element()
}

/// A submodule's row: its name, a badge for anything but an up-to-date
/// checkout, and its path and commits in the tooltip.
#[allow(clippy::too_many_arguments)]
fn submodule_row(
    index: usize,
    name: &str,
    path: &str,
    detail: &str,
    status: SubmoduleStatus,
    selected: bool,
    enabled: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let tooltip_text = detail.to_owned();
    let mut row_element = crate::ui::list_row(
        ("submodule", index),
        crate::ui::RowState::NORMAL.selected(selected),
        &colors.design,
    )
    .h(px(ITEM_HEIGHT))
    .w_full()
    .min_w_0()
    .overflow_hidden()
    .mx(px(0.0))
    .pr(px(18.0))
    .rounded(px(7.0))
    .text_size(px(colors.design.small()))
    .text_color(colors.sidebar_foreground)
    .child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
    .row_tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx));
    if let Some((label, tone)) = submodule_badge(status) {
        row_element = row_element.child(crate::ui::status_badge(label, tone, &colors.design));
    }

    // An uninitialised submodule has no checkout to open; its row only offers
    // the context menu that initialises it.
    if status != SubmoduleStatus::Uninitialised {
        let click_entity = entity.clone();
        let click_path = path.to_owned();
        row_element = row_element
            .cursor_pointer()
            .on_click(move |event, window, cx| {
                let path = click_path.clone();
                click_entity.update(cx, |view, cx| {
                    if event.click_count() < 2 {
                        view.branches.select_extra(ExtraSelection::Submodule(path));
                        view.branches.focus_list(window, cx);
                        cx.notify();
                    } else {
                        view.open_repository_in_window(Path::new(&path), window, cx);
                    }
                });
            });
    }

    if !enabled {
        return row_element.into_any_element();
    }

    let menu_entity = entity.clone();
    let menu_path = path.to_owned();
    crate::ui::context_menu(
        ("submodule-menu", index),
        &colors.design,
        row_element,
        move |menu, _window, _cx| {
            let open_blocked = (status == SubmoduleStatus::Uninitialised)
                .then_some(t!("branch.submodule.not_initialised").to_string());
            let update_blocked = if status == SubmoduleStatus::Uninitialised {
                Some(t!("branch.submodule.not_initialised").to_string())
            } else {
                busy.then_some(t!("branch.reason.busy").to_string())
            };
            let init_blocked = busy.then_some(t!("branch.reason.busy").to_string());
            let open = {
                let entity = menu_entity.clone();
                let path = menu_path.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| {
                        view.open_repository_in_window(Path::new(&path), window, cx)
                    });
                }
            };
            let open_new_tab = {
                let entity = menu_entity.clone();
                let path = menu_path.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| {
                        view.open_repository_in_new_tab(Path::new(&path), window, cx)
                    });
                }
            };
            let open_new_window = {
                let entity = menu_entity.clone();
                let path = menu_path.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| {
                        view.open_repository_in_new_window(Path::new(&path), window, cx)
                    });
                }
            };
            let update = {
                let entity = menu_entity.clone();
                let path = menu_path.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let path = path.clone();
                    entity.update(cx, |view, cx| {
                        view.update_submodule(path, false, window, cx)
                    });
                }
            };
            let init = {
                let entity = menu_entity.clone();
                let path = menu_path.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let path = path.clone();
                    entity.update(cx, |view, cx| view.update_submodule(path, true, window, cx));
                }
            };
            let menu = menu
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.open").to_string(),
                        open_blocked.as_ref(),
                    ))
                    .disabled(open_blocked.is_some())
                    .on_click(open),
                )
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.open_new_tab").to_string(),
                        open_blocked.as_ref(),
                    ))
                    .disabled(open_blocked.is_some())
                    .on_click(open_new_tab),
                )
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.open_new_window").to_string(),
                        open_blocked.as_ref(),
                    ))
                    .disabled(open_blocked.is_some())
                    .on_click(open_new_window),
                )
                .separator()
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.update").to_string(),
                        update_blocked.as_ref(),
                    ))
                    .disabled(update_blocked.is_some())
                    .on_click(update),
                );
            let menu = if status == SubmoduleStatus::Uninitialised {
                menu.item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("branch.menu.initialise").to_string(),
                        init_blocked.as_ref(),
                    ))
                    .disabled(init_blocked.is_some())
                    .on_click(init),
                )
            } else {
                menu
            };
            let copy_path = menu_path.clone();
            menu.separator().item(
                PopupMenuItem::new(t!("branch.menu.copy_path").to_string()).on_click(
                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                    },
                ),
            )
        },
    )
    .w_full()
    .into_any_element()
}

/// A worktree's row: its branch (or the commit a detached one is on), its
/// badges, and its path and lock reason in the tooltip.
#[allow(clippy::too_many_arguments)]
fn worktree_row(
    index: usize,
    label: &str,
    path: &str,
    detail: &str,
    lock_reason: Option<&str>,
    is_main: bool,
    locked: bool,
    prunable: bool,
    current: bool,
    selected: bool,
    enabled: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let tooltip_text = detail.to_owned();
    let mut row_element = crate::ui::list_row(
        ("worktree", index),
        // The checkout the window is showing reads like the HEAD branch; the
        // badge says t!("branch.worktree.main").to_string() rather than the tint, because a linked worktree can
        // be the one on screen.
        crate::ui::RowState::NORMAL.head(current).selected(selected),
        &colors.design,
    )
    .h(px(ITEM_HEIGHT))
    .w_full()
    .min_w_0()
    .overflow_hidden()
    .mx(px(0.0))
    .pr(px(18.0))
    .rounded(px(7.0))
    .text_size(px(colors.design.small()))
    .text_color(colors.sidebar_foreground)
    .child(div().flex_1().min_w_0().truncate().child(label.to_owned()))
    .row_tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx));
    if is_main {
        row_element = row_element.child(crate::ui::status_badge(
            t!("branch.worktree.main").to_string(),
            crate::ui::BadgeTone::Accent,
            &colors.design,
        ));
    }
    if locked {
        let badge = crate::ui::status_badge(
            t!("branch.worktree.locked").to_string(),
            crate::ui::BadgeTone::Warning,
            &colors.design,
        );
        let reason = lock_reason.map(str::to_owned);
        let wrapper = div().id(("worktree-lock", index)).flex_none();
        row_element = row_element.child(match reason {
            Some(reason) => wrapper
                .row_tooltip(move |window, cx| Tooltip::new(reason.clone()).build(window, cx))
                .child(badge),
            None => wrapper.child(badge),
        });
    }
    if prunable {
        row_element = row_element.child(crate::ui::status_badge(
            t!("branch.worktree.missing").to_string(),
            crate::ui::BadgeTone::Danger,
            &colors.design,
        ));
    }

    // A prunable worktree has no checkout left; opening it would only fail.
    if !prunable {
        let click_entity = entity.clone();
        let click_path = path.to_owned();
        row_element = row_element
            .cursor_pointer()
            .on_click(move |event, window, cx| {
                let path = PathBuf::from(&click_path);
                click_entity.update(cx, |view, cx| {
                    if event.click_count() < 2 {
                        view.branches.select_extra(ExtraSelection::Worktree(path));
                        view.branches.focus_list(window, cx);
                        cx.notify();
                    } else if !current {
                        view.open_repository_in_window(&path, window, cx);
                    }
                });
            });
    }

    if !enabled {
        return row_element.into_any_element();
    }

    let menu_entity = entity.clone();
    let remove_path = PathBuf::from(path);
    let copy_path = path.to_owned();
    crate::ui::context_menu(
        ("worktree-menu", index),
        &colors.design,
        row_element,
        move |menu, _window, _cx| {
            let open_in_window_blocked = if prunable {
                Some(t!("branch.reason.missing_worktree").to_string())
            } else if current {
                Some(t!("branch.reason.already_open").to_string())
            } else {
                None
            };
            let open_new_window_blocked =
                prunable.then_some(t!("branch.reason.missing_worktree").to_string());
            let remove_blocked = if is_main {
                Some(t!("branch.reason.main_worktree").to_string())
            } else if locked {
                Some(t!("branch.reason.locked_worktree").to_string())
            } else if prunable {
                Some(t!("branch.reason.missing_worktree").to_string())
            } else {
                busy.then_some(t!("branch.reason.busy").to_string())
            };
            let open = {
                let entity = menu_entity.clone();
                let path = copy_path.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| {
                        view.open_repository_in_window(Path::new(&path), window, cx)
                    });
                }
            };
            let open_new_tab = {
                let entity = menu_entity.clone();
                let path = copy_path.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| {
                        view.open_repository_in_new_tab(Path::new(&path), window, cx)
                    });
                }
            };
            let open_new_window = {
                let entity = menu_entity.clone();
                let path = copy_path.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| {
                        view.open_repository_in_new_window(Path::new(&path), window, cx)
                    });
                }
            };
            let remove = {
                let entity = menu_entity.clone();
                let path = remove_path.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let path = path.clone();
                    entity.update(cx, |view, cx| {
                        view.confirm_remove_worktree(path, window, cx)
                    });
                }
            };
            let copy_path = copy_path.clone();
            menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.open").to_string(),
                    open_in_window_blocked.as_ref(),
                ))
                .disabled(open_in_window_blocked.is_some())
                .on_click(open),
            )
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.open_new_tab").to_string(),
                    open_in_window_blocked.as_ref(),
                ))
                .disabled(open_in_window_blocked.is_some())
                .on_click(open_new_tab),
            )
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.open_new_window").to_string(),
                    open_new_window_blocked.as_ref(),
                ))
                .disabled(open_new_window_blocked.is_some())
                .on_click(open_new_window),
            )
            .separator()
            .item(
                // The main worktree, a locked one and a missing checkout all
                // refuse `git worktree remove`; offering it would only
                // report the refusal.
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.remove").to_string(),
                    remove_blocked.as_ref(),
                ))
                .disabled(remove_blocked.is_some())
                .on_click(remove),
            )
            .separator()
            .item(
                PopupMenuItem::new(t!("branch.menu.copy_path").to_string()).on_click(
                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                    },
                ),
            )
        },
    )
    .w_full()
    .into_any_element()
}

/// The badge a submodule's state deserves, or nothing when it is up to date.
fn submodule_badge(status: SubmoduleStatus) -> Option<(String, crate::ui::BadgeTone)> {
    match status {
        SubmoduleStatus::Uninitialised => Some((
            t!("branch.submodule.not_initialised").to_string(),
            crate::ui::BadgeTone::Neutral,
        )),
        SubmoduleStatus::CommitDiffers => Some((
            t!("branch.worktree.changed").to_string(),
            crate::ui::BadgeTone::Accent,
        )),
        SubmoduleStatus::Modified => Some((
            t!("branch.worktree.modified").to_string(),
            crate::ui::BadgeTone::Warning,
        )),
        SubmoduleStatus::UpToDate => None,
    }
}

/// A submodule row's tooltip: where it lives, what it points at, and the two
/// commits whose difference is the row's state.
fn submodule_detail(submodule: &SubmoduleInfo) -> String {
    let mut detail = submodule.path.clone();
    if let Some(url) = &submodule.url {
        detail.push('\n');
        detail.push_str(url);
    }
    detail.push_str(&t!(
        "branch.detail.recorded",
        oid = commit_label(submodule.recorded_commit)
    ));
    detail.push_str(&t!(
        "branch.detail.checked_out",
        oid = commit_label(submodule.checked_out_commit)
    ));
    detail
}

fn commit_label(commit: Option<strata_core::Oid>) -> String {
    commit.map_or_else(
        || t!("branch.detail.none").to_string().to_owned(),
        crate::data::short_oid,
    )
}

/// A worktree row's label: the branch it has checked out, or the commit a
/// detached checkout is on.
fn worktree_label(worktree: &WorktreeInfo) -> String {
    match &worktree.head {
        Some(HeadInfo::Branch { short_name, .. }) => short_name.clone(),
        Some(HeadInfo::Detached { target }) => t!(
            "branch.detail.detached",
            oid = crate::data::short_oid(*target)
        )
        .to_string(),
        None => t!("branch.detail.unknown_head").to_string().to_owned(),
    }
}

/// A worktree row's tooltip: its path, and why it cannot be removed when git
/// says so.
fn worktree_detail(worktree: &WorktreeInfo) -> String {
    let mut detail = worktree.path.display().to_string();
    if worktree.locked {
        let line = match &worktree.lock_reason {
            Some(reason) => t!("branch.detail.locked_reason", reason = reason).to_string(),
            None => t!("branch.detail.locked").to_string(),
        };
        detail.push_str(&line);
    }
    if worktree.prunable {
        detail.push_str(&t!("branch.detail.missing_checkout"));
    }
    detail
}

/// Whether two paths name the same checkout.
///
/// The session's worktree and `git worktree list` can spell one directory
/// differently (symlinks, a trailing separator), so both sides are canonicalized
/// when they exist. A prunable worktree's path does not, and then git's own
/// spelling is the only one there is.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

impl RepoView {
    /// The upstream of a local branch, as the last snapshot read it.
    fn branch_upstream(&self, branch: &str) -> Option<UpstreamInfo> {
        match self.source()? {
            Source::Repo(snapshot) => snapshot.upstreams.get(branch).cloned(),
            Source::Fake(_) => None,
        }
    }

    /// One configured remote from the last snapshot.
    fn configured_remote(&self, name: &str) -> Option<RemoteInfo> {
        match self.source()? {
            Source::Repo(snapshot) => snapshot
                .remotes
                .iter()
                .find(|remote| remote.name == name)
                .cloned(),
            Source::Fake(_) => None,
        }
    }

    /// Every remote branch the set-upstream dialog can offer.
    fn upstream_choices(&self) -> Vec<UpstreamChoice> {
        match self.source() {
            Some(Source::Repo(snapshot)) => upstream_choices(&snapshot.refs, &snapshot.remotes),
            _ => Vec::new(),
        }
    }

    /// Focuses a dialog that has no text field to hand focus to.
    fn focus_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(focus) = self.branches.dialog_focus.clone() {
            window.defer(cx, move |window, cx| window.focus(&focus, cx));
        }
    }

    /// Checks out a branch: a local branch directly, a remote one as its local
    /// counterpart, created from the remote ref when it does not exist yet.
    ///
    /// The context menus and the sidebar's rows share this, so a branch is
    /// checked out the same way wherever it was clicked.
    pub(crate) fn checkout_branch(
        &mut self,
        kind: RefKind,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_busy() {
            return;
        }
        match kind {
            RefKind::LocalBranch => {
                let label = t!("branch.op.checkout", branch = name).to_string();
                self.run_operation(
                    Operation::Checkout {
                        branch: name.to_owned(),
                    },
                    label,
                    window,
                    cx,
                );
            }
            RefKind::RemoteBranch => {
                let Some(local) = local_name_for_remote(name) else {
                    return;
                };
                let existing = self.has_local_branch(local);
                checkout_remote(self, name, local, existing, window, cx);
            }
            RefKind::Tag => {}
        }
    }

    /// Merges `branch` into the checked-out one.
    pub(crate) fn merge_branch(
        &mut self,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("branch.op.merge", branch = branch).to_string();
        self.run_operation(
            Operation::Merge {
                branch: branch.to_owned(),
                no_ff: false,
            },
            label,
            window,
            cx,
        );
    }

    /// Renames a local branch through the same form the sidebar opens.
    pub(crate) fn rename_branch(
        &mut self,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_rename_branch(branch.to_owned(), window, cx);
    }

    /// Deletes a local branch through the same confirmation the sidebar opens.
    pub(crate) fn delete_branch_here(
        &mut self,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_delete_branch(branch.to_owned(), window, cx);
    }

    fn has_local_branch(&self, branch: &str) -> bool {
        self.session.as_ref().is_some_and(|session| {
            session
                .snapshot()
                .refs
                .iter()
                .any(|info| info.kind == RefKind::LocalBranch && info.short_name == branch)
        })
    }

    pub(crate) fn open_rename_branch(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        self.branches.set_viewport(window.viewport_size());
        let value = branch.clone();
        let input = cx.new(|cx| {
            let mut state =
                InputState::new(window, cx).placeholder(t!("branch.placeholder.name").to_string());
            state.set_value(value, window, cx);
            state
        });
        let handle = input.focus_handle(cx);
        // The remote side of the rename comes from the configured upstream,
        // not from the tracking ref: a remote branch deleted on the server
        // still has to be deletable here.
        let target = self
            .session
            .as_ref()
            .and_then(|session| {
                session
                    .repo()
                    .branch_upstream_target(&branch)
                    .ok()
                    .flatten()
            })
            .map(|target| RemoteBranch {
                remote: target.remote,
                branch: target.branch,
            });
        let label = self.branch_upstream(&branch).map(|upstream| upstream.name);
        self.branches.open_rename(input, branch, target, label);
        window.defer(cx, move |window, cx| handle.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn open_set_upstream(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        self.branches.set_viewport(window.viewport_size());
        let choices = self.upstream_choices();
        let current = self.branch_upstream(&branch).map(|upstream| upstream.name);
        self.branches.open_upstream(branch, choices, current);
        self.focus_dialog(window, cx);
        cx.notify();
    }

    pub(crate) fn open_add_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        self.branches.set_viewport(window.viewport_size());
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("origin"));
        let url = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("branch.placeholder.url").to_string())
        });
        let handle = name.focus_handle(cx);
        self.branches.open_add_remote(name, url);
        window.defer(cx, move |window, cx| handle.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn open_rename_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        self.branches.set_viewport(window.viewport_size());
        let value = name.clone();
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .placeholder(t!("branch.placeholder.remote_name").to_string());
            state.set_value(value, window, cx);
            state
        });
        let handle = input.focus_handle(cx);
        self.branches.open_rename_remote(input, name);
        window.defer(cx, move |window, cx| handle.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn open_edit_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(remote) = self.configured_remote(&name) else {
            return;
        };
        self.branches.set_viewport(window.viewport_size());
        let fetch_value = remote.fetch_url.clone().unwrap_or_default();
        let fetch = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .placeholder(t!("branch.placeholder.fetch_url").to_string());
            state.set_value(fetch_value, window, cx);
            state
        });
        let push = remote.push_url.clone().map(|value| {
            cx.new(|cx| {
                let mut state = InputState::new(window, cx)
                    .placeholder(t!("branch.placeholder.push_url").to_string());
                state.set_value(value, window, cx);
                state
            })
        });
        let handle = fetch.focus_handle(cx);
        self.branches.open_edit_remote(name, fetch, push);
        window.defer(cx, move |window, cx| handle.focus(window, cx));
        cx.notify();
    }

    /// Asks before removing a remote: its remote-tracking branches go with it.
    pub(crate) fn confirm_remove_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        crate::ui::confirm(
            window,
            cx,
            crate::ui::Confirm::new(
                t!("branch.confirm.remove_remote", name = name).to_string(),
                t!("branch.confirm.remove_remote_detail").to_string(),
            )
            .ok_label(t!("branch.confirm.remove_remote_button").to_string()),
            move |window, cx| {
                let name = name.clone();
                let _ = entity.update(cx, |view, cx| view.remove_remote(name, window, cx));
            },
        );
    }

    pub(crate) fn remove_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("branch.op.remove_remote", name = name).to_string();
        self.run_operation(Operation::RemoveRemote { name }, label, window, cx);
    }

    pub(crate) fn fetch_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prune = crate::settings::Settings::get(cx).behaviour.prune_on_fetch;
        let label = t!("branch.op.fetch", name = name).to_string();
        self.run_operation(
            Operation::Fetch {
                remote: Some(name),
                prune,
            },
            label,
            window,
            cx,
        );
    }

    pub(crate) fn unset_branch_upstream(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("branch.op.unset_upstream", branch = branch).to_string();
        self.run_operation(Operation::UnsetBranchUpstream { branch }, label, window, cx);
    }

    /// Runs the force form of an operation the user just confirmed.
    pub(crate) fn run_retry(&mut self, retry: Retry, window: &mut Window, cx: &mut Context<Self>) {
        match retry {
            Retry::DeleteBranch { name, .. } => self.delete_branch(name, true, window, cx),
            Retry::RemoveWorktree { path, .. } => self.remove_worktree(path, true, window, cx),
        }
    }

    /// Reloads history limited to `short_name`'s history and remembers the name
    /// for the sidebar banner. The full ref name is what the walk resolves, so
    /// a branch that is not `refs/heads/...` still reads exactly.
    pub(crate) fn show_only_ref(
        &mut self,
        kind: RefKind,
        short_name: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let spec = session
            .snapshot()
            .refs
            .iter()
            .find(|info| info.kind == kind && info.short_name == short_name)
            .map(|info| info.full_name.clone())
            .unwrap_or_else(|| short_name.to_owned());
        session.set_history_scope(HistoryScope::Refs(vec![spec]));
        self.branches.set_scope(Some(short_name.to_owned()));
        self.branches.clear_revealed();
        cx.notify();
    }

    /// Restores every ref as a history starting point, undoing "show only".
    pub(crate) fn show_all_refs(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        session.set_history_scope(HistoryScope::All);
        self.branches.set_scope(None);
        self.branches.clear_revealed();
        cx.notify();
    }

    /// Whether `kind`/`short_name` is kept out of the history graph.
    pub(crate) fn is_ref_hidden(&self, kind: RefKind, short_name: &str) -> bool {
        let full_name = ref_full_name(kind, short_name);
        self.hidden_refs.contains(&full_name)
    }

    /// How many refs the history graph leaves out.
    pub(crate) fn hidden_ref_count(&self) -> usize {
        self.hidden_refs.len()
    }

    /// Leaves `kind`/`short_name`'s history out of the graph until "show all".
    ///
    /// The ref itself stays in the sidebar, so the branch can still be checked
    /// out or merged; only the walk that starts at its tip is dropped.
    pub(crate) fn hide_ref_in_history(
        &mut self,
        kind: RefKind,
        short_name: &str,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        let full_name = ref_full_name(kind, short_name);
        if self.hidden_refs.contains(&full_name) {
            return;
        }
        self.hidden_refs.push(full_name.clone());
        if let Some(key) = self.repository_key.clone() {
            crate::settings::update(cx, |settings| {
                settings.history.hide_ref(&key, full_name.clone())
            });
        }
        if let Some(session) = self.session.as_ref() {
            session.set_hidden_refs(self.hidden_refs.clone());
        }
        cx.notify();
    }

    /// Restores every hidden ref to the history graph.
    pub(crate) fn show_hidden_refs(&mut self, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        if self.hidden_refs.is_empty() {
            return;
        }
        self.hidden_refs.clear();
        if let Some(key) = self.repository_key.clone() {
            crate::settings::update(cx, |settings| settings.history.show_all_refs(&key));
        }
        if let Some(session) = self.session.as_ref() {
            session.set_hidden_refs(Vec::new());
        }
        cx.notify();
    }

    /// Opens the merge/rebase menu for a branch dropped on `target`.
    ///
    /// The entries are decided up front, so a drop whose operations cannot run
    /// (the target is not checked out) still opens the menu and says why rather
    /// than doing nothing.
    pub(crate) fn open_branch_drop_menu(
        &mut self,
        source: DraggedBranch,
        target: String,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.branches.dialog_open() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let head = match session.snapshot().head.as_ref() {
            Some(HeadInfo::Branch { short_name, .. }) => Some(short_name.clone()),
            _ => None,
        };
        let fast_forward = (source.kind == RefKind::RemoteBranch)
            .then(|| self.fast_forward_ready(&source.name, &target))
            .flatten();
        let busy = self.is_busy();
        let Some(entries) = drop_entries(&source, &target, head.as_deref(), busy, fast_forward)
        else {
            return;
        };
        self.branches.drop_menu = Some(DropMenu {
            position,
            source,
            target,
            entries,
        });
        if let Some(focus) = self.branches.dialog_focus.clone() {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Runs one entry of the open branch drop menu.
    ///
    /// Merging and fast-forwarding are the existing merge operation, and the
    /// rebase goes through the same confirmation the branch menu opens; nothing
    /// here runs git itself.
    pub(crate) fn run_drop_action(
        &mut self,
        action: DropAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.branches.drop_menu.take() else {
            return;
        };
        self.focus_root(window, cx);
        cx.notify();
        if self.is_busy() {
            return;
        }
        match action {
            DropAction::Merge | DropAction::FastForward => {
                let label = if action == DropAction::FastForward {
                    t!(
                        "branch.op.fast_forward",
                        target = menu.target,
                        source = menu.source.name
                    )
                    .to_string()
                } else {
                    t!("branch.op.merge", branch = menu.source.name).to_string()
                };
                self.run_operation(
                    Operation::Merge {
                        branch: menu.source.name,
                        no_ff: false,
                    },
                    label,
                    window,
                    cx,
                );
            }
            DropAction::Rebase => {
                let tip = self.session.as_ref().and_then(|session| {
                    session
                        .snapshot()
                        .refs
                        .iter()
                        .find(|info| {
                            info.kind == menu.source.kind && info.short_name == menu.source.name
                        })
                        .map(|info| info.target)
                });
                match tip {
                    Some(tip) => self.confirm_rebase(tip, menu.source.name, window, cx),
                    None => {
                        self.status_error = Some(t!("branch.drop.cannot_read").to_string());
                        cx.notify();
                    }
                }
            }
        }
    }

    pub(crate) fn dismiss_drop_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.branches.drop_menu.take().is_some() {
            self.focus_root(window, cx);
            cx.notify();
        }
    }

    /// Whether `source`'s tip contains `target`'s, which lets a merge
    /// fast-forward instead of creating a merge commit. `None` when either tip
    /// cannot be read.
    fn fast_forward_ready(&self, source: &str, target: &str) -> Option<bool> {
        let session = self.session.as_ref()?;
        let snapshot = session.snapshot();
        let tip = |kind: RefKind, name: &str| {
            snapshot
                .refs
                .iter()
                .find(|info| info.kind == kind && info.short_name == name)
                .map(|info| info.target)
        };
        let target_tip = tip(RefKind::LocalBranch, target)?;
        let source_tip = tip(RefKind::RemoteBranch, source)?;
        session.repo().is_ancestor(target_tip, source_tip).ok()
    }
}

/// Checks out `remote` as `local`, creating `local` from the remote ref when it
/// does not exist yet.
pub(crate) fn checkout_remote(
    view: &mut RepoView,
    remote: &str,
    local: &str,
    existing: bool,
    window: &mut Window,
    cx: &mut Context<RepoView>,
) {
    if existing {
        let label = t!("branch.op.checkout", branch = local).to_string();
        view.run_operation(
            Operation::Checkout {
                branch: local.to_owned(),
            },
            label,
            window,
            cx,
        );
    } else {
        let label = t!(
            "branch.op.create_from_remote",
            local = local,
            remote = remote
        )
        .to_string();
        view.run_operation(
            Operation::CreateBranch {
                name: local.to_owned(),
                start_point: Some(remote.to_owned()),
                checkout: true,
            },
            label,
            window,
            cx,
        );
    }
}

fn row(
    id: impl Into<ElementId>,
    name: &str,
    state: crate::ui::RowState,
    colors: &Colors,
) -> Stateful<Div> {
    row_with_icon(id, None, name, state, colors)
}

fn row_with_icon(
    id: impl Into<ElementId>,
    item_icon: Option<&'static [u8]>,
    name: &str,
    state: crate::ui::RowState,
    colors: &Colors,
) -> Stateful<Div> {
    let name = strip_stash_prefix(name);
    let row = crate::ui::list_row(id, state, &colors.design)
        .h(px(ITEM_HEIGHT))
        // A list item is laid out at its content width unless it is told to
        // fill, which would leave most of the row outside its hitbox.
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .mx(px(0.0))
        // Leave room on the right so trailing labels stay clear of the
        // overlay vertical scrollbar.
        .pr(px(18.0))
        .rounded(px(7.0))
        .text_size(px(colors.design.small()))
        .text_color(colors.sidebar_foreground);
    let row = match item_icon {
        Some(item_icon) => row.child(glyph(item_icon, 14.0, colors.muted_foreground).flex_none()),
        None => row,
    };
    row.child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
}

fn glyph(data: &'static [u8], size: f32, color: Hsla) -> Icon {
    style::glyph_icon(data, size, color)
}

/// The sidebar lists `stash@{0}: message`; the row shows the message alone, as
/// the design does, while the click handler keeps using the stash's index.
fn strip_stash_prefix(name: &str) -> &str {
    match name
        .strip_prefix("stash@{")
        .and_then(|rest| rest.split_once("}: "))
    {
        Some((_, message)) => message,
        None => name,
    }
}

/// One remote's branch list in the sidebar.
#[derive(Debug, PartialEq, Eq)]
struct RemoteGroup {
    name: String,
    /// Shown as the header's tooltip, so a remote that has never been fetched
    /// still tells the user where it points.
    fetch_url: Option<String>,
    branches: Vec<String>,
}

/// `origin/main`, `origin/feat/x`, `upstream/main` grouped by their first path
/// component. Every configured remote gets a group even without branches, and
/// a remote-tracking ref for a remote that is no longer configured still gets
/// one so it cannot silently disappear.
fn remote_groups(entries: &[String], remotes: &[RemoteInfo]) -> Vec<RemoteGroup> {
    let mut groups: Vec<RemoteGroup> = remotes
        .iter()
        .map(|remote| RemoteGroup {
            name: remote.name.clone(),
            fetch_url: remote.fetch_url.clone(),
            branches: Vec::new(),
        })
        .collect();
    for name in entries {
        let group = name.split('/').next().unwrap_or(name).to_owned();
        match groups.iter_mut().find(|existing| existing.name == group) {
            Some(existing) => existing.branches.push(name.clone()),
            None => groups.push(RemoteGroup {
                name: group,
                fetch_url: None,
                branches: vec![name.clone()],
            }),
        }
    }
    for group in &mut groups {
        group.branches.sort_by(|a, b| natural_cmp(a, b));
    }
    groups
}

/// The names in natural order, so a numbered ref reads `feature-2` before
/// `feature-10` instead of after it.
pub(crate) fn natural_sorted(names: &[String]) -> Vec<&str> {
    let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
    sorted.sort_by(|a, b| natural_cmp(a, b));
    sorted
}

/// Orders names the way a person reads them: a run of ASCII digits compares as
/// a number, everything else compares case-insensitively, and names that only
/// differ in case fall back to their original text so the order is stable.
///
/// This is what keeps `feature-1, feature-2, …, feature-10` and
/// `v1.9.0, v1.10.0` in sequence; the snapshot's own order is lexical and puts
/// `feature-10` between `feature-1` and `feature-2`.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut left = a.as_bytes();
    let mut right = b.as_bytes();
    loop {
        match (left.first(), right.first()) {
            // The common prefix matches, so only case can still separate them.
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) if l.is_ascii_digit() && r.is_ascii_digit() => {
                let (left_run, left_rest) = split_digit_run(left);
                let (right_run, right_rest) = split_digit_run(right);
                match cmp_digit_runs(left_run, right_run) {
                    // Equally large runs may still differ in leading zeros.
                    Ordering::Equal => {
                        left = left_rest;
                        right = right_rest;
                    }
                    order => return order,
                }
            }
            (Some(l), Some(r)) => match l.to_ascii_lowercase().cmp(&r.to_ascii_lowercase()) {
                Ordering::Equal => {
                    left = &left[1..];
                    right = &right[1..];
                }
                order => return order,
            },
        }
    }
}

/// Splits a byte string at the end of its leading digit run.
fn split_digit_run(bytes: &[u8]) -> (&[u8], &[u8]) {
    let end = bytes
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(bytes.len());
    bytes.split_at(end)
}

/// Compares two digit runs by value without parsing them, so a run longer than
/// `u64` is ordered as correctly as a short one: strip the leading zeros, then
/// the shorter run is the smaller number, and equal lengths compare lexically.
fn cmp_digit_runs(a: &[u8], b: &[u8]) -> Ordering {
    let a = without_leading_zeros(a);
    let b = without_leading_zeros(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn without_leading_zeros(run: &[u8]) -> &[u8] {
    let start = run
        .iter()
        .position(|&byte| byte != b'0')
        .unwrap_or(run.len());
    &run[start..]
}

/// A section's "nothing here" line. Its indent follows the place it belongs
/// to: branch rows sit under the section labels, remote branches under a
/// remote's own header.
fn empty_row_at(text: &str, indent: Pixels, colors: &Colors) -> Div {
    div()
        .pl(indent)
        .pr_2()
        .h(px(ITEM_HEIGHT))
        .flex()
        .items_center()
        .text_size(px(11.0))
        .text_color(colors.muted_foreground)
        .child(text.to_owned())
}

fn create_card(
    form: &CreateForm,
    colors: &Colors,
    entity: &Entity<RepoView>,
    focus: Option<&FocusHandle>,
) -> AnyElement {
    let confirm = {
        let entity = entity.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| view.submit_branch_dialog(window, cx));
        }
    };
    let cancel = {
        let entity = entity.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| {
                view.branches.close_dialog();
                view.focus_root(window, cx);
                cx.notify();
            });
        }
    };
    let toggle = {
        let entity = entity.clone();
        move |checked: &bool, _: &mut Window, cx: &mut App| {
            let checked = *checked;
            entity.update(cx, |view, cx| {
                view.branches.set_checkout(checked);
                cx.notify();
            });
        }
    };

    let mut card = dialog_card(colors, entity, focus)
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.create").to_string()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!(
                    "branch.dialog.new_branch_start",
                    start = form.start.label
                )),
        )
        .child(Input::new(&form.input).w_full())
        .child(
            Checkbox::new("branch-create-checkout")
                .checked(form.checkout)
                .label(t!("branch.dialog.checkout_after").to_string())
                .on_change(toggle),
        );
    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.danger)
                .child(error.clone()),
        );
    }
    card.child(
        div()
            .flex()
            .justify_end()
            .gap_2()
            .pt_1()
            .child(
                crate::ui::button("branch-create-cancel", t!("common.cancel").to_string())
                    .size(crate::ui::ButtonSize::Medium)
                    .on_click(cancel),
            )
            .child(
                crate::ui::button("branch-create-ok", t!("branch.dialog.create").to_string())
                    .size(crate::ui::ButtonSize::Medium)
                    .on_click(confirm),
            ),
    )
    .into_any_element()
}

fn dialog_card(colors: &Colors, entity: &Entity<RepoView>, focus: Option<&FocusHandle>) -> Div {
    let confirm = {
        let entity = entity.clone();
        move |_: &ConfirmBranchDialog, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| view.submit_branch_dialog(window, cx));
        }
    };
    let cancel = {
        let entity = entity.clone();
        move |_: &CancelBranchDialog, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| {
                view.branches.close_dialog();
                view.focus_root(window, cx);
                cx.notify();
            });
        }
    };
    div()
        .w(px(420.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .bg(colors.overlay)
        .border_1()
        .border_color(colors.border)
        .text_size(px(12.0))
        .text_color(colors.foreground)
        .occlude()
        .when_some(focus, |card, focus| card.track_focus(focus))
        .key_context(BRANCH_DIALOG)
        .on_action(confirm)
        .on_action(cancel)
}

/// The Cancel and OK row every dialog ends with.
fn dialog_buttons(
    entity: &Entity<RepoView>,
    ok_id: &'static str,
    ok_label: impl Into<gpui::SharedString>,
    enabled: bool,
) -> Div {
    let confirm = {
        let entity = entity.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| view.submit_branch_dialog(window, cx));
        }
    };
    let cancel = {
        let entity = entity.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| {
                view.branches.close_dialog();
                view.focus_root(window, cx);
                cx.notify();
            });
        }
    };
    div()
        .flex()
        .justify_end()
        .gap_2()
        .pt_1()
        .child(
            crate::ui::button("branch-dialog-cancel", t!("common.cancel").to_string())
                .size(crate::ui::ButtonSize::Medium)
                .on_click(cancel),
        )
        .child(
            crate::ui::button(ok_id, ok_label)
                .size(crate::ui::ButtonSize::Medium)
                .disabled(!enabled)
                .on_click(confirm),
        )
}

fn rename_card(
    form: &RenameForm,
    colors: &Colors,
    entity: &Entity<RepoView>,
    focus: Option<&FocusHandle>,
) -> AnyElement {
    let toggle = {
        let entity = entity.clone();
        move |checked: &bool, _: &mut Window, cx: &mut App| {
            let checked = *checked;
            entity.update(cx, |view, cx| {
                view.branches.set_rename_remote(checked);
                cx.notify();
            });
        }
    };

    let mut card = dialog_card(colors, entity, focus)
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.rename").to_string()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!("branch.dialog.renaming", branch = form.branch)),
        )
        .child(Input::new(&form.input).w_full());
    match &form.upstream_label {
        Some(label) => {
            card = card.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(t!("branch.dialog.current_upstream", label = label)),
            );
            if form.upstream.is_some() {
                card = card
                    .child(
                        Checkbox::new("branch-rename-remote")
                            .checked(form.rename_remote)
                            .label(t!("branch.dialog.rename_remote_too").to_string())
                            .on_change(toggle),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(colors.muted_foreground)
                            .child(
                                "Pushes the new name, deletes the old remote branch and sets \
                                 the upstream.",
                            ),
                    );
            }
        }
        None => {
            card = card.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(t!("branch.dialog.no_upstream").to_string()),
            );
        }
    }
    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.danger)
                .child(error.clone()),
        );
    }
    card.child(dialog_buttons(
        entity,
        "branch-rename-ok",
        t!("branch.dialog.rename").to_string(),
        true,
    ))
    .into_any_element()
}

fn upstream_card(
    form: &UpstreamForm,
    colors: &Colors,
    entity: &Entity<RepoView>,
    focus: Option<&FocusHandle>,
) -> AnyElement {
    let mut card = dialog_card(colors, entity, focus)
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.set_upstream").to_string()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(match &form.current {
                    Some(current) => t!(
                        "branch.dialog.tracks",
                        branch = form.branch,
                        current = current
                    )
                    .to_string(),
                    None => t!("branch.dialog.has_no_upstream", branch = form.branch).to_string(),
                }),
        );

    if form.choices.is_empty() {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!("branch.dialog.no_remote_branches").to_string()),
        );
    } else {
        let rows = form
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let select = {
                    let entity = entity.clone();
                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                        entity.update(cx, |view, cx| {
                            view.branches.set_upstream_choice(index);
                            cx.notify();
                        });
                    }
                };
                crate::ui::list_row(
                    ("upstream-choice", index),
                    crate::ui::RowState::NORMAL.selected(form.selected == Some(index)),
                    &colors.design,
                )
                .cursor_pointer()
                .on_click(select)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(choice.label.clone()),
                )
            })
            .collect::<Vec<_>>();
        card = card.child(
            div()
                .id("upstream-choice-list")
                .flex()
                .flex_col()
                .gap_1()
                .max_h(px(200.0))
                .overflow_y_scroll()
                .children(rows),
        );
    }

    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.danger)
                .child(error.clone()),
        );
    }
    card.child(dialog_buttons(
        entity,
        "branch-upstream-ok",
        t!("branch.dialog.set_upstream").to_string(),
        form.selected.is_some(),
    ))
    .into_any_element()
}

fn add_remote_card(
    form: &AddRemoteForm,
    colors: &Colors,
    entity: &Entity<RepoView>,
    focus: Option<&FocusHandle>,
) -> AnyElement {
    let toggle = {
        let entity = entity.clone();
        move |checked: &bool, _: &mut Window, cx: &mut App| {
            let checked = *checked;
            entity.update(cx, |view, cx| {
                view.branches.set_fetch_now(checked);
                cx.notify();
            });
        }
    };
    let mut card = dialog_card(colors, entity, focus)
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.add_remote").to_string()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!("branch.dialog.add_remote_detail").to_string()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .child(t!("branch.dialog.name").to_string()),
        )
        .child(Input::new(&form.name).w_full())
        .child(
            div()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .child(t!("branch.dialog.url").to_string()),
        )
        .child(Input::new(&form.url).w_full())
        .child(
            Checkbox::new("remote-add-fetch")
                .checked(form.fetch)
                .label(t!("branch.dialog.fetch_now").to_string())
                .on_change(toggle),
        );
    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.danger)
                .child(error.clone()),
        );
    }
    card.child(dialog_buttons(
        entity,
        "remote-add-ok",
        t!("branch.dialog.add_remote").to_string(),
        true,
    ))
    .into_any_element()
}

fn rename_remote_card(
    form: &RenameRemoteForm,
    colors: &Colors,
    entity: &Entity<RepoView>,
    focus: Option<&FocusHandle>,
) -> AnyElement {
    let mut card = dialog_card(colors, entity, focus)
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.rename_remote").to_string()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!("branch.dialog.renaming_remote", name = form.name).to_string()),
        )
        .child(Input::new(&form.input).w_full());
    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.danger)
                .child(error.clone()),
        );
    }
    card.child(dialog_buttons(
        entity,
        "remote-rename-ok",
        t!("branch.dialog.rename_remote").to_string(),
        true,
    ))
    .into_any_element()
}

fn edit_remote_card(
    form: &EditRemoteForm,
    colors: &Colors,
    entity: &Entity<RepoView>,
    focus: Option<&FocusHandle>,
) -> AnyElement {
    let mut card = dialog_card(colors, entity, focus)
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.edit_remote", name = form.name)),
        )
        .child(
            div()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .child(t!("branch.dialog.fetch_url").to_string()),
        )
        .child(Input::new(&form.fetch).w_full());
    match &form.push {
        Some(push) => {
            card = card
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .child(t!("branch.dialog.push_url").to_string()),
                )
                .child(Input::new(push).w_full());
        }
        None => {
            card = card.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(t!("branch.dialog.push_uses_fetch").to_string()),
            );
        }
    }
    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.danger)
                .child(error.clone()),
        );
    }
    card.child(dialog_buttons(
        entity,
        "remote-url-ok",
        t!("branch.dialog.save_url").to_string(),
        true,
    ))
    .into_any_element()
}

/// Offers the force form of an operation git refused.
///
/// The session event that learns about the refusal has no window, so the dialog
/// is opened on the active one. It always asks, even when confirmations are off:
/// the forced form is a different, stronger action than the one the user chose,
/// and skipping the ask would silently escalate it.
pub(crate) fn offer_force(entity: Entity<RepoView>, retry: Retry, cx: &mut App) {
    let Some(handle) = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())
    else {
        log::warn!("cannot offer the forced operation without a window");
        return;
    };
    let entity = entity.downgrade();
    let _ = handle.update(cx, |_, window, cx| {
        let confirm = match &retry {
            Retry::DeleteBranch { name, .. } => crate::ui::Confirm::new(
                t!("history.delete_branch", name = name).to_string(),
                t!("branch.confirm.force_delete_detail").to_string(),
            )
            .ok_label(t!("branch.confirm.force_delete").to_string()),
            Retry::RemoveWorktree { path, .. } => crate::ui::Confirm::new(
                t!("branch.confirm.remove_worktree", path = worktree_name(path)).to_string(),
                t!("branch.confirm.remove_worktree_detail").to_string(),
            )
            .ok_label(t!("branch.confirm.force_remove").to_string()),
        }
        .always_ask();
        crate::ui::confirm(window, cx, confirm, move |window, cx| {
            let retry = retry.clone();
            let _ = entity.update(cx, |view, cx| view.run_retry(retry, window, cx));
        });
    });
}

/// The name a worktree is referred to by: its directory, not the whole path.
pub(crate) fn worktree_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `git check-ref-format` rules that apply to `refs/heads/<name>`, plus the one
/// rule the CLI enforces by accident: a leading `-` would parse as an option.
pub(crate) fn validate_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(t!("branch.validation.name_required").to_string());
    }
    if name == "@" {
        return Err(t!("branch.validation.name_shorthand").to_string());
    }
    if name.starts_with('-') {
        return Err(t!("branch.validation.leading_dash").to_string());
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err(t!("branch.validation.slash_edges").to_string());
    }
    if name.contains("//") {
        return Err(t!("branch.validation.double_slash").to_string());
    }
    if name.contains("..") {
        return Err(t!("branch.validation.dot_dot").to_string());
    }
    if name.contains("@{") {
        return Err(t!("branch.validation.at_brace").to_string());
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(t!("branch.validation.control").to_string());
    }
    if name.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err(t!("branch.validation.specials").to_string());
    }
    if name.ends_with('.') {
        return Err(t!("branch.validation.trailing_dot").to_string());
    }
    if name.split('/').any(|part| {
        part.is_empty() || part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock")
    }) {
        return Err(t!("branch.validation.remote_part").to_string());
    }
    Ok(())
}

/// Names git accepts for a remote.
///
/// A remote's branches live under `refs/remotes/<name>/`, so git applies the
/// ref-name rules to the name; these are the same rules as for a branch, with
/// the message naming a remote instead. Unlike a branch, a remote name may
/// contain `/` (git stores the tracking refs below it), so parts are only
/// checked for the forbidden forms.
pub(crate) fn validate_remote_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(t!("branch.validation.remote_required").to_string());
    }
    if name.starts_with('-') {
        return Err(t!("branch.validation.remote_leading_dash").to_string());
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err(t!("branch.validation.remote_slash_edges").to_string());
    }
    if name.contains("//") {
        return Err(t!("branch.validation.remote_double_slash").to_string());
    }
    if name.contains("..") {
        return Err(t!("branch.validation.remote_dot_dot").to_string());
    }
    if name.contains("@{") {
        return Err(t!("branch.validation.remote_at_brace").to_string());
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(t!("branch.validation.remote_control").to_string());
    }
    if name.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err(t!("branch.validation.remote_specials").to_string());
    }
    if name.ends_with('.') {
        return Err(t!("branch.validation.remote_trailing_dot").to_string());
    }
    if name.split('/').any(|part| {
        part.is_empty() || part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock")
    }) {
        return Err(t!("branch.validation.remote_part").to_string());
    }
    Ok(())
}

/// The URL a remote command receives: non-empty, and not something git would
/// read as an option.
pub(crate) fn validate_remote_url(url: &str) -> Result<&str, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err(t!("branch.validation.url_required").to_string());
    }
    if url.starts_with('-') {
        return Err(t!("branch.validation.url_leading_dash").to_string());
    }
    Ok(url)
}

/// The sidebar label for a branch's upstream: `↑2 ↓1`, `↑3`, `↓1`, `?`,
/// or nothing when the branch is level with or missing its upstream.
pub(crate) fn upstream_label(upstream: &UpstreamInfo) -> Option<String> {
    if upstream.gone {
        return None;
    }
    let (Some(ahead), Some(behind)) = (upstream.ahead, upstream.behind) else {
        return Some("?".to_owned());
    };
    let mut label = String::new();
    if ahead > 0 {
        label.push_str(&t!("branch.upstream.ahead", count = ahead));
    }
    if behind > 0 {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push_str(&t!("branch.upstream.behind", count = behind));
    }
    (!label.is_empty()).then_some(label)
}

/// Why an upstream shows a marker, when it is `?` or gone.
pub(crate) fn upstream_note(upstream: &UpstreamInfo) -> Option<String> {
    if upstream.gone {
        return Some(t!("branch.upstream.gone_tooltip").to_string());
    }
    (upstream.ahead.is_none() || upstream.behind.is_none())
        .then(|| t!("branch.upstream.shallow").to_string())
}

/// The full ref name of a ref the sidebar lists by its short name.
///
/// The walk hides refs by full name, so the prefix has to match the kind: a
/// local `topic` and a remote `topic` are different refs.
pub(crate) fn ref_full_name(kind: RefKind, short_name: &str) -> String {
    match kind {
        RefKind::LocalBranch => format!("refs/heads/{short_name}"),
        RefKind::RemoteBranch => format!("refs/remotes/{short_name}"),
        RefKind::Tag => format!("refs/tags/{short_name}"),
    }
}

/// `origin/feature/login` -> `feature/login`. The remote name is the first path
/// component, which is how `refs/remotes/` names are built.
///
/// `origin/HEAD` is the remote's symbolic default-branch ref, not something
/// that can be checked out, so it maps to nothing.
pub(crate) fn local_name_for_remote(remote: &str) -> Option<&str> {
    let (_, local) = remote.split_once('/')?;
    if local.is_empty() || local == "HEAD" {
        return None;
    }
    Some(local)
}

/// Every remote branch, as the set-upstream dialog lists it: the sidebar's
/// natural order, with the remote's symbolic `HEAD` left out.
pub(crate) fn upstream_choices(refs: &[RefInfo], remotes: &[RemoteInfo]) -> Vec<UpstreamChoice> {
    let mut choices: Vec<UpstreamChoice> = refs
        .iter()
        .filter(|info| info.kind == RefKind::RemoteBranch)
        .filter_map(|info| {
            let (remote, remote_branch) = split_remote_branch(&info.short_name, remotes)?;
            if remote_branch == "HEAD" {
                return None;
            }
            Some(UpstreamChoice {
                label: info.short_name.clone(),
                remote: remote.to_owned(),
                remote_branch: remote_branch.to_owned(),
            })
        })
        .collect();
    choices.sort_by(|a, b| natural_cmp(&a.label, &b.label));
    choices
}

/// Splits `origin/topic` into the remote and the branch on it.
///
/// git accepts remote names that contain `/`, so the configured remote names
/// are matched longest first instead of taking the first component. A tracking
/// ref whose remote is no longer configured falls back to that first component,
/// which is all that is left to go on.
fn split_remote_branch<'a>(
    short_name: &'a str,
    remotes: &'a [RemoteInfo],
) -> Option<(&'a str, &'a str)> {
    let mut best: Option<(&str, &str)> = None;
    for remote in remotes {
        let Some(rest) = short_name
            .strip_prefix(remote.name.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        if best.is_none_or(|(name, _)| remote.name.len() > name.len()) {
            best = Some((remote.name.as_str(), rest));
        }
    }
    best.or_else(|| {
        short_name
            .split_once('/')
            .filter(|(_, rest)| !rest.is_empty())
    })
}

/// `git branch -d` refuses a branch with commits that are not reachable from
/// HEAD; that message is what turns the delete dialog into a force offer.
fn is_not_fully_merged(error: &str) -> bool {
    error.contains("not fully merged")
}

/// `git worktree remove` refuses a checkout with local changes; that message is
/// what turns the removal into a force offer. A missing directory is git's
/// `validation failed` instead, and forcing would not help: Prune is the fix.
fn is_dirty_worktree(error: &str) -> bool {
    error.contains("contains modified or untracked files")
        || error.contains("use --force to delete it")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(name: &str, ahead: u32, behind: u32, gone: bool) -> UpstreamInfo {
        UpstreamInfo {
            name: name.to_owned(),
            ahead: Some(ahead),
            behind: Some(behind),
            gone,
        }
    }

    fn unknown_upstream(name: &str) -> UpstreamInfo {
        UpstreamInfo {
            name: name.to_owned(),
            ahead: None,
            behind: None,
            gone: false,
        }
    }

    #[test]
    fn upstream_labels_show_only_the_sides_that_differ() {
        assert_eq!(
            upstream_label(&upstream("origin/main", 2, 1, false)).as_deref(),
            Some("↑2 ↓1")
        );
        assert_eq!(
            upstream_label(&upstream("origin/main", 3, 0, false)).as_deref(),
            Some("↑3")
        );
        assert_eq!(
            upstream_label(&upstream("origin/main", 0, 4, false)).as_deref(),
            Some("↓4")
        );
        assert_eq!(
            upstream_label(&upstream("origin/main", 0, 0, false)),
            None,
            "a branch level with its upstream needs no label"
        );
        assert_eq!(
            upstream_label(&upstream("origin/gone", 0, 0, true)),
            None,
            "a deleted upstream renders an icon rather than a text label"
        );
    }

    #[test]
    fn unknown_counts_render_a_question_mark_with_a_reason() {
        let unknown = unknown_upstream("origin/main");
        assert_eq!(upstream_label(&unknown).as_deref(), Some("?"));
        assert_eq!(
            upstream_note(&unknown).as_deref(),
            Some("Counts unavailable: history is shallow")
        );
        assert_eq!(
            upstream_note(&upstream("origin/main", 0, 0, false)),
            None,
            "a known count needs no explanation"
        );
        assert_eq!(
            upstream_note(&upstream("origin/gone", 0, 0, true)).as_deref(),
            Some("The upstream branch was deleted on the remote"),
            "a gone upstream carries its explanation in its tooltip"
        );
    }

    #[test]
    fn branch_names_follow_git_check_ref_format() {
        for name in [
            "main",
            "feat/branch-sidebar",
            "fix-123",
            "v1.0.0",
            "a/b/c",
            "release.locked",
        ] {
            assert_eq!(validate_branch_name(name), Ok(()), "{name} should be valid");
        }

        for name in [
            "",
            "@",
            "-foo",
            "/foo",
            "foo/",
            "foo bar",
            "foo\tbar",
            "foo..bar",
            "foo/../bar",
            "foo//bar",
            "foo@{1}",
            "foo~1",
            "foo^",
            "foo:bar",
            "foo?",
            "foo*",
            "foo[bar]",
            "foo\\bar",
            "foo.",
            "foo./bar",
            "foo/.bar",
            ".foo",
            "foo.lock",
            "foo.lock/bar",
        ] {
            assert!(
                validate_branch_name(name).is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn stash_rows_show_the_message_without_the_index() {
        assert_eq!(
            strip_stash_prefix("stash@{0}: WIP on diff options"),
            "WIP on diff options"
        );
        assert_eq!(strip_stash_prefix("main"), "main");
        assert_eq!(strip_stash_prefix("stash@{0}"), "stash@{0}");
    }

    #[test]
    fn remote_names_follow_git_s_ref_rules() {
        for name in ["origin", "upstream", "team/one", "backup-2", "a.b"] {
            assert_eq!(validate_remote_name(name), Ok(()), "{name} should be valid");
        }

        for name in [
            "",
            "  ",
            "-origin",
            "/origin",
            "origin/",
            "ori//gin",
            "ori..gin",
            "ori@{1}gin",
            "ori gin",
            "ori\ngin",
            "ori~gin",
            "ori:gin",
            "ori?gin",
            "ori*gin",
            "ori[gin",
            "ori\\gin",
            "origin.",
            ".origin",
            "origin.lock",
            "origin.lock/main",
        ] {
            assert!(
                validate_remote_name(name).is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn a_remote_url_must_be_present_and_not_an_option() {
        assert_eq!(
            validate_remote_url("  https://example.invalid/repo.git "),
            Ok("https://example.invalid/repo.git")
        );
        assert_eq!(validate_remote_url("/tmp/repo.git"), Ok("/tmp/repo.git"));
        assert!(validate_remote_url("   ").is_err());
        assert!(validate_remote_url("--upload-pack=evil").is_err());
    }

    #[test]
    fn the_rename_dialog_renames_the_remote_only_when_asked() {
        let upstream = RemoteBranch {
            remote: "origin".into(),
            branch: "topic".into(),
        };
        let request =
            RenameRequest::parse("topic", "  feature/login ", Some(&upstream), false).unwrap();
        assert_eq!(request.new_name, "feature/login");
        assert_eq!(
            request.rename_upstream, None,
            "renaming on the remote is opt-in"
        );

        let both = RenameRequest::parse("topic", "feature", Some(&upstream), true).unwrap();
        assert_eq!(both.rename_upstream, Some(upstream));
    }

    #[test]
    fn the_rename_dialog_refuses_an_invalid_or_unchanged_name() {
        assert!(RenameRequest::parse("topic", "topic", None, false).is_err());
        assert!(RenameRequest::parse("topic", "  topic  ", None, false).is_err());
        assert!(RenameRequest::parse("topic", "bad..name", None, false).is_err());
        assert!(RenameRequest::parse("topic", "", None, false).is_err());
        // A branch without an upstream has nothing to rename on a remote even
        // if the flag somehow arrived ticked.
        let request = RenameRequest::parse("topic", "renamed", None, true).unwrap();
        assert_eq!(request.rename_upstream, None);
    }

    #[test]
    fn the_add_remote_dialog_validates_its_name_and_url() {
        let request = AddRemoteRequest::parse(" origin ", " /tmp/repo.git ", true).unwrap();
        assert_eq!(request.name, "origin");
        assert_eq!(request.url, "/tmp/repo.git");
        assert!(request.fetch);

        assert!(AddRemoteRequest::parse("", "/tmp/repo.git", false).is_err());
        assert!(AddRemoteRequest::parse("origin", "", false).is_err());
        assert!(AddRemoteRequest::parse("bad name", "/tmp/repo.git", false).is_err());
        assert!(AddRemoteRequest::parse("origin", "-x", false).is_err());
    }

    #[test]
    fn the_edit_url_dialog_sets_a_push_url_only_when_the_remote_has_one() {
        let fetch_only = EditRemoteUrlRequest::parse("origin", " /tmp/a.git ", None).unwrap();
        assert_eq!(fetch_only.fetch_url, "/tmp/a.git");
        assert_eq!(fetch_only.push_url, None);

        let both =
            EditRemoteUrlRequest::parse("origin", "/tmp/a.git", Some(" /tmp/b.git ")).unwrap();
        assert_eq!(both.push_url.as_deref(), Some("/tmp/b.git"));

        assert!(EditRemoteUrlRequest::parse("origin", "", None).is_err());
        assert!(
            EditRemoteUrlRequest::parse("origin", "/tmp/a.git", Some("  ")).is_err(),
            "a push URL that is shown must stay filled in"
        );
    }

    fn ref_info(kind: RefKind, short_name: &str) -> RefInfo {
        RefInfo {
            full_name: match kind {
                RefKind::LocalBranch => format!("refs/heads/{short_name}"),
                RefKind::RemoteBranch => format!("refs/remotes/{short_name}"),
                RefKind::Tag => format!("refs/tags/{short_name}"),
            },
            short_name: short_name.to_owned(),
            kind,
            target: oid(1),
        }
    }

    #[test]
    fn set_upstream_lists_remote_branches_and_drops_the_symbolic_head() {
        let remotes = [
            remote("origin", Some("/tmp/origin.git")),
            remote("team/one", Some("/tmp/team.git")),
        ];
        let refs = [
            ref_info(RefKind::RemoteBranch, "origin/main"),
            ref_info(RefKind::RemoteBranch, "origin/HEAD"),
            ref_info(RefKind::RemoteBranch, "origin/feature"),
            ref_info(RefKind::RemoteBranch, "team/one/main"),
            ref_info(RefKind::LocalBranch, "main"),
            ref_info(RefKind::Tag, "v1"),
        ];

        let choices = upstream_choices(&refs, &remotes);
        let labels: Vec<&str> = choices.iter().map(|choice| choice.label.as_str()).collect();
        assert_eq!(
            labels,
            ["origin/feature", "origin/main", "team/one/main"],
            "remote branches in natural order, without the remote's HEAD"
        );
        assert_eq!(
            choices[2].remote, "team/one",
            "a remote name with a slash is not split at its first component"
        );
        assert_eq!(choices[2].remote_branch, "main");
    }

    #[test]
    fn a_tracking_ref_of_an_unconfigured_remote_falls_back_to_its_first_part() {
        let refs = [ref_info(RefKind::RemoteBranch, "gone/main")];
        let choices = upstream_choices(&refs, &[]);
        assert_eq!(choices[0].remote, "gone");
        assert_eq!(choices[0].remote_branch, "main");
        assert_eq!(split_remote_branch("origin", &[]), None);
    }

    #[test]
    fn the_upstream_dialog_starts_on_the_current_upstream_or_the_first_branch() {
        let choice = |label: &str, remote: &str, branch: &str| UpstreamChoice {
            label: label.to_owned(),
            remote: remote.to_owned(),
            remote_branch: branch.to_owned(),
        };
        let choices = vec![
            choice("origin/main", "origin", "main"),
            choice("origin/topic", "origin", "topic"),
        ];

        let mut branches = Branches::default();
        branches.open_upstream(
            "feature".into(),
            choices.clone(),
            Some("origin/topic".into()),
        );
        let selected = match &branches.dialog {
            Some(Dialog {
                kind: DialogKind::Upstream(form),
                ..
            }) => form.selected,
            _ => panic!("the upstream dialog is open"),
        };
        assert_eq!(
            selected,
            Some(1),
            "the branch's current upstream is the starting choice"
        );

        branches.open_upstream("feature".into(), choices.clone(), None);
        let selected = match &branches.dialog {
            Some(Dialog {
                kind: DialogKind::Upstream(form),
                ..
            }) => form.selected,
            _ => panic!("the upstream dialog is open"),
        };
        assert_eq!(selected, Some(0));

        branches.open_upstream("feature".into(), Vec::new(), None);
        let selected = match &branches.dialog {
            Some(Dialog {
                kind: DialogKind::Upstream(form),
                ..
            }) => form.selected,
            _ => panic!("the upstream dialog is open"),
        };
        assert_eq!(
            selected, None,
            "with nothing to choose there is no default upstream"
        );
    }

    #[test]
    fn revealing_a_ref_moves_the_highlight_until_a_commit_is_selected() {
        let mut reveal = RevealState::default();
        assert!(!reveal.is_ref(RefKind::LocalBranch, "main"));

        reveal.reveal_ref(RefKind::LocalBranch, "main");
        assert!(reveal.is_ref(RefKind::LocalBranch, "main"));

        // Another ref replaces the highlight instead of adding to it.
        reveal.reveal_ref(RefKind::RemoteBranch, "origin/main");
        assert!(!reveal.is_ref(RefKind::LocalBranch, "main"));
        assert!(reveal.is_ref(RefKind::RemoteBranch, "origin/main"));

        reveal.reveal_stash(2);
        assert!(!reveal.is_ref(RefKind::RemoteBranch, "origin/main"));
        assert!(reveal.is_stash(2));
        assert!(!reveal.is_stash(1));

        // Selecting a commit clears whichever row was revealed.
        reveal.clear();
        assert!(!reveal.is_stash(2));
        assert!(!reveal.is_ref(RefKind::LocalBranch, "main"));
    }

    fn remote(name: &str, fetch_url: Option<&str>) -> RemoteInfo {
        RemoteInfo {
            name: name.to_owned(),
            fetch_url: fetch_url.map(str::to_owned),
            push_url: None,
        }
    }

    #[test]
    fn every_configured_remote_gets_a_group_even_without_branches() {
        let remotes = [
            remote("backup", Some("/tmp/backup.git")),
            remote("origin", Some("https://example.invalid/repo.git")),
        ];
        let entries = vec!["origin/main".to_owned(), "origin/feature".to_owned()];

        let groups = remote_groups(&entries, &remotes);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "backup");
        assert!(
            groups[0].branches.is_empty(),
            "a never-fetched remote must still be listed"
        );
        assert_eq!(
            groups[0].fetch_url.as_deref(),
            Some("/tmp/backup.git"),
            "the header tooltip needs the fetch URL"
        );
        assert_eq!(groups[1].name, "origin");
        assert_eq!(
            groups[1].branches,
            ["origin/feature", "origin/main"],
            "each remote's branches read in natural order"
        );
    }

    #[test]
    fn a_remote_tracking_ref_without_a_configured_remote_keeps_its_group() {
        let entries = vec!["gone/main".to_owned()];
        let groups = remote_groups(&entries, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "gone");
        assert_eq!(groups[0].branches, ["gone/main"]);
        assert_eq!(groups[0].fetch_url, None);
    }

    #[test]
    fn no_configured_remotes_and_no_refs_means_no_groups() {
        assert!(remote_groups(&[], &[]).is_empty());
    }

    #[test]
    fn remote_branches_map_to_the_local_name_after_the_remote() {
        assert_eq!(local_name_for_remote("origin/main"), Some("main"));
        assert_eq!(
            local_name_for_remote("origin/feature/login"),
            Some("feature/login")
        );
        assert_eq!(local_name_for_remote("upstream/fix-1"), Some("fix-1"));
        assert_eq!(local_name_for_remote("origin"), None);
        assert_eq!(local_name_for_remote("origin/"), None);
        assert_eq!(
            local_name_for_remote("origin/HEAD"),
            None,
            "the remote's symbolic HEAD is not a branch"
        );
    }

    #[test]
    fn only_an_unmerged_delete_offers_force() {
        assert!(is_not_fully_merged(
            "error: the branch 'topic' is not fully merged\nhint: ..."
        ));
        assert!(!is_not_fully_merged("fatal: branch 'topic' not found"));
        assert!(!is_not_fully_merged("error: cannot delete branch 'main'"));
    }

    #[test]
    fn a_failed_unforced_delete_leaves_a_force_offer() {
        let mut branches = Branches::default();
        branches.start(
            OperationId(7),
            "delete branch topic".to_owned(),
            Some(Retry::DeleteBranch {
                name: "topic".to_owned(),
                forced: false,
            }),
        );
        let label = branches.finish(
            OperationId(7),
            Some("error: the branch 'topic' is not fully merged"),
        );
        assert_eq!(label.as_deref(), Some("delete branch topic"));
        assert_eq!(
            branches.take_force_retry(),
            Some(Retry::DeleteBranch {
                name: "topic".to_owned(),
                forced: true,
            }),
            "the offer must rerun the delete with force"
        );
        assert_eq!(
            branches.take_force_retry(),
            None,
            "the offer is taken once, so it cannot reopen by itself"
        );
    }

    #[test]
    fn a_forced_delete_failure_does_not_loop() {
        let mut branches = Branches::default();
        branches.start(
            OperationId(8),
            "delete branch topic".to_owned(),
            Some(Retry::DeleteBranch {
                name: "topic".to_owned(),
                forced: true,
            }),
        );
        branches.finish(
            OperationId(8),
            Some("error: the branch 'topic' is not fully merged"),
        );
        assert_eq!(branches.take_force_retry(), None);
    }

    #[test]
    fn a_dirty_worktree_removal_offers_a_forced_removal() {
        let mut branches = Branches::default();
        branches.start(
            OperationId(9),
            "remove worktree strata-topic".to_owned(),
            Some(Retry::RemoveWorktree {
                path: PathBuf::from("/src/strata-topic"),
                forced: false,
            }),
        );
        let label = branches.finish(
            OperationId(9),
            Some(
                "fatal: '/src/strata-topic' contains modified or untracked files, \
                 use --force to delete it",
            ),
        );
        assert_eq!(label.as_deref(), Some("remove worktree strata-topic"));
        assert_eq!(
            branches.take_force_retry(),
            Some(Retry::RemoveWorktree {
                path: PathBuf::from("/src/strata-topic"),
                forced: true,
            })
        );
    }

    #[test]
    fn a_missing_worktree_checkout_is_pruned_not_forced() {
        let mut branches = Branches::default();
        branches.start(
            OperationId(10),
            "remove worktree strata-gone".to_owned(),
            Some(Retry::RemoveWorktree {
                path: PathBuf::from("/src/strata-gone"),
                forced: false,
            }),
        );
        branches.finish(
            OperationId(10),
            Some("fatal: validation failed, cannot remove working tree"),
        );
        assert_eq!(
            branches.take_force_retry(),
            None,
            "forcing cannot help a worktree whose directory is gone; pruning does"
        );
    }

    fn sorted(names: &[String]) -> Vec<String> {
        natural_sorted(names)
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }

    #[test]
    fn numbered_branches_sort_by_their_number_not_their_text() {
        let mut shuffled: Vec<String> =
            (1..=12).map(|number| format!("feature-{number}")).collect();
        shuffled.reverse();
        shuffled.push("main".to_owned());

        let expected: Vec<String> = (1..=12)
            .map(|number| format!("feature-{number}"))
            .chain(std::iter::once("main".to_owned()))
            .collect();
        assert_eq!(sorted(&shuffled), expected);
    }

    #[test]
    fn names_that_differ_only_in_case_keep_their_original_order() {
        let names = ["main".to_owned(), "MAIN".to_owned(), "Main".to_owned()];
        assert_eq!(sorted(&names), ["MAIN", "Main", "main"]);
    }

    #[test]
    fn digit_runs_compare_as_numbers_and_letters_ignore_case() {
        assert_eq!(natural_cmp("feature-2", "feature-10"), Ordering::Less);
        assert_eq!(natural_cmp("feature-10", "feature-2"), Ordering::Greater);
        assert_eq!(natural_cmp("feature-2", "feature-2-hotfix"), Ordering::Less);
        assert_eq!(
            natural_cmp("v1.9.0", "v1.10.0"),
            Ordering::Less,
            "a version's components are numbers, not text"
        );
        assert_eq!(natural_cmp("v2.0.0", "v1.10.0"), Ordering::Greater);
        assert_eq!(natural_cmp("topic-1", "topic-a"), Ordering::Less);
        // Only case separates these, so the original text breaks the tie.
        assert_eq!(natural_cmp("Feature-1", "feature-1"), Ordering::Less);
        assert_eq!(natural_cmp("feature-1", "Feature-1"), Ordering::Greater);
        assert_eq!(natural_cmp("feature-1", "feature-1"), Ordering::Equal);
    }

    #[test]
    fn leading_zeros_do_not_change_a_run_s_value() {
        assert_eq!(natural_cmp("feature-007", "feature-8"), Ordering::Less);
        assert_eq!(natural_cmp("feature-010", "feature-9"), Ordering::Greater);
        assert_eq!(
            natural_cmp("feature-07", "feature-7"),
            Ordering::Less,
            "equal numbers still need a stable order"
        );
    }

    #[test]
    fn digit_runs_too_long_for_u64_still_order_by_value() {
        let forty_nines = "9".repeat(40);
        // 41 digits is more than 40, whatever the value.
        let forty_one_digits = format!("1{}", "0".repeat(40));
        assert_eq!(natural_cmp(&forty_nines, &forty_one_digits), Ordering::Less);

        let just_below = format!("{}{}", "9".repeat(39), "8");
        assert_eq!(natural_cmp(&just_below, &forty_nines), Ordering::Less);
        assert_eq!(natural_cmp(&forty_nines, &forty_nines), Ordering::Equal);
    }

    fn sidebar_data(
        local: &[&str],
        remote: &[&str],
        tags: &[&str],
        stashes: &[&str],
    ) -> SidebarData {
        let owned = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
        SidebarData {
            head_branch: Some("main".to_owned()),
            working_tree: (0, 0),
            local: owned(local),
            remote: owned(remote),
            tags: owned(tags),
            stashes: owned(stashes),
        }
    }

    fn flatten_with(
        branches: &Branches,
        data: SidebarData,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
    ) -> Vec<SidebarItem> {
        branches.flatten(
            data,
            upstreams,
            remotes,
            &SidebarExtras::default(),
            &Design::default(),
        )
    }

    fn flatten(branches: &Branches, data: SidebarData) -> Vec<SidebarItem> {
        flatten_with(branches, data, &HashMap::new(), &[])
    }

    fn flatten_extras(
        branches: &Branches,
        data: SidebarData,
        extras: &SidebarExtras<'_>,
    ) -> Vec<SidebarItem> {
        branches.flatten(data, &HashMap::new(), &[], extras, &Design::default())
    }

    #[test]
    fn sections_flatten_into_headings_and_rows() {
        let branches = Branches::default();
        let mut upstreams = HashMap::new();
        upstreams.insert("main".to_owned(), upstream("origin/main", 2, 1, false));
        upstreams.insert("topic".to_owned(), upstream("origin/topic", 0, 0, true));

        let items = flatten_with(
            &branches,
            sidebar_data(&["topic", "main"], &[], &[], &[]),
            &upstreams,
            &[],
        );

        assert_eq!(
            items,
            vec![
                SidebarItem::SectionHeader {
                    section: Section::Local,
                    count: 2,
                    open: true,
                },
                SidebarItem::LocalBranch {
                    name: "main".into(),
                    label: "main".into(),
                    index: 0,
                    depth: 0,
                    upstream: Some("↑2 ↓1".into()),
                    upstream_gone: false,
                    upstream_note: None,
                },
                SidebarItem::LocalBranch {
                    name: "topic".into(),
                    label: "topic".into(),
                    index: 1,
                    depth: 0,
                    upstream: None,
                    upstream_gone: true,
                    upstream_note: Some("The upstream branch was deleted on the remote".into()),
                },
                SidebarItem::SectionHeader {
                    section: Section::Remote,
                    count: 0,
                    open: true,
                },
                SidebarItem::SectionHeader {
                    section: Section::Tags,
                    count: 0,
                    open: true,
                },
                SidebarItem::SectionHeader {
                    section: Section::Stashes,
                    count: 0,
                    open: true,
                },
            ],
            "an empty section is its heading and a zero, with no placeholder row"
        );
    }

    #[test]
    fn a_collapsed_section_contributes_only_its_heading() {
        let mut branches = Branches::default();
        branches.toggle(Section::Tags.index());

        let items = flatten(
            &branches,
            sidebar_data(&["main"], &[], &["v1", "v2"], &["stash@{0}: wip"]),
        );

        assert!(items.contains(&SidebarItem::SectionHeader {
            section: Section::Tags,
            count: 2,
            open: false,
        }));
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, SidebarItem::Tag { .. })),
            "a collapsed section draws no rows"
        );
        assert!(
            items
                .iter()
                .any(|item| matches!(item, SidebarItem::Stash { .. })),
            "the sections after it are unaffected"
        );
    }

    #[test]
    fn hidden_sections_contribute_nothing() {
        let branches = Branches::default();
        let mut design = Design::default();
        design.show_tags = false;
        design.show_stashes = false;

        let items = branches.flatten(
            sidebar_data(&["main"], &[], &["v1"], &["stash@{0}: wip"]),
            &HashMap::new(),
            &[],
            &SidebarExtras::default(),
            &design,
        );

        assert!(
            !items.iter().any(|item| matches!(
                item,
                SidebarItem::SectionHeader {
                    section: Section::Tags | Section::Stashes,
                    ..
                } | SidebarItem::Tag { .. }
                    | SidebarItem::Stash { .. }
            )),
            "a section the settings hide has neither a heading nor rows"
        );
    }

    #[test]
    fn the_remote_section_groups_branches_under_their_remote() {
        let branches = Branches::default();
        let remotes = [
            remote("backup", Some("/tmp/backup.git")),
            remote("origin", Some("https://example.invalid/repo.git")),
        ];

        let items = flatten_with(
            &branches,
            sidebar_data(
                &["main"],
                &["origin/main", "origin/feature", "origin/HEAD"],
                &[],
                &[],
            ),
            &HashMap::new(),
            &remotes,
        );

        let remote_items: Vec<&SidebarItem> = items
            .iter()
            .skip_while(|item| {
                !matches!(
                    item,
                    SidebarItem::SectionHeader {
                        section: Section::Remote,
                        ..
                    }
                )
            })
            .take_while(|item| {
                !matches!(
                    item,
                    SidebarItem::SectionHeader {
                        section: Section::Tags,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(
            remote_items,
            vec![
                &SidebarItem::SectionHeader {
                    section: Section::Remote,
                    count: 3,
                    open: true,
                },
                &SidebarItem::RemoteGroup {
                    name: "backup".into(),
                    fetch_url: Some("/tmp/backup.git".into()),
                    count: 0,
                    collapsed: false,
                },
                &SidebarItem::Placeholder {
                    text: "no branches fetched yet".to_string(),
                    remote: "backup".into(),
                },
                &SidebarItem::RemoteGroup {
                    name: "origin".into(),
                    fetch_url: Some("https://example.invalid/repo.git".into()),
                    count: 3,
                    collapsed: false,
                },
                &SidebarItem::RemoteBranch {
                    name: "origin/feature".into(),
                    label: "feature".into(),
                    index: 0,
                    depth: 0,
                    local_name: Some("feature".into()),
                    existing: false,
                },
                &SidebarItem::RemoteBranch {
                    name: "origin/HEAD".into(),
                    label: "HEAD".into(),
                    index: 1,
                    depth: 0,
                    local_name: None,
                    existing: false,
                },
                &SidebarItem::RemoteBranch {
                    name: "origin/main".into(),
                    label: "main".into(),
                    index: 2,
                    depth: 0,
                    local_name: Some("main".into()),
                    existing: true,
                },
            ]
        );
    }

    #[test]
    fn a_collapsed_remote_group_contributes_only_its_heading() {
        let mut branches = Branches::default();
        branches.toggle_remote_group("origin".to_owned());

        let items = flatten_with(
            &branches,
            sidebar_data(&[], &["origin/main"], &[], &[]),
            &HashMap::new(),
            &[remote("origin", None)],
        );

        assert!(items.contains(&SidebarItem::RemoteGroup {
            name: "origin".into(),
            fetch_url: None,
            count: 1,
            collapsed: true,
        }));
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, SidebarItem::RemoteBranch { .. })),
            "a folded remote draws no branches"
        );
        assert!(
            !items
                .iter()
                .any(|item| item.is(&ItemKey::RemoteEmpty("origin".to_owned()))),
            "a folded remote draws no placeholder either"
        );
    }

    fn tree(names: &[&str]) -> Vec<RefNode<()>> {
        ref_tree(names.iter().map(|name| ((*name).to_owned(), ())).collect())
    }

    fn leaf_labels<T>(nodes: &[RefNode<T>]) -> Vec<&str> {
        nodes
            .iter()
            .map(|node| match node {
                RefNode::Leaf { label, .. } => label.as_str(),
                RefNode::Folder { segment, .. } => segment.as_str(),
            })
            .collect()
    }

    #[test]
    fn refs_sharing_a_path_segment_nest_under_a_folder() {
        let nodes = tree(&["t3/fast-blame", "t3/blame-view", "main"]);

        assert_eq!(leaf_labels(&nodes), ["main", "t3"]);
        let RefNode::Folder {
            path,
            segment,
            children,
        } = &nodes[1]
        else {
            panic!("the t3 refs belong in a folder");
        };
        assert_eq!(segment, "t3");
        assert_eq!(path, "t3");
        assert_eq!(leaf_labels(children), ["blame-view", "fast-blame"]);
    }

    #[test]
    fn deeper_segments_nest_inside_each_other() {
        let nodes = tree(&[
            "feature/auth/login",
            "feature/auth/logout",
            "feature/ui",
            "main",
        ]);

        assert_eq!(leaf_labels(&nodes), ["feature", "main"]);
        let RefNode::Folder { children, .. } = &nodes[0] else {
            panic!("feature is a folder");
        };
        assert_eq!(leaf_labels(children), ["auth", "ui"]);
        let RefNode::Folder { children: auth, .. } = &children[0] else {
            panic!("auth is a folder under feature");
        };
        assert_eq!(leaf_labels(auth), ["login", "logout"]);
    }

    #[test]
    fn a_folder_holding_one_branch_shows_the_whole_name_instead() {
        assert_eq!(
            tree(&["t3/blame-view"]),
            vec![RefNode::Leaf {
                label: "t3/blame-view".into(),
                path: "t3/blame-view".into(),
                value: (),
            }],
            "a one-branch folder would only hide the name"
        );
        assert_eq!(
            tree(&["a/b/c"]),
            vec![RefNode::Leaf {
                label: "a/b/c".into(),
                path: "a/b/c".into(),
                value: (),
            }],
            "a chain of one-branch folders collapses to one row"
        );
        // The folder exists once a sibling gives it something to group.
        let nodes = tree(&["a/b/c", "a/b/d"]);
        let RefNode::Folder { children, .. } = &nodes[0] else {
            panic!("a is a folder");
        };
        let RefNode::Folder {
            children: inner, ..
        } = &children[0]
        else {
            panic!("b is a folder under a");
        };
        assert_eq!(leaf_labels(inner), ["c", "d"]);
    }

    #[test]
    fn folders_and_leaves_keep_natural_order() {
        let nodes = tree(&[
            "topic-10",
            "topic-2",
            "feature-10/x",
            "feature-2/x",
            "zebra/x",
        ]);

        assert_eq!(
            leaf_labels(&nodes),
            [
                "feature-2/x",
                "feature-10/x",
                "topic-2",
                "topic-10",
                "zebra/x"
            ]
        );
    }

    #[test]
    fn flatten_draws_folder_rows_above_their_leaves() {
        let items = flatten(
            &Branches::default(),
            sidebar_data(&["t3/fast-blame", "main", "t3/blame-view"], &[], &[], &[]),
        );

        assert_eq!(
            items,
            vec![
                SidebarItem::SectionHeader {
                    section: Section::Local,
                    count: 3,
                    open: true,
                },
                SidebarItem::LocalBranch {
                    name: "main".into(),
                    label: "main".into(),
                    index: 0,
                    depth: 0,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                SidebarItem::Folder {
                    key: "local:t3".into(),
                    segment: "t3".into(),
                    count: 2,
                    open: true,
                    depth: 0,
                    section: Section::Local,
                },
                SidebarItem::LocalBranch {
                    name: "t3/blame-view".into(),
                    label: "blame-view".into(),
                    index: 1,
                    depth: 1,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                SidebarItem::LocalBranch {
                    name: "t3/fast-blame".into(),
                    label: "fast-blame".into(),
                    index: 2,
                    depth: 1,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                SidebarItem::SectionHeader {
                    section: Section::Remote,
                    count: 0,
                    open: true,
                },
                SidebarItem::SectionHeader {
                    section: Section::Tags,
                    count: 0,
                    open: true,
                },
                SidebarItem::SectionHeader {
                    section: Section::Stashes,
                    count: 0,
                    open: true,
                },
            ]
        );
    }

    #[test]
    fn remote_branches_fold_under_their_remote() {
        let items = flatten_with(
            &Branches::default(),
            sidebar_data(
                &[],
                &["origin/t3/a", "origin/t3/b", "origin/main"],
                &[],
                &[],
            ),
            &HashMap::new(),
            &[remote("origin", None)],
        );

        assert!(items.contains(&SidebarItem::Folder {
            key: "remote:origin:t3".into(),
            segment: "t3".into(),
            count: 2,
            open: true,
            depth: 0,
            section: Section::Remote,
        }));
        let labels: Vec<String> = items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::RemoteBranch { label, depth, .. } => Some(format!("{label}/{depth}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            ["main/0", "a/1", "b/1"],
            "the folder groups origin's t3 refs, and only those"
        );
    }

    #[test]
    fn a_folder_folds_like_a_section_and_stays_folded() {
        let mut branches = Branches::default();
        assert!(!branches.folder_folded("local:t3"));
        branches.toggle_folder("local:t3".into());

        let folded = |items: &[SidebarItem]| {
            items.iter().any(|item| {
                matches!(item, SidebarItem::Folder { key, open: false, .. } if key == "local:t3")
            })
        };
        let items = flatten(&branches, sidebar_data(&["t3/a", "t3/b"], &[], &[], &[]));
        assert!(folded(&items), "a folded folder starts closed");
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, SidebarItem::LocalBranch { .. })),
            "a folded folder draws no leaves"
        );

        let refreshed = flatten(
            &branches,
            sidebar_data(&["t3/a", "t3/b", "t3/c"], &[], &[], &[]),
        );
        assert!(
            folded(&refreshed),
            "the fold survives the snapshot that adds a ref"
        );

        branches.toggle_folder("local:t3".into());
        let open = flatten(
            &branches,
            sidebar_data(&["t3/a", "t3/b", "t3/c"], &[], &[], &[]),
        );
        assert!(open.contains(&SidebarItem::Folder {
            key: "local:t3".into(),
            segment: "t3".into(),
            count: 3,
            open: true,
            depth: 0,
            section: Section::Local,
        }));
        assert_eq!(
            open.iter()
                .filter(|item| matches!(item, SidebarItem::LocalBranch { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn the_current_branch_folder_opens_when_head_moves_into_it() {
        let mut branches = Branches::default();
        branches.toggle_folder("local:t3".into());
        let main = sidebar_data(&["main", "t3/a", "t3/b"], &[], &[], &[]);
        let items = flatten(&branches, main);
        assert!(
            items.iter().any(|item| matches!(
                item,
                SidebarItem::Folder { key, open: false, .. } if key == "local:t3"
            )),
            "the folder folds while HEAD is elsewhere"
        );

        let mut on_t3 = sidebar_data(&["main", "t3/a", "t3/b"], &[], &[], &[]);
        on_t3.head_branch = Some("t3/a".into());
        let items = flatten(&branches, on_t3);
        assert!(
            items.iter().any(|item| matches!(
                item,
                SidebarItem::Folder { key, open: true, .. } if key == "local:t3"
            )),
            "the checked-out branch's folder opens itself"
        );
        assert!(
            items.iter().any(
                |item| matches!(item, SidebarItem::LocalBranch { name, .. } if name == "t3/a")
            )
        );
    }

    #[test]
    fn a_filter_lists_matching_leaves_by_their_full_path_without_folders() {
        let mut branches = Branches::default();
        branches.set_filter(Some("t3/blame".into()));

        let items = flatten(
            &branches,
            sidebar_data(&["t3/blame-view", "t3/fast-blame", "main"], &[], &[], &[]),
        );

        assert!(
            !items
                .iter()
                .any(|item| matches!(item, SidebarItem::Folder { .. })),
            "a filter shows the leaves directly"
        );
        let rows: Vec<(String, String, usize)> = items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::LocalBranch {
                    name, label, depth, ..
                } => Some((name.clone(), label.clone(), *depth)),
                _ => None,
            })
            .collect();
        assert_eq!(
            rows,
            [("t3/blame-view".to_owned(), "t3/blame-view".to_owned(), 0)],
            "the match keeps its whole path and no indent"
        );
    }

    #[test]
    fn a_filter_hides_remotes_without_a_match() {
        let mut branches = Branches::default();
        branches.set_filter(Some("origin/t3/a".into()));

        let items = flatten_with(
            &branches,
            sidebar_data(
                &[],
                &["origin/t3/a", "origin/t3/b", "backup/main"],
                &[],
                &[],
            ),
            &HashMap::new(),
            &[],
        );

        assert!(!items.iter().any(|item| matches!(
            item,
            SidebarItem::RemoteGroup { name, .. } if name == "backup"
        )));
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::RemoteBranch { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["t3/a"]);
    }

    fn dragged(kind: RefKind, name: &str) -> DraggedBranch {
        DraggedBranch {
            kind,
            name: name.to_owned(),
        }
    }

    #[test]
    fn a_local_branch_dropped_on_another_offers_merge_and_rebase() {
        let entries = drop_entries(
            &dragged(RefKind::LocalBranch, "topic"),
            "main",
            Some("main"),
            false,
            None,
        )
        .expect("a drop between two branches has a menu");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].action, DropAction::Merge);
        assert_eq!(entries[0].label, "Merge topic into main");
        assert_eq!(entries[0].blocked, None);
        assert_eq!(entries[1].action, DropAction::Rebase);
        assert_eq!(entries[1].label, "Rebase main onto topic");
        assert_eq!(entries[1].blocked, None);
    }

    #[test]
    fn a_remote_branch_dropped_on_its_tracking_branch_also_offers_fast_forward() {
        let entries = drop_entries(
            &dragged(RefKind::RemoteBranch, "origin/topic"),
            "topic",
            Some("topic"),
            false,
            Some(true),
        )
        .expect("a remote drop has a menu");

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].action, DropAction::FastForward);
        assert_eq!(entries[2].label, "Fast-forward topic");
        assert_eq!(entries[2].blocked, None);

        let behind = drop_entries(
            &dragged(RefKind::RemoteBranch, "origin/topic"),
            "topic",
            Some("topic"),
            false,
            Some(false),
        )
        .expect("the menu still opens");
        assert_eq!(behind[2].blocked.as_deref(), Some("not a fast-forward"));

        let elsewhere = drop_entries(
            &dragged(RefKind::RemoteBranch, "origin/topic"),
            "main",
            Some("main"),
            false,
            Some(true),
        )
        .expect("a remote can be merged into another branch");
        assert_eq!(
            elsewhere.len(),
            2,
            "fast-forward only applies to the tracking branch"
        );
    }

    #[test]
    fn a_branch_dropped_on_itself_has_no_menu() {
        assert_eq!(
            drop_entries(
                &dragged(RefKind::LocalBranch, "main"),
                "main",
                Some("main"),
                false,
                None,
            ),
            None
        );
    }

    #[test]
    fn a_drop_on_a_branch_that_is_not_checked_out_explains_why() {
        let entries = drop_entries(
            &dragged(RefKind::LocalBranch, "topic"),
            "main",
            Some("other"),
            false,
            None,
        )
        .expect("the menu opens with disabled entries");
        assert_eq!(
            entries[0].blocked.as_deref(),
            Some("check out main first"),
            "a merge runs on the checked-out branch"
        );
        assert_eq!(entries[1].blocked.as_deref(), Some("check out main first"));

        let busy = drop_entries(
            &dragged(RefKind::LocalBranch, "topic"),
            "main",
            Some("main"),
            true,
            None,
        )
        .expect("the menu opens while an operation runs");
        assert_eq!(busy[0].blocked.as_deref(), Some("an operation is running"));
    }

    #[test]
    fn item_keys_are_unique_and_survive_a_refresh() {
        let branches = Branches::default();
        let before = flatten(&branches, sidebar_data(&["main"], &[], &["v1", "v2"], &[]));
        let after = flatten(
            &branches,
            sidebar_data(&["main"], &[], &["v0", "v1", "v2"], &[]),
        );

        let keys: std::collections::HashSet<ItemKey> =
            before.iter().map(SidebarItem::key).collect();
        assert_eq!(keys.len(), before.len(), "every row has a key of its own");

        let key = ItemKey::Tag("v2".to_owned());
        let old_index = before.iter().position(|item| item.is(&key)).unwrap();
        let new_index = after.iter().position(|item| item.is(&key)).unwrap();
        assert_eq!(
            new_index,
            old_index + 1,
            "a tag added above moves the row, not the key"
        );
    }

    #[test]
    fn a_refresh_keeps_the_row_at_the_top_of_the_viewport() {
        let branches = Branches::default();
        let tags: Vec<String> = (1..=10).map(|number| format!("v{number}")).collect();
        let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let before = Rc::new(flatten(&branches, sidebar_data(&[], &[], &tag_refs, &[])));
        branches.restore_scroll(&before);

        // Five rows down: past the two empty sections' headings and
        // placeholders, so `v1` is the first tag at the top.
        branches
            .scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.0), px(-140.0)));

        let mut inserted = vec!["v0".to_owned()];
        inserted.extend(tags);
        let inserted_refs: Vec<&str> = inserted.iter().map(String::as_str).collect();
        let after = Rc::new(flatten(
            &branches,
            sidebar_data(&[], &[], &inserted_refs, &[]),
        ));
        branches.restore_scroll(&after);

        assert_eq!(
            branches.scroll.0.borrow().base_handle.offset().y,
            px(-168.0),
            "the same tag must stay at the top after one is added above it"
        );
    }

    #[test]
    fn a_refresh_that_keeps_the_list_keeps_the_scroll_offset() {
        let branches = Branches::default();
        let items = Rc::new(flatten(&branches, sidebar_data(&["main"], &[], &[], &[])));
        branches.restore_scroll(&items);
        branches
            .scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.0), px(-56.0)));

        branches.restore_scroll(&items);

        assert_eq!(
            branches.scroll.0.borrow().base_handle.offset().y,
            px(-56.0),
            "an unchanged list must not move under the pointer"
        );
    }

    fn oid(byte: u8) -> strata_core::Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn submodule(name: &str, status: SubmoduleStatus) -> SubmoduleInfo {
        SubmoduleInfo {
            name: name.to_owned(),
            path: name.to_owned(),
            url: Some(format!("../{name}.git")),
            recorded_commit: Some(oid(1)),
            checked_out_commit: match status {
                SubmoduleStatus::Uninitialised => None,
                SubmoduleStatus::CommitDiffers => Some(oid(2)),
                SubmoduleStatus::UpToDate | SubmoduleStatus::Modified => Some(oid(1)),
            },
            status,
        }
    }

    fn worktree(
        path: &str,
        head: Option<HeadInfo>,
        is_main: bool,
        locked: bool,
        prunable: bool,
    ) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from(path),
            head,
            is_main,
            locked,
            lock_reason: locked.then(|| "testing".to_owned()),
            prunable,
        }
    }

    fn branch_head(name: &str) -> HeadInfo {
        HeadInfo::Branch {
            short_name: name.to_owned(),
            target: Some(oid(3)),
        }
    }

    #[test]
    fn a_repository_without_submodules_or_linked_worktrees_hides_both_sections() {
        let branches = Branches::default();
        let extras = SidebarExtras {
            revision: 4,
            ..SidebarExtras::default()
        };

        let items = flatten_extras(&branches, sidebar_data(&["main"], &[], &[], &[]), &extras);

        assert!(
            !items.iter().any(|item| matches!(
                item,
                SidebarItem::SectionHeader {
                    section: Section::Submodules | Section::Worktrees,
                    ..
                } | SidebarItem::Submodule { .. }
                    | SidebarItem::Worktree { .. }
            )),
            "an empty repository has no submodule or worktree heading to fold"
        );
    }

    #[test]
    fn the_main_worktree_alone_does_not_earn_a_section() {
        let branches = Branches::default();
        let main = [worktree(
            "/src/strata",
            Some(branch_head("main")),
            true,
            false,
            false,
        )];
        let main_only = SidebarExtras {
            worktrees: &main,
            current_worktree: Some(Path::new("/src/strata")),
            revision: 1,
            ..SidebarExtras::default()
        };
        let items = flatten_extras(
            &branches,
            sidebar_data(&["main"], &[], &[], &[]),
            &main_only,
        );
        assert!(!items.iter().any(|item| matches!(
            item,
            SidebarItem::SectionHeader {
                section: Section::Worktrees,
                ..
            } | SidebarItem::Worktree { .. }
        )));

        let both = [
            worktree("/src/strata", Some(branch_head("main")), true, false, false),
            worktree(
                "/src/strata-docs",
                Some(branch_head("docs")),
                false,
                false,
                false,
            ),
        ];
        let linked = SidebarExtras {
            worktrees: &both,
            current_worktree: Some(Path::new("/src/strata")),
            ..main_only
        };
        let items = flatten_extras(&branches, sidebar_data(&["main"], &[], &[], &[]), &linked);
        assert!(items.contains(&SidebarItem::SectionHeader {
            section: Section::Worktrees,
            count: 2,
            open: true,
        }));
        let current = items
            .iter()
            .find_map(|item| match item {
                SidebarItem::Worktree { current, path, .. } if path == "/src/strata" => {
                    Some(*current)
                }
                _ => None,
            })
            .expect("the main worktree row");
        assert!(current, "the window's own checkout is marked like HEAD");
    }

    #[test]
    fn submodule_rows_carry_the_state_their_badge_names() {
        let branches = Branches::default();
        let submodules = [
            submodule("libs/parser", SubmoduleStatus::UpToDate),
            submodule("libs/legacy", SubmoduleStatus::Uninitialised),
            submodule("libs/ui", SubmoduleStatus::CommitDiffers),
            submodule("libs/dirty", SubmoduleStatus::Modified),
        ];
        let extras = SidebarExtras {
            submodules: &submodules,
            revision: 1,
            ..SidebarExtras::default()
        };

        let items = flatten_extras(&branches, sidebar_data(&["main"], &[], &[], &[]), &extras);

        assert!(items.contains(&SidebarItem::SectionHeader {
            section: Section::Submodules,
            count: 4,
            open: true,
        }));
        let statuses: Vec<SubmoduleStatus> = items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::Submodule { status, .. } => Some(*status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            [
                SubmoduleStatus::UpToDate,
                SubmoduleStatus::Uninitialised,
                SubmoduleStatus::CommitDiffers,
                SubmoduleStatus::Modified,
            ]
        );
        assert_eq!(
            submodule_badge(SubmoduleStatus::UpToDate),
            None,
            "an up-to-date checkout needs no label"
        );
        assert_eq!(
            submodule_badge(SubmoduleStatus::Uninitialised)
                .map(|(label, _)| label)
                .as_deref(),
            Some("not initialised")
        );
        assert_eq!(
            submodule_badge(SubmoduleStatus::CommitDiffers)
                .map(|(label, _)| label)
                .as_deref(),
            Some("changed")
        );
        assert_eq!(
            submodule_badge(SubmoduleStatus::Modified)
                .map(|(label, _)| label)
                .as_deref(),
            Some("modified")
        );
    }

    #[test]
    fn a_submodule_tooltip_names_its_path_url_and_both_commits() {
        let detail = submodule_detail(&submodule("libs/ui", SubmoduleStatus::CommitDiffers));
        assert!(detail.starts_with("libs/ui\n../libs/ui.git"), "{detail}");
        assert!(detail.contains("recorded 0101010"), "{detail}");
        assert!(detail.contains("checked out 0202020"), "{detail}");

        let uninitialised =
            submodule_detail(&submodule("libs/legacy", SubmoduleStatus::Uninitialised));
        assert!(
            uninitialised.contains("checked out none"),
            "{uninitialised}"
        );
    }

    #[test]
    fn worktree_rows_label_their_branch_or_detached_commit_and_badge_their_state() {
        let branches = Branches::default();
        let worktrees = [
            worktree("/src/strata", Some(branch_head("main")), true, false, false),
            worktree(
                "/src/strata-docs",
                Some(branch_head("docs")),
                false,
                true,
                false,
            ),
            worktree(
                "/src/strata-gone",
                Some(HeadInfo::Detached { target: oid(0xab) }),
                false,
                false,
                true,
            ),
        ];
        let extras = SidebarExtras {
            worktrees: &worktrees,
            revision: 1,
            ..SidebarExtras::default()
        };

        let items = flatten_extras(&branches, sidebar_data(&["main"], &[], &[], &[]), &extras);

        let rows: Vec<&SidebarItem> = items
            .iter()
            .filter(|item| matches!(item, SidebarItem::Worktree { .. }))
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            worktree_label(&worktrees[0]),
            "main",
            "a branch checkout is named by its branch"
        );
        assert_eq!(
            worktree_label(&worktrees[2]),
            "detached abababa",
            "a detached checkout is named by its short commit"
        );
        let labels: Vec<String> = rows
            .iter()
            .map(|item| match item {
                SidebarItem::Worktree {
                    is_main,
                    locked,
                    prunable,
                    ..
                } => format!("{is_main}/{locked}/{prunable}"),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            labels,
            ["true/false/false", "false/true/false", "false/false/true"]
        );
        assert_eq!(
            worktree_detail(&worktrees[1]),
            "/src/strata-docs\nlocked: testing"
        );
        assert!(worktree_detail(&worktrees[2]).contains("missing"));
    }

    #[test]
    fn submodule_and_worktree_keys_survive_a_refresh() {
        let branches = Branches::default();
        let first = [
            submodule("a", SubmoduleStatus::UpToDate),
            submodule("b", SubmoduleStatus::UpToDate),
        ];
        let second = [
            submodule("0", SubmoduleStatus::UpToDate),
            submodule("a", SubmoduleStatus::UpToDate),
            submodule("b", SubmoduleStatus::UpToDate),
        ];
        let worktrees = [
            worktree("/src/strata", Some(branch_head("main")), true, false, false),
            worktree(
                "/src/strata-docs",
                Some(branch_head("docs")),
                false,
                false,
                false,
            ),
        ];
        let before_extras = SidebarExtras {
            submodules: &first,
            worktrees: &worktrees,
            revision: 1,
            ..SidebarExtras::default()
        };
        let after_extras = SidebarExtras {
            submodules: &second,
            worktrees: &worktrees,
            revision: 2,
            ..SidebarExtras::default()
        };
        let data = || sidebar_data(&["main"], &[], &[], &[]);
        let before = flatten_extras(&branches, data(), &before_extras);
        let after = flatten_extras(&branches, data(), &after_extras);

        let key = ItemKey::Submodule("b".to_owned());
        let old_index = before.iter().position(|item| item.is(&key)).unwrap();
        let new_index = after.iter().position(|item| item.is(&key)).unwrap();
        assert_eq!(
            new_index,
            old_index + 1,
            "a submodule added above moves the row, not the key"
        );

        let keys: std::collections::HashSet<ItemKey> = after.iter().map(SidebarItem::key).collect();
        assert_eq!(keys.len(), after.len(), "every row has a key of its own");
    }

    #[test]
    fn the_item_cache_follows_the_submodule_and_worktree_answer() {
        let branches = Branches::default();
        let submodules = [submodule("libs/ui", SubmoduleStatus::UpToDate)];
        let worktrees = [
            worktree("/src/strata", Some(branch_head("main")), true, false, false),
            worktree(
                "/src/strata-docs",
                Some(branch_head("docs")),
                false,
                false,
                false,
            ),
        ];
        let upstreams = HashMap::new();
        let design = Design::default();

        let first = SidebarExtras {
            submodules: &submodules,
            worktrees: &worktrees,
            revision: 1,
            ..SidebarExtras::default()
        };
        let cached = branches.items(
            sidebar_data(&["main"], &[], &[], &[]),
            3,
            &upstreams,
            &[],
            &first,
            &design,
        );
        let again = branches.items(
            sidebar_data(&["main"], &[], &[], &[]),
            3,
            &upstreams,
            &[],
            &first,
            &design,
        );
        assert!(
            Rc::ptr_eq(&cached, &again),
            "an unchanged snapshot and answer reuse the flattened list"
        );

        let mut updated = SidebarExtras { ..first };
        updated.revision = 2;
        let rebuilt = branches.items(
            sidebar_data(&["main"], &[], &[], &[]),
            3,
            &upstreams,
            &[],
            &updated,
            &design,
        );
        assert!(
            !Rc::ptr_eq(&cached, &rebuilt),
            "a new submodule/worktree answer rebuilds the sections"
        );
    }

    #[test]
    fn extra_selection_and_reveal_exclude_each_other() {
        let mut branches = Branches::default();
        branches.reveal_ref(RefKind::LocalBranch, "main");
        assert!(branches.reveal.is_ref(RefKind::LocalBranch, "main"));

        branches.select_extra(ExtraSelection::Submodule("libs/ui".into()));
        assert_eq!(
            branches.selected_extra(),
            Some(&ExtraSelection::Submodule("libs/ui".into()))
        );
        assert!(
            !branches.reveal.is_ref(RefKind::LocalBranch, "main"),
            "selecting an extra row clears any revealed ref"
        );

        branches.reveal_ref(RefKind::Tag, "v1.0");
        assert_eq!(
            branches.selected_extra(),
            None,
            "revealing a ref clears any selected submodule/worktree"
        );

        branches.select_extra(ExtraSelection::Worktree(PathBuf::from("/src/strata-docs")));
        branches.reveal_stash(0);
        assert_eq!(branches.selected_extra(), None);
        assert!(branches.reveal.is_stash(0));
    }

    #[gpui::test]
    fn narrow_sidebar_rows_truncate_and_keep_trailing_markers_clear_of_scrollbar(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::cell::RefCell;

        use gpui::{Bounds, VisualTestContext};
        use gpui_component::Root;
        use strata_core::forge::{
            ChecksState, PullRequestChecks, PullRequestState, PullRequestSummary, ReviewState,
        };

        use crate::Launch;

        fn test_colors() -> Colors {
            let design = Design::default();
            Colors {
                sidebar_foreground: design.text,
                muted_foreground: design.muted,
                border: design.border,
                foreground: design.text,
                danger: design.remove_text,
                overlay: design.panel,
                design,
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let config = temp.path().join("gitconfig");
        std::fs::write(&config, "").expect("write gitconfig");
        let dir = temp.path().join("repo");
        std::fs::create_dir_all(&dir).expect("repo dir");
        let init = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .env("GIT_CONFIG_GLOBAL", &config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git init");
        assert!(init.status.success());

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(crate::settings::Settings::default());
            crate::actions::bind(cx);
        });

        let repo_win = cx
            .update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    cx.new(|cx| RepoView::new(Launch::open_quiet(&dir, None), window, cx))
                })
            })
            .expect("repo window");
        let repo_view = repo_win.root(cx).expect("repo root");

        let long_branch = "feature/very-long-branch-name-that-must-ellipsize-in-the-narrow-sidebar";
        let pr = PullRequestSummary {
            number: 42,
            title: "Long PR".into(),
            state: PullRequestState::Open,
            head_branch: long_branch.into(),
            head_owner: None,
            base: "main".into(),
            url: "https://example.invalid/pr/42".into(),
            review: ReviewState::Approved,
            checks: PullRequestChecks {
                state: ChecksState::Passing,
                passing: 1,
                failing: 0,
                pending: 0,
            },
            updated_at: "2026-01-01T00:00:00Z".into(),
        };

        struct Probe {
            repo: Entity<RepoView>,
            pr: PullRequestSummary,
            row_bounds: Rc<RefCell<Vec<Bounds<Pixels>>>>,
        }
        impl gpui::Render for Probe {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let colors = test_colors();
                let slot = self.row_bounds.clone();
                let list_width = px(152.0);
                div()
                    .w(list_width)
                    .h(px(200.0))
                    .flex()
                    .flex_col()
                    .child(local_row(
                        "feature/extremely-long-branch-with-ahead-behind-counters-and-pr",
                        "feature/extremely-long-branch-with-ahead-behind-counters-and-pr",
                        0,
                        0,
                        false,
                        Some("↑2 ↓1"),
                        false,
                        None,
                        Some(&self.pr),
                        None,
                        false,
                        true,
                        false,
                        false,
                        &colors,
                        self.repo.clone(),
                    ))
                    .child(local_row(
                        "feature/extremely-long-branch-with-deleted-upstream-and-pr",
                        "feature/extremely-long-branch-with-deleted-upstream-and-pr",
                        1,
                        0,
                        false,
                        None,
                        true,
                        Some("The upstream branch was deleted on the remote".to_owned()),
                        Some(&self.pr),
                        None,
                        false,
                        true,
                        false,
                        false,
                        &colors,
                        self.repo.clone(),
                    ))
                    .child(local_row(
                        "feature/extremely-long-branch-with-shallow-question-mark-and-pr",
                        "feature/extremely-long-branch-with-shallow-question-mark-and-pr",
                        2,
                        0,
                        false,
                        Some("?"),
                        false,
                        Some("Counts unavailable: history is shallow".to_owned()),
                        Some(&self.pr),
                        None,
                        false,
                        true,
                        false,
                        false,
                        &colors,
                        self.repo.clone(),
                    ))
                    // A folder row draws in the same narrow list without
                    // overflowing or painting over its own hover.
                    .child(folder_row(
                        "local:feature",
                        "feature",
                        3,
                        true,
                        0,
                        Section::Local,
                        &colors,
                        self.repo.clone(),
                    ))
                    .on_children_prepainted(move |children, _, _| {
                        *slot.borrow_mut() = children;
                    })
            }
        }

        let row_bounds: Rc<RefCell<Vec<Bounds<Pixels>>>> = Rc::new(RefCell::new(Vec::new()));
        let probe_bounds = row_bounds.clone();
        let probe_repo = repo_view.clone();
        let probe_win = cx
            .update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    let probe = cx.new(|_| Probe {
                        repo: probe_repo,
                        pr,
                        row_bounds: probe_bounds,
                    });
                    cx.new(|cx| Root::new(probe, window, cx))
                })
            })
            .expect("probe window");
        let mut vcx = VisualTestContext::from_window(probe_win.into(), cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let rows = row_bounds.borrow();
        assert_eq!(rows.len(), 4);
        for (i, bounds) in rows.iter().enumerate() {
            assert!(
                bounds.size.width <= px(152.5),
                "row {i} expanded past narrow sidebar width: {:?}",
                bounds.size.width
            );
        }

        // The PR badge sits at the far right of each row (after the upstream
        // count / `?` / unlink icon) and paints an accent-soft pill quad.
        // Checking every painted pill quad guarantees both the upstream marker
        // and the PR badge stay inside the row and clear of the 16 px overlay
        // vertical scrollbar.
        let badge_bg: gpui::Background = test_colors().design.accent_soft.into();
        vcx.update(|window, _| {
            let scale = window.scale_factor();
            let badge_quads: Vec<_> = window
                .painted_quads()
                .into_iter()
                .filter(|quad| quad.background == badge_bg)
                .collect();
            assert_eq!(
                badge_quads.len(),
                3,
                "all three trailing PR badges (and the upstream markers before them) must remain visible"
            );
            for (i, quad) in badge_quads.iter().enumerate() {
                let row_right = rows[i].right();
                let badge_right = px(quad.bounds.right().as_f32() / scale);
                assert!(
                    badge_right <= row_right - px(16.0),
                    "row {i} trailing content ({badge_right:?}) overlapped the 16px scrollbar zone (row right {row_right:?})"
                );
            }
        });
    }
}
