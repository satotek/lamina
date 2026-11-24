//! Branch management: the sidebar's ref sections, their context menus, and the
//! create/delete dialogs.
//!
//! Nothing here runs git. Every write is an [`Operation`] handed to the
//! window's [`RepoView`], which gives it to the session worker; the sections
//! only read the snapshot the view has already published. The helpers that do
//! not need a window (name validation, upstream labels, remote-to-local name
//! mapping) live in [`refs`] and [`validate`] and are unit tested without a
//! repository.
//!
//! [`tree`] flattens the sections into the virtualised list and folds shared
//! path segments into folders; [`rows`] draws one line of it; [`dialogs`] owns
//! the forms. The submodule and worktree sections are part of the same list,
//! and their data comes from the view's last submodule/worktree answer rather
//! than from the snapshot, so the list's cache key carries that answer's
//! generation.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, Entity, FocusHandle, Hsla, InteractiveElement, IntoElement, ParentElement,
    Pixels, Size, Styled, UniformListScrollHandle, Window, actions, div, uniform_list,
};
use lamina_core::repo::{SubmoduleInfo, WorktreeInfo};
use lamina_core::session::OperationId;
use lamina_core::{RefKind, RemoteInfo, UpstreamInfo};

use crate::data::SidebarData;
use crate::pull_requests::BranchPullRequests;
use crate::repo_view::RepoView;
use crate::scrollbar::WithScrollbar as _;
use crate::style::{self, Design};

use dialogs::Dialog;
use refs::{is_dirty_worktree, is_not_fully_merged};
use rows::{DropMenu, item_element};
use tree::{ItemCache, ScrollAnchor};

// The sidebar is split into the parts a reader looks for: the flattened list,
// the rows it draws, the branch forms, and the helper tables they share.
mod dialogs;
mod operations;
mod refs;
mod rows;
mod tree;
mod validate;

pub(crate) use dialogs::{DialogRequest, offer_force, worktree_name};
pub(crate) use operations::checkout_remote;
pub(crate) use refs::{local_name_for_remote, upstream_label, upstream_note};
pub(crate) use rows::branch_drop_target_with;
pub(crate) use validate::validate_branch_name;

actions!(
    lamina,
    [
        CreateBranchFromHead,
        ConfirmBranchDialog,
        CancelBranchDialog,
    ]
);

/// Height of one line of the sidebar list. `uniform_list` lays every item out
/// at the height it measured once, so headers, rows and placeholders must all
/// be this tall; it matches [`crate::ui::list_row`]'s row height.
const ITEM_HEIGHT: f32 = 28.0;

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

    /// The revealed ref when it is a local branch.
    fn local_branch(&self) -> Option<&str> {
        match &self.revealed {
            Some(RevealedRef::Ref(RefKind::LocalBranch, name)) => Some(name.as_str()),
            _ => None,
        }
    }
}

/// A selectable submodule or worktree row in the sidebar.
///
/// Selecting one highlights it and lets `Enter` open it in the current window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExtraSelection {
    Submodule(String),
    Worktree(PathBuf),
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

impl Branches {
    /// The local branch the sidebar revealed last, if any. The palette's
    /// three-way compare command acts on it, like the branch row's menu does.
    pub(crate) fn revealed_local_branch(&self) -> Option<&str> {
        self.reveal.local_branch()
    }

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
        self.restore_scroll(&items, &colors.design);
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "remove worktree lamina-topic".to_owned(),
            Some(Retry::RemoveWorktree {
                path: PathBuf::from("/src/lamina-topic"),
                forced: false,
            }),
        );
        let label = branches.finish(
            OperationId(9),
            Some(
                "fatal: '/src/lamina-topic' contains modified or untracked files, \
                 use --force to delete it",
            ),
        );
        assert_eq!(label.as_deref(), Some("remove worktree lamina-topic"));
        assert_eq!(
            branches.take_force_retry(),
            Some(Retry::RemoveWorktree {
                path: PathBuf::from("/src/lamina-topic"),
                forced: true,
            })
        );
    }

    #[test]
    fn a_missing_worktree_checkout_is_pruned_not_forced() {
        let mut branches = Branches::default();
        branches.start(
            OperationId(10),
            "remove worktree lamina-gone".to_owned(),
            Some(Retry::RemoveWorktree {
                path: PathBuf::from("/src/lamina-gone"),
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

        branches.select_extra(ExtraSelection::Worktree(PathBuf::from("/src/lamina-docs")));
        branches.reveal_stash(0);
        assert_eq!(branches.selected_extra(), None);
        assert!(branches.reveal.is_stash(0));
    }
}
