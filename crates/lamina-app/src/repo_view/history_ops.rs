//! History actions on a commit: cherry-pick, revert, reset, tags and edits.
//!
//! The commit list's context menu, the tag form and the commit-editing rules
//! live here. Nothing in this module runs git: every write becomes an
//! [`Operation`] handed to [`RepoView::run_operation`], whose session worker
//! runs it and publishes the new snapshot. An operation that stops on a
//! conflict is a normal outcome (the conflict banner takes over from the
//! published state), so the handlers only report failures.
//!
//! The menu's enable rules depend on the commit, HEAD and the repository state
//! alone, so they are plain functions that the unit tests exercise without a
//! repository.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    App, ClickEvent, ClipboardItem, Context, Entity, EntityId, Focusable as _, IntoElement,
    ParentElement, Render, Styled, Subscription, WeakEntity, Window, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use lamina_core::git_cli::{
    AcrossMergesAction, MoveDirection, RebaseAction, RebaseStep, drop_commit, move_commit,
    reword_commit, squash_commit_into_parent,
};
use lamina_core::session::{
    Operation, RepoLocation, RepoReader, RepoSnapshot, RepositorySession, ResetMode,
};
use lamina_core::{ChangeKind, Error, HeadInfo, Oid, RefKind, RepoState, Result, WorktreeStatus};

use super::RepoView;
use super::commit_selection::{self, MultiPickRow};
use super::edit_dialog;
use super::rebase_dialog::{self, RebaseRequest};
use super::rebase_onto::rebase_menu_item;
use crate::data::short_oid;
use crate::settings::Settings;
use crate::style::Design;
use crate::ui;
use crate::ui::menu_label;

/// The history commands a commit row can offer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HistoryAction {
    CherryPick,
    Revert,
    Reset,
    CreateTag,
    DeleteTag,
    /// Branch creation is not a history edit; it only needs the commit.
    CreateBranch,
    /// Leave the current branch and point HEAD at the commit. Works while
    /// HEAD is already detached; it does not need a branch to move.
    CheckoutCommit,
}

/// What the history menu may offer right now, read from one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuState {
    /// Short name of the branch HEAD is on; `None` when HEAD is detached or the
    /// repository has no commit yet.
    branch: Option<String>,
    /// A merge, rebase, cherry-pick or revert git left half-done.
    pending: bool,
    /// Another operation is running.
    busy: bool,
    /// Commit HEAD points at, so picking or reverting it can be disabled.
    head: Option<Oid>,
    /// Tracked staged or worktree changes. Core refuses a rebase with them.
    dirty: bool,
    /// The repository is on another machine. Supported writes ignore this;
    /// tags, rewrites and other helper gaps still read it.
    ssh: bool,
}

impl MenuState {
    pub(crate) fn read(head: Option<&HeadInfo>, state: RepoState, busy: bool) -> Self {
        let (branch, commit) = match head {
            Some(HeadInfo::Branch { short_name, target }) => (Some(short_name.clone()), *target),
            Some(HeadInfo::Detached { target }) => (None, Some(*target)),
            None => (None, None),
        };
        Self {
            branch,
            pending: state != RepoState::Normal,
            busy,
            head: commit,
            dirty: false,
            ssh: false,
        }
    }

    /// Marks every history write unavailable. `read` stays about the snapshot.
    pub(crate) fn with_ssh(mut self, ssh: bool) -> Self {
        self.ssh = ssh;
        self
    }

    /// Adds the worktree state the snapshot carries; `read` stays about HEAD.
    pub(crate) fn with_worktree(mut self, status: &WorktreeStatus) -> Self {
        self.dirty = !status.staged.is_empty()
            || status
                .unstaged
                .iter()
                .any(|change| change.kind != ChangeKind::Untracked);
        self
    }

    /// What reset, cherry-pick and revert name: the branch, or HEAD when there
    /// is no branch to move.
    pub(crate) fn branch_label(&self) -> String {
        self.branch.clone().unwrap_or_else(|| "HEAD".to_owned())
    }

    /// The branch HEAD is on, or `None` when it is detached or there is no
    /// commit yet.
    pub(crate) fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    /// A merge, rebase, cherry-pick or revert git left half-done.
    pub(crate) fn pending(&self) -> bool {
        self.pending
    }

    /// Another operation is running.
    pub(crate) fn busy(&self) -> bool {
        self.busy
    }

    /// The reason every history-changing entry shares right now, shown once at
    /// the top of the commit menu instead of after each disabled entry.
    ///
    /// A running operation is reported before a half-finished one: it is the
    /// state that will change on its own.
    pub(crate) fn shared_reason(&self) -> Option<String> {
        if self.busy {
            Some(t!("branch.reason.busy").to_string())
        } else if self.pending {
            Some(t!("reflog.finish_first").to_string())
        } else {
            None
        }
    }

    /// Tracked staged or worktree changes.
    pub(crate) fn dirty(&self) -> bool {
        self.dirty
    }

    /// HEAD has a commit but is not on a branch.
    pub(crate) fn detached(&self) -> bool {
        self.branch.is_none() && self.head.is_some()
    }

    /// Why `action` cannot run, or `None` when it can.
    ///
    /// A tag can be created or deleted while another operation is in progress
    /// (it only writes a ref), but not while git is busy with one.
    pub(crate) fn blocked(&self, action: HistoryAction) -> Option<String> {
        // Tags stay off the wire. The remote reason wins over busy, the same
        // way a missing helper used to win over every other state.
        if self.ssh && matches!(action, HistoryAction::CreateTag | HistoryAction::DeleteTag) {
            return Some(crate::remote_open::unavailable_reason());
        }
        if self.busy {
            return Some(t!("branch.reason.busy").to_string());
        }
        match action {
            HistoryAction::CreateTag | HistoryAction::DeleteTag | HistoryAction::CreateBranch => {
                None
            }
            // Detaching does not need a branch, including when HEAD is already
            // detached. It still has to wait out a merge or rebase.
            HistoryAction::CheckoutCommit if self.pending => {
                Some(t!("reflog.finish_first").to_string())
            }
            HistoryAction::CheckoutCommit if self.head.is_none() => {
                Some(t!("history_ops.no_branch_to_move").to_string())
            }
            HistoryAction::CheckoutCommit => None,
            _ if self.pending => Some(t!("reflog.finish_first").to_string()),
            _ if self.branch.is_none() && self.head.is_some() => {
                Some(t!("reflog.head_detached").to_string())
            }
            _ if self.branch.is_none() => Some(t!("history_ops.no_branch_to_move").to_string()),
            _ => None,
        }
    }

    /// [`Self::blocked`] plus the commit-specific rule: picking or reverting
    /// HEAD itself has nothing to apply.
    pub(crate) fn blocked_for(&self, action: HistoryAction, commit: Oid) -> Option<String> {
        if self.ssh && matches!(action, HistoryAction::CreateTag | HistoryAction::DeleteTag) {
            return Some(crate::remote_open::unavailable_reason());
        }
        if matches!(action, HistoryAction::CherryPick | HistoryAction::Revert)
            && self.head == Some(commit)
        {
            return Some(t!("history_ops.reason.is_head").to_string());
        }
        self.blocked(action)
    }
}

/// A menu entry's label. A `reason` the menu already shows once at the top
/// (`shared`) is not repeated after every entry.
pub(crate) fn blocked_label(
    base: impl Into<String>,
    reason: Option<&str>,
    shared: Option<&str>,
) -> String {
    match reason {
        Some(reason) if Some(reason) != shared => menu_label(base, Some(reason)),
        _ => base.into(),
    }
}

/// The warning a rewrite's confirmation shows when the edited commit is already
/// on the branch's upstream: publishing the rewritten commit needs a force
/// push. Non-blocking, because the rewrite is still exactly what was asked for.
pub(crate) fn published_warning(upstream: Option<&str>) -> Option<String> {
    upstream
        .map(|upstream| t!("history_ops.pushed_commit_warning", upstream = upstream).to_string())
}

/// Appends [`published_warning`] to a confirmation body, separated by a blank
/// line so it reads as its own note.
fn with_pushed_warning(detail: String, upstream: Option<&str>) -> String {
    match published_warning(upstream) {
        Some(warning) => format!("{detail}\n\n{warning}"),
        None => detail,
    }
}

/// The commit rewrites the context menu can run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EditAction {
    Reword,
    Squash,
    Fixup,
    Drop,
    MoveUp,
    MoveDown,
}

impl EditAction {
    /// The menu entry, without the reason a disabled one shows.
    fn label(self) -> String {
        match self {
            Self::Reword => t!("history_ops.menu.reword"),
            Self::Squash => t!("history_ops.menu.squash"),
            Self::Fixup => t!("history_ops.menu.fixup"),
            Self::Drop => t!("history_ops.menu.drop"),
            Self::MoveUp => t!("history_ops.menu.move_up"),
            Self::MoveDown => t!("history_ops.menu.move_down"),
        }
        .to_string()
    }

    /// What the status bar and the command log call the operation.
    fn verb(self) -> String {
        match self {
            Self::Reword => t!("history_ops.op.reword"),
            Self::Squash => t!("history_ops.op.squash"),
            Self::Fixup => t!("history_ops.op.fixup"),
            Self::Drop => t!("history_ops.op.drop"),
            Self::MoveUp => t!("history_ops.op.move_up"),
            Self::MoveDown => t!("history_ops.op.move_down"),
        }
        .to_string()
    }

    /// The confirmation title; `short` is the commit id.
    fn title(self, short: &str) -> String {
        match self {
            Self::Reword => t!("history_ops.confirm.reword", short = short).to_string(),
            Self::Squash => t!("history_ops.confirm.squash", short = short).to_string(),
            Self::Fixup => t!("history_ops.confirm.fixup", short = short).to_string(),
            Self::Drop => t!("history_ops.confirm.drop", short = short).to_string(),
            Self::MoveUp => t!("history_ops.confirm.move_up", short = short).to_string(),
            Self::MoveDown => t!("history_ops.confirm.move_down", short = short).to_string(),
        }
    }
}

/// One row of the "Edit" submenu: a commit rewrite or the whole-range dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EditEntry {
    Action(EditAction),
    History,
}

impl EditEntry {
    /// Every row, in menu order.
    const ALL: [Self; 7] = [
        Self::Action(EditAction::Reword),
        Self::Action(EditAction::Squash),
        Self::Action(EditAction::Fixup),
        Self::Action(EditAction::Drop),
        Self::Action(EditAction::MoveUp),
        Self::Action(EditAction::MoveDown),
        Self::History,
    ];

    /// The menu entry, without the reason a disabled one shows.
    fn label(self) -> String {
        match self {
            Self::Action(action) => action.label(),
            Self::History => t!("history_ops.menu.edit_history").to_string(),
        }
    }
}

/// One commit of HEAD's first-parent line and its parents, newest first.
type LineCommit = (Oid, Vec<Oid>);

/// How far the line walk goes before an unseen commit is called off the line.
/// The path from HEAD to a visible commit is far shorter in practice.
const LINE_LIMIT: usize = 10_000;

/// How many ancestry nodes one remote reachability check visits. A remote tip
/// near HEAD reaches the edited range in a handful of steps; the cap keeps a
/// remote on an unrelated history from walking the whole repository.
const REMOTE_WALK_LIMIT: usize = 20_000;

pub(super) fn head_target(head: Option<&HeadInfo>) -> Option<Oid> {
    match head? {
        HeadInfo::Branch { target, .. } => *target,
        HeadInfo::Detached { target } => Some(*target),
    }
}

/// Walks HEAD's first parents until `target` is found, then two commits
/// further so every action's `onto` commit is in the line. `None` when a commit
/// cannot be read before the walk knows whether `target` is on the line.
fn walk_first_parents(
    head: Oid,
    target: Oid,
    mut parents: impl FnMut(Oid) -> Option<Vec<Oid>>,
) -> Option<(Vec<LineCommit>, Option<usize>)> {
    let mut line = Vec::new();
    let mut current = head;
    let mut seen = HashSet::new();
    let mut distance = None;
    while line.len() < LINE_LIMIT {
        if !seen.insert(current) {
            return None;
        }
        let commit_parents = parents(current)?;
        line.push((current, commit_parents.clone()));
        if current == target {
            distance = Some(line.len() - 1);
        }
        if let Some(distance) = distance
            && line.len() > distance + 2
        {
            return Some((line, Some(distance)));
        }
        match commit_parents.first() {
            Some(parent) => current = *parent,
            None => break,
        }
    }
    Some((line, distance))
}

fn snapshot_parents(snapshot: &RepoSnapshot, id: Oid) -> Option<Vec<Oid>> {
    let index = *snapshot.commit_index.get(&id)?;
    let (commit, _) = snapshot.commits.get(index)?;
    Some(commit.parents.clone())
}

/// Whether a session answers from the wire instead of this machine.
///
/// A remote read is an SSH round trip, so it must not run on the UI thread;
/// the menu rules below only read what the snapshot already carries for one.
fn is_remote(session: &dyn RepositorySession) -> bool {
    matches!(session.location(), RepoLocation::Ssh { .. })
}

/// The parents of `id` for a UI-thread ancestry walk.
///
/// The loaded snapshot comes first, and only a local session may fall back to
/// the repository: reading a commit from there is cheap through gix on this
/// machine, but on an ssh tab it would hold the frame the menu opens in until
/// the host answers.
fn parents_in(
    snapshot: &RepoSnapshot,
    remote: bool,
    id: Oid,
    read: impl FnOnce() -> Option<Vec<Oid>>,
) -> Option<Vec<Oid>> {
    snapshot_parents(snapshot, id).or_else(|| (!remote).then(read).flatten())
}

/// Every commit HEAD can reach, capped like the remote check: a menu only needs
/// to know whether any selected commit is already on the branch.
fn head_reachable_set(session: &dyn RepositorySession) -> HashSet<Oid> {
    let snapshot = session.snapshot();
    let Some(head) = head_target(snapshot.head.as_ref()) else {
        return HashSet::new();
    };
    let repo = session.reader();
    let remote = is_remote(session);
    let mut reachable = HashSet::new();
    let mut stack = vec![head];
    while let Some(id) = stack.pop() {
        if !reachable.insert(id) {
            continue;
        }
        if reachable.len() > REMOTE_WALK_LIMIT {
            break;
        }
        let parents = parents_in(&snapshot, remote, id, || repo.parents(id).ok());
        if let Some(parents) = parents {
            stack.extend(parents);
        }
    }
    reachable
}

/// Everything the edit entries need about one commit, read once when the menu
/// opens. The rules are methods on plain data so the tests can build a line
/// without a repository.
#[derive(Clone)]
pub(crate) struct EditContext {
    /// HEAD's first-parent line, newest first, far enough for every action.
    line: Vec<LineCommit>,
    /// Where the target sits on the line; `None` when it is not on it.
    distance: Option<usize>,
    /// Whether HEAD reaches the target at all. A commit a merge brought in is
    /// reachable without being on the first-parent line, which the entries
    /// explain differently from a commit on another branch.
    reachable: bool,
    /// The shape around a commit a merge brought in, when it is one.
    side: Option<SideView>,
    head: Oid,
    /// Remote-tracking refs, as `(short name, tip)`.
    remotes: Vec<(String, Oid)>,
    snapshot: Arc<RepoSnapshot>,
    repo: Arc<dyn RepoReader>,
    /// The session answers from the wire, so this context may only read what
    /// the snapshot carries.
    remote: bool,
}

impl EditContext {
    fn read(session: &dyn RepositorySession, commit: Oid) -> Option<Self> {
        let snapshot = session.snapshot();
        let head = head_target(snapshot.head.as_ref())?;
        let repo = session.reader();
        let remote = is_remote(session);
        let (line, distance) = walk_first_parents(head, commit, |id| {
            parents_in(&snapshot, remote, id, || repo.parents(id).ok())
        })
        .unwrap_or_default();
        let remotes = snapshot
            .refs
            .iter()
            .filter(|reference| reference.kind == RefKind::RemoteBranch)
            .map(|reference| (reference.short_name.clone(), reference.target))
            .collect();
        let mut context = Self {
            line,
            distance,
            reachable: distance.is_some(),
            side: None,
            head,
            remotes,
            snapshot,
            repo,
            remote,
        };
        // The walk only follows first parents, so a commit behind a merge needs
        // the full ancestry check to tell it apart from one on another branch.
        if !context.reachable {
            context.reachable = context.reaches(commit, head);
            if context.reachable {
                context.side = context.side_view(commit);
            }
        }
        Some(context)
    }

    /// What the enable rules read, for [`blocked_on_line`].
    fn view(&self) -> LineView<'_> {
        LineView {
            line: &self.line,
            distance: self.distance,
            reachable: self.reachable,
            side: self.side,
        }
    }

    /// Reads what the rules need about a merged-in commit: whether it and its
    /// parent are merges, and whether the parent is already back on HEAD's
    /// line, where the merged branch forked.
    fn side_view(&self, commit: Oid) -> Option<SideView> {
        let parents_of = |id: Oid| self.parents_of(id);
        let parents = parents_of(commit)?;
        let parent = parents.first().copied();
        Some(SideView {
            is_merge: parents.len() > 1,
            parent_is_merge: parent
                .and_then(parents_of)
                .is_some_and(|grandparents| grandparents.len() > 1),
            parent_on_line: parent
                .is_none_or(|parent| self.line.iter().any(|(id, _)| *id == parent)),
        })
    }

    /// The `onto` commit and the commits `onto..HEAD` oldest first, which is
    /// what core's helpers and `git rebase -i` expect.
    fn plan(&self, action: EditAction) -> Option<(Oid, Vec<Oid>)> {
        let distance = self.distance?;
        let onto_index = onto_index(distance, action)?;
        let onto = self.line.get(onto_index)?.0;
        Some((onto, range_commits(&self.line, onto_index)))
    }

    /// Parents for the merge checks, from the snapshot or a local repository.
    fn parents_of(&self, id: Oid) -> Option<Vec<Oid>> {
        parents_in(&self.snapshot, self.remote, id, || {
            self.repo.parents(id).ok()
        })
    }

    /// The first line of `commit`'s message, from the loaded rows when the
    /// walk has them and from the repository otherwise.
    fn summary(&self, commit: Oid) -> String {
        if let Some(&index) = self.snapshot.commit_index.get(&commit)
            && let Some((info, _)) = self.snapshot.commits.get(index)
        {
            return info.summary.clone();
        }
        if self.remote {
            return short_oid(commit);
        }
        self.repo
            .commit_message(commit)
            .ok()
            .and_then(|message| message.lines().next().map(str::to_owned))
            .unwrap_or_else(|| short_oid(commit))
    }

    /// The branch's upstream when it already contains `commit`, so rewriting
    /// the commit needs a force push.
    fn upstream_containing(&self, commit: Oid) -> Option<String> {
        let HeadInfo::Branch { short_name, .. } = self.snapshot.head.as_ref()? else {
            return None;
        };
        let name = match self.snapshot.upstreams.get(short_name) {
            Some(upstream) => upstream.name.clone(),
            // A narrowed snapshot can leave the upstream out; the branch's
            // configuration still names it, and the tracking ref resolves the
            // tip directly.
            None => {
                let upstream = self.repo.branch_upstream_target(short_name).ok()??;
                format!("{}/{}", upstream.remote, upstream.branch)
            }
        };
        let tip = self
            .snapshot
            .refs
            .iter()
            .find(|reference| {
                reference.kind == RefKind::RemoteBranch && reference.short_name == name
            })
            .map(|reference| reference.target)
            .or_else(|| self.repo.resolve_commit(&name).ok().flatten())?;
        let published = self
            .repo
            .is_ancestor(commit, tip)
            .unwrap_or_else(|_| self.reaches(commit, tip));
        published.then_some(name)
    }

    /// Remote-tracking branches that already contain commits in `action`'s
    /// range, so rewriting them needs a force push.
    fn pushed_remotes(&self, action: EditAction) -> Vec<String> {
        let Some((_, commits)) = self.plan(action) else {
            return Vec::new();
        };
        self.pushed_remotes_in(&commits)
    }

    /// Remote-tracking branches that already contain one of `commits`, for a
    /// range another action builds, such as a rebase onto another ref.
    fn pushed_remotes_in(&self, commits: &[Oid]) -> Vec<String> {
        remotes_containing(commits, &self.remotes, |commit, tip| {
            self.reaches(commit, tip)
        })
    }

    /// Whether `ancestor` is reachable from `descendant`.
    ///
    /// The loaded rows answer this without touching objects. A commit the walk
    /// has not published yet cannot be on the path to an edited commit that
    /// was, because the walk emits children before parents, so it is a leaf
    /// and the check stops there. The repository read is the fallback for a
    /// narrowed history scope that left the remote out.
    fn reaches(&self, ancestor: Oid, descendant: Oid) -> bool {
        let mut stack = vec![descendant];
        let mut seen = HashSet::new();
        while let Some(id) = stack.pop() {
            if id == ancestor {
                return true;
            }
            if !seen.insert(id) {
                continue;
            }
            if seen.len() > REMOTE_WALK_LIMIT {
                return false;
            }
            let parents = self.parents_of(id);
            if let Some(parents) = parents {
                stack.extend(parents);
            }
        }
        false
    }
}

/// The part of [`EditContext`] the enable rules read.
#[derive(Clone, Copy)]
struct LineView<'a> {
    line: &'a [LineCommit],
    distance: Option<usize>,
    reachable: bool,
    side: Option<SideView>,
}

/// A commit HEAD reaches only through a merge's second parent, seen from the
/// rules: `git rebase --rebase-merges` can replay its side branch, but only
/// within that branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SideView {
    is_merge: bool,
    parent_is_merge: bool,
    /// The parent is where the side branch forked, so it is outside the
    /// branch's own chain (also true for a root, which has no parent).
    parent_on_line: bool,
}

impl<'a> LineView<'a> {
    fn at(line: &'a [LineCommit], distance: Option<usize>, reachable: bool) -> Self {
        Self {
            line,
            distance,
            reachable,
            side: None,
        }
    }

    #[cfg(test)]
    fn with_side(mut self, side: SideView) -> Self {
        self.side = Some(side);
        self
    }

    /// Whether `action` has to replay through merges with
    /// `git rebase --rebase-merges` instead of a plain `git rebase -i`.
    fn needs_rebase_merges(&self, action: EditAction) -> bool {
        let Some(distance) = self.distance else {
            return self.reachable;
        };
        match action {
            EditAction::Reword => false,
            EditAction::Drop | EditAction::MoveUp => merge_within(self.line, distance),
            EditAction::Squash | EditAction::Fixup | EditAction::MoveDown => {
                merge_within(self.line, distance + 1)
            }
        }
    }

    /// The target and its parents, when it is on the line.
    fn target(&self) -> Option<&'a (Oid, Vec<Oid>)> {
        self.line.get(self.distance?)
    }

    /// Whether a merge commit sits between HEAD and the target, which only a
    /// replay has to care about.
    fn has_merge(&self) -> bool {
        self.distance
            .is_some_and(|distance| merge_within(self.line, distance))
    }

    /// Whether a reword of the target has to be rebuilt with `git commit-tree`
    /// rather than replayed: a merge stops `git rebase -i`, and a root commit
    /// has no `onto` commit to replay onto.
    fn reword_needs_rebuild(&self) -> bool {
        self.target().is_some_and(|(_, parents)| parents.is_empty()) || self.has_merge()
    }
}

/// Why `action` cannot run on the commit the view describes.
///
/// A commit HEAD cannot reach at all is a different problem from one a merge
/// brought in, and both differ from a merge between the commit and HEAD, which
/// only stops an action that has to replay the range. A squash or a downward
/// move needs the parent's parent as the range base, so a root in that spot has
/// no `onto` commit to offer.
fn blocked_on_line(view: LineView<'_>, action: EditAction) -> Option<String> {
    let Some(target) = view.target() else {
        if !view.reachable {
            return Some(t!("history_ops.reason.not_on_branch").to_string());
        }
        return blocked_on_side(view.side, action);
    };
    let distance = view.distance?;
    if action == EditAction::Reword {
        // A reword only replaces a message, so it is rebuilt rather than
        // replayed: a root or a merge in the range does not stand in its way.
        return None;
    }
    if target.1.is_empty() {
        return Some(t!("history_ops.reason.root_commit").to_string());
    }
    // `--rebase-merges` recreates merges around the commit, but a merge itself
    // is not a `pick` it can drop, meld or move.
    if target.1.len() > 1 {
        return Some(t!("history_ops.reason.is_merge").to_string());
    }
    let is_merge = |index: usize| {
        view.line
            .get(index)
            .is_some_and(|(_, parents)| parents.len() > 1)
    };
    match action {
        EditAction::Reword | EditAction::Drop => {}
        EditAction::Squash | EditAction::Fixup => {
            if is_merge(distance + 1) {
                return Some(t!("history_ops.reason.parent_merge").to_string());
            }
            if view.line.get(distance + 2).is_none() {
                return Some(t!("history_ops.reason.parent_root").to_string());
            }
        }
        EditAction::MoveUp => {
            if distance == 0 {
                return Some(t!("history_ops.reason.already_newest").to_string());
            }
            if is_merge(distance - 1) {
                return Some(t!("history_ops.reason.next_to_merge").to_string());
            }
        }
        EditAction::MoveDown => {
            if is_merge(distance + 1) {
                return Some(t!("history_ops.reason.next_to_merge").to_string());
            }
            if view.line.get(distance + 2).is_none() {
                return Some(t!("history_ops.reason.already_oldest").to_string());
            }
        }
    }
    None
}

/// The same rules for a commit a merge brought in. Without its shape the
/// commit can only be described, not edited.
fn blocked_on_side(side: Option<SideView>, action: EditAction) -> Option<String> {
    let merged_in = || Some(t!("history_ops.reason.merged_in").to_string());
    let Some(side) = side else {
        return merged_in();
    };
    match action {
        // The commit-tree rebuild follows HEAD's first-parent line only.
        EditAction::Reword => merged_in(),
        _ if side.is_merge => Some(t!("history_ops.reason.is_merge").to_string()),
        EditAction::Drop | EditAction::MoveUp => None,
        EditAction::Squash | EditAction::Fixup if side.parent_on_line => {
            Some(t!("history_ops.reason.parent_outside").to_string())
        }
        EditAction::MoveDown if side.parent_on_line => {
            Some(t!("history_ops.reason.already_oldest").to_string())
        }
        EditAction::Squash | EditAction::Fixup if side.parent_is_merge => {
            Some(t!("history_ops.reason.parent_merge").to_string())
        }
        EditAction::MoveDown if side.parent_is_merge => {
            Some(t!("history_ops.reason.next_to_merge").to_string())
        }
        EditAction::Squash | EditAction::Fixup | EditAction::MoveDown => None,
    }
}

/// Whether a merge commit sits between HEAD and the commit at `index`.
fn merge_within(line: &[LineCommit], index: usize) -> bool {
    line.iter()
        .take(index + 1)
        .any(|(_, parents)| parents.len() > 1)
}

/// Index in `line` of the commit the range starts after.
fn onto_index(distance: usize, action: EditAction) -> Option<usize> {
    Some(match action {
        EditAction::Reword | EditAction::Drop => distance + 1,
        EditAction::Squash | EditAction::Fixup | EditAction::MoveDown => distance + 2,
        EditAction::MoveUp if distance > 0 => distance + 1,
        EditAction::MoveUp => return None,
    })
}

/// `line[..onto]` oldest first: the first-parent list core's helpers expect.
fn range_commits(line: &[LineCommit], onto_index: usize) -> Vec<Oid> {
    line[..onto_index]
        .iter()
        .rev()
        .map(|(commit, _)| *commit)
        .collect()
}

/// Distance from HEAD the edited result is expected to have once git has
/// rewritten the branch, so the selection can follow it.
///
/// A squash or fixup replaces the selected commit's parent with the combined
/// commit, and the parent moves up into the selected commit's place. A move
/// leaves the commit one step above or below where it was.
fn selection_distance(distance: usize, action: EditAction) -> Option<usize> {
    match action {
        EditAction::Reword | EditAction::Squash | EditAction::Fixup | EditAction::Drop => {
            Some(distance)
        }
        EditAction::MoveUp => distance.checked_sub(1),
        EditAction::MoveDown => Some(distance + 1),
    }
}

/// Remote names whose tip reaches a commit of `range`.
fn remotes_containing(
    range: &[Oid],
    remotes: &[(String, Oid)],
    mut reachable: impl FnMut(Oid, Oid) -> bool,
) -> Vec<String> {
    let mut names = Vec::new();
    for (name, tip) in remotes {
        if range.iter().any(|commit| reachable(*commit, *tip)) {
            names.push(name.clone());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Whether a rewrite may run, with the reason when it may not.
///
/// The state checks come first because they decide the whole repository, then
/// the line decides the commit, and the worktree comes last because the more
/// specific reason is more useful than t!("reflog.commit_or_stash").to_string().
fn edit_blocked(
    state: &MenuState,
    edit: Option<&EditContext>,
    action: EditAction,
) -> Option<String> {
    let view = match edit {
        Some(edit) => edit.view(),
        None => LineView::at(&[], None, false),
    };
    edit_blocked_on_line(state, view, action)
}

fn edit_blocked_on_line(
    state: &MenuState,
    view: LineView<'_>,
    action: EditAction,
) -> Option<String> {
    // Interactive rebase is not a remote operation. This check has to stay
    // ahead of the shared busy/pending reasons, or stopping the blanket remote
    // block would turn the rewrite entries on.
    if state.ssh {
        return Some(crate::remote_open::unavailable_reason());
    }
    if state.busy {
        return Some(t!("branch.reason.busy").to_string());
    }
    if state.pending {
        return Some(t!("reflog.finish_first").to_string());
    }
    if state.branch.is_none() && state.head.is_some() {
        return Some(t!("reflog.head_detached").to_string());
    }
    if let Some(reason) = blocked_on_line(view, action) {
        return Some(reason);
    }
    // A reword never needs a clean worktree: with uncommitted changes it is
    // rebuilt with `git commit-tree`, which only moves the branch and keeps
    // HEAD's tree, so the changes stay valid. Every other rewrite replays its
    // changes through the worktree, so it waits for a clean one.
    if state.dirty && action != EditAction::Reword {
        return Some(t!("reflog.commit_or_stash").to_string());
    }
    None
}

/// Why the interactive history dialog cannot open. It replays the range a
/// reword does with `git rebase -i`, which neither a merge nor a root commit
/// gives an `onto` commit for.
fn edit_history_blocked_on_line(state: &MenuState, view: LineView<'_>) -> Option<String> {
    if let Some(reason) = edit_blocked_on_line(state, view, EditAction::Reword) {
        return Some(reason);
    }
    if view.target().is_some_and(|(_, parents)| parents.is_empty()) {
        return Some(t!("history_ops.reason.parent_root").to_string());
    }
    if view.has_merge() {
        return Some(t!("history_ops.reason.merge").to_string());
    }
    None
}

/// Why one row of the "Edit" submenu cannot run.
fn edit_entry_blocked_on_line(
    state: &MenuState,
    view: LineView<'_>,
    entry: EditEntry,
) -> Option<String> {
    match entry {
        EditEntry::Action(action) => edit_blocked_on_line(state, view, action),
        EditEntry::History => edit_history_blocked_on_line(state, view),
    }
}

/// The line a menu's edit context describes, or an empty view without one.
fn view_of(edit: Option<&EditContext>) -> LineView<'_> {
    match edit {
        Some(edit) => edit.view(),
        None => LineView::at(&[], None, false),
    }
}

fn edit_entry_blocked(
    state: &MenuState,
    edit: Option<&EditContext>,
    entry: EditEntry,
) -> Option<String> {
    edit_entry_blocked_on_line(state, view_of(edit), entry)
}

/// The one reason that blocks every edit row, when they all share it.
///
/// The menu then shows a disabled "Edit" entry with that reason instead of a
/// submenu whose every row repeats it. One available row is enough to open the
/// submenu, where the inapplicable rows say why.
fn collapsed_edit_reason_on_line(state: &MenuState, view: LineView<'_>) -> Option<String> {
    let mut shared: Option<String> = None;
    for entry in EditEntry::ALL {
        let blocked = edit_entry_blocked_on_line(state, view, entry)?;
        match &shared {
            Some(previous) if *previous != blocked => return None,
            _ => shared = Some(blocked),
        }
    }
    shared
}

fn collapsed_edit_reason(state: &MenuState, edit: Option<&EditContext>) -> Option<String> {
    collapsed_edit_reason_on_line(state, view_of(edit))
}

/// The selection a finished rewrite should restore: the commit at `distance`
/// first-parent steps below the new HEAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingSelection {
    old_head: Oid,
    distance: usize,
}

impl PendingSelection {
    /// The selection a plan rewrite restores: the commit `distance` steps
    /// below the rewritten tip, once the new snapshot arrives.
    pub(crate) fn at(old_head: Oid, distance: usize) -> Self {
        Self { old_head, distance }
    }
}

/// The first-parent commit `distance` steps below `head`, as a row index.
fn row_at_distance(snapshot: &RepoSnapshot, head: Oid, distance: usize) -> Option<usize> {
    let mut current = head;
    for _ in 0..distance {
        let index = *snapshot.commit_index.get(&current)?;
        let (commit, _) = snapshot.commits.get(index)?;
        current = *commit.parents.first()?;
    }
    snapshot.commit_index.get(&current).copied()
}

/// The fixup todo core has no named helper for; every other step is a pick.
fn fixup_steps(commits: &[Oid], commit: Oid) -> Result<Vec<RebaseStep>> {
    let position = commits
        .iter()
        .position(|candidate| *candidate == commit)
        .ok_or_else(|| {
            Error::other(t!("history_ops.reason.not_in_range", commit = commit).to_string())
        })?;
    let mut steps = commits
        .iter()
        .map(|commit| RebaseStep {
            commit: *commit,
            action: RebaseAction::Pick,
        })
        .collect::<Vec<_>>();
    steps[position].action = RebaseAction::Fixup;
    Ok(steps)
}

/// The parts of a commit row a menu entry needs, owned so the menu closure can
/// outlive the row it was built for.
#[derive(Clone, Debug)]
pub(crate) struct CommitTarget {
    /// Full object id; absent for the synthetic source, which has no commits.
    pub(crate) id: Option<Oid>,
    pub(crate) short: String,
    pub(crate) summary: String,
    /// Tags pointing at this commit, for their delete entries.
    pub(crate) tags: Vec<String>,
}

/// A multi-selection's pick, read when its context menu opens.
///
/// `commits` is already oldest first, so the menu cannot apply a pick in
/// click order.
pub(crate) struct MultiPickContext {
    commits: Vec<Oid>,
    /// The ids `Copy commit ids` writes, newline separated.
    copy: String,
    count: usize,
    branch: String,
    blocked: Option<String>,
    /// The synthetic uncommitted row is part of the selection.
    uncommitted: bool,
}

impl MultiPickContext {
    /// The selection's real commits, oldest first. The synthetic benchmark's
    /// rows are left out.
    pub(super) fn commits(&self) -> &[Oid] {
        &self.commits
    }

    pub(super) fn uncommitted(&self) -> bool {
        self.uncommitted
    }
}

/// A branch ref that points at the commit a menu was opened on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BranchAtCommit {
    pub(crate) kind: RefKind,
    pub(crate) name: String,
}

/// The branch actions offered for a ref.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BranchAction {
    Checkout,
    Merge,
    Rebase,
    Rename,
    Delete,
    Solo,
    Hide,
    ShowAll,
}

impl BranchAction {
    /// Whether the action moves HEAD or rewrites history, which git refuses
    /// while another merge, rebase or cherry-pick is unfinished.
    ///
    /// Rename and delete only write a ref, so they keep working mid-operation.
    fn needs_idle_repository(self) -> bool {
        matches!(self, Self::Checkout | Self::Merge | Self::Rebase)
    }
}

/// Which branch entries make sense for `kind`.
///
/// The checked-out branch cannot be checked out, merged into itself or
/// rebased onto itself, and deleting it would refuse anyway; renaming it is
/// the one action that still applies. A remote branch has no local name to
/// rename or delete, so it only offers the three history actions.
///
/// `hidden` is this ref's own state, so a hidden branch does not offer "hide"
/// again; `any_hidden` adds the way back to every branch.
pub(crate) fn branch_actions(
    kind: RefKind,
    is_current: bool,
    hidden: bool,
    any_hidden: bool,
) -> Vec<BranchAction> {
    if kind == RefKind::Tag {
        return Vec::new();
    }
    let mut actions = match kind {
        RefKind::LocalBranch if is_current => vec![BranchAction::Rename],
        RefKind::LocalBranch => vec![
            BranchAction::Checkout,
            BranchAction::Merge,
            BranchAction::Rebase,
            BranchAction::Rename,
            BranchAction::Delete,
        ],
        RefKind::RemoteBranch => vec![
            BranchAction::Checkout,
            BranchAction::Merge,
            BranchAction::Rebase,
        ],
        RefKind::Tag => Vec::new(),
    };
    // Hiding the checked-out branch changes nothing: HEAD always walks.
    if !is_current && !hidden {
        actions.push(BranchAction::Hide);
    }
    actions.push(BranchAction::Solo);
    if any_hidden {
        actions.push(BranchAction::ShowAll);
    }
    actions
}

/// The top of the commit menu: every branch pointing at the commit.
///
/// One branch's entries sit inline, so the common case is one click. Several
/// branches become one submenu each, because five entries per branch would
/// bury the commit actions below them.
fn branch_section(
    menu: PopupMenu,
    commit: Oid,
    state: &MenuState,
    entity: &Entity<RepoView>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let branches = entity.read(cx).branches_at_commit(commit);
    if branches.is_empty() {
        return menu;
    }
    let current = state.branch_label();
    if let [branch] = branches.as_slice() {
        return branch_actions_menu(
            menu.separator(),
            branch,
            &current,
            state.clone(),
            entity,
            cx,
        )
        .separator();
    }
    let mut menu = menu.separator();
    for branch in branches {
        let label = branch.name.clone();
        let current = current.clone();
        let state = state.clone();
        let entity = entity.clone();
        menu = menu.submenu(label, window, cx, move |submenu, _, cx| {
            branch_actions_menu(submenu, &branch, &current, state.clone(), &entity, cx)
        });
    }
    menu.separator()
}

/// One branch's entries, in the order [`branch_actions`] lists them.
///
/// The menu shows an operation's reason once at its top, so entries blocked
/// only by that reason stay short. Rename and delete keep working while an
/// operation is unfinished; checkout, merge and rebase wait for it.
fn branch_actions_menu(
    mut menu: PopupMenu,
    branch: &BranchAtCommit,
    current: &str,
    state: MenuState,
    entity: &Entity<RepoView>,
    cx: &App,
) -> PopupMenu {
    let busy = state.busy();
    let shared = state.shared_reason();
    let history_locked = busy || state.pending();
    // Same gap as the sidebar: the helper walk is not refiltered.
    let scope_reason = state.ssh.then(crate::remote_open::unavailable_reason);
    let is_current = branch.kind == RefKind::LocalBranch && branch.name == current;
    let hidden = entity.read(cx).is_ref_hidden(branch.kind, &branch.name);
    let any_hidden = entity.read(cx).hidden_ref_count() > 0;
    for action in branch_actions(branch.kind, is_current, hidden, any_hidden) {
        let name = branch.name.clone();
        let kind = branch.kind;
        // The shared reason is already written once at the top of the menu, so
        // these entries stay plain; they are only ever blocked by it.
        let locked = if action.needs_idle_repository() {
            history_locked
        } else {
            busy
        };
        let item = match action {
            BranchAction::Solo => solo_history_item(kind, &name, entity, scope_reason.as_deref()),
            BranchAction::Hide => hide_history_item(kind, &name, entity, scope_reason.as_deref()),
            BranchAction::ShowAll => show_all_history_item(entity, scope_reason.as_deref()),
            BranchAction::Rebase => {
                let Some((_, target)) = entity.read(cx).branch_rebase_target(kind, &name) else {
                    continue;
                };
                rebase_menu_item(
                    target,
                    current,
                    Some(&name),
                    shared.as_deref(),
                    entity.clone(),
                )
            }
            BranchAction::Checkout => {
                let label = t!("history_ops.branch.checkout", branch = name).to_string();
                PopupMenuItem::new(label).disabled(locked).on_click({
                    let entity = entity.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |view, cx| view.checkout_branch(kind, &name, window, cx));
                    }
                })
            }
            BranchAction::Merge => {
                let label =
                    t!("history_ops.branch.merge", branch = name, current = current).to_string();
                PopupMenuItem::new(label).disabled(locked).on_click({
                    let entity = entity.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |view, cx| view.merge_branch(&name, window, cx));
                    }
                })
            }
            BranchAction::Rename => {
                let label = t!("history_ops.branch.rename", branch = name).to_string();
                PopupMenuItem::new(label).disabled(locked).on_click({
                    let entity = entity.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |view, cx| view.rename_branch(&name, window, cx));
                    }
                })
            }
            BranchAction::Delete => {
                let label = t!("history_ops.branch.delete", branch = name).to_string();
                PopupMenuItem::new(label).disabled(locked).on_click({
                    let entity = entity.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |view, cx| view.delete_branch_here(&name, window, cx));
                    }
                })
            }
        };
        menu = menu.item(item);
    }
    menu
}

/// The refs a repository's history leaves out, as the sidebar and the commit
/// menu offer them.
///
/// The sidebar calls this for its own branch rows; the commit menu's branch
/// section appends the same entries through [`branch_actions_menu`], so both
/// places offer the same three commands.
pub(crate) fn history_filter_menu(
    mut menu: PopupMenu,
    kind: RefKind,
    name: &str,
    is_current: bool,
    entity: &Entity<RepoView>,
    cx: &App,
) -> PopupMenu {
    // The helper publishes one unfiltered walk. Hiding a ref here would only
    // change the local copy of the scope, so the graph would keep the commits.
    let reason = entity
        .read(cx)
        .is_ssh()
        .then(crate::remote_open::unavailable_reason);
    let hidden = entity.read(cx).is_ref_hidden(kind, name);
    let any_hidden = entity.read(cx).hidden_ref_count() > 0;
    if !is_current && !hidden {
        menu = menu.item(hide_history_item(kind, name, entity, reason.as_deref()));
    }
    menu = menu.item(solo_history_item(kind, name, entity, reason.as_deref()));
    if any_hidden {
        menu = menu.item(show_all_history_item(entity, reason.as_deref()));
    }
    menu
}

fn hide_history_item(
    kind: RefKind,
    name: &str,
    entity: &Entity<RepoView>,
    reason: Option<&str>,
) -> PopupMenuItem {
    let entity = entity.clone();
    let name = name.to_owned();
    PopupMenuItem::new(crate::ui::menu_label(
        t!("branch.menu.hide_in_history").to_string(),
        reason,
    ))
    .disabled(reason.is_some())
    .on_click(move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
        entity.update(cx, |view, cx| view.hide_ref_in_history(kind, &name, cx));
    })
}

fn solo_history_item(
    kind: RefKind,
    name: &str,
    entity: &Entity<RepoView>,
    reason: Option<&str>,
) -> PopupMenuItem {
    let entity = entity.clone();
    let name = name.to_owned();
    PopupMenuItem::new(crate::ui::menu_label(
        t!("branch.menu.show_only").to_string(),
        reason,
    ))
    .disabled(reason.is_some())
    .on_click(move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
        entity.update(cx, |view, cx| view.show_only_ref(kind, &name, cx));
    })
}

fn show_all_history_item(entity: &Entity<RepoView>, reason: Option<&str>) -> PopupMenuItem {
    let entity = entity.clone();
    PopupMenuItem::new(crate::ui::menu_label(
        t!("branch.menu.show_all_hidden").to_string(),
        reason,
    ))
    .disabled(reason.is_some())
    .on_click(move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
        entity.update(cx, |view, cx| view.show_hidden_refs(cx));
    })
}

/// The context menu for one commit row.
///
/// The top level stays short: the entries that act on the commit directly, then
/// one submenu per group of related actions. Clipboard entries are always
/// available; the history actions need a real commit and a session, and say why
/// they are disabled when they cannot run. Popup items carry no tooltip, so the
/// reason is folded into the label.
pub(crate) fn commit_menu(
    mut menu: PopupMenu,
    target: CommitTarget,
    state: Option<MenuState>,
    multi: Option<MultiPickContext>,
    entity: Entity<RepoView>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    // A merge, rebase or cherry-pick disables several entries for the same
    // reason. One note at the top keeps the long menu readable; the entries
    // below stay short.
    let shared = state.as_ref().and_then(MenuState::shared_reason);
    if let Some(reason) = shared.as_ref() {
        menu = menu.item(PopupMenuItem::label(reason.clone())).separator();
    }
    // While several rows are selected, the single-commit entries have no one
    // row to act on, so the menu offers the multi-pick instead.
    if let Some(multi) = multi {
        return multi_commit_menu(menu, &multi, shared.as_deref(), entity);
    }
    // The branches pointing at this commit act on the branch, not the commit,
    // so they lead the menu: that is what a right-click on a tagged tip is
    // usually for.
    if let (Some(commit), Some(state)) = (target.id, state.as_ref()) {
        menu = branch_section(menu, commit, state, &entity, window, cx);
    }
    let copy_id = target
        .id
        .map_or_else(|| target.short.clone(), |commit| commit.to_string());
    let copy_summary = target.summary.clone();
    menu = menu.separator();
    menu = copy_menu(menu, copy_id, copy_summary, window, cx);

    let (Some(commit), Some(state)) = (target.id, state) else {
        return menu;
    };
    let short = target.short;
    let summary = target.summary;
    let tags = target.tags;
    let shared = shared.as_deref();
    menu = RepoView::graph_submenu(menu, entity.clone(), commit, state.head, window, cx);

    if let Some(item) = crate::github::commit_item(&entity, commit, cx) {
        menu = menu.item(item);
    }

    let onto = state.branch_label();
    let cherry_blocked = state.blocked_for(HistoryAction::CherryPick, commit);
    let revert_blocked = state.blocked_for(HistoryAction::Revert, commit);
    let checkout_blocked = state.blocked(HistoryAction::CheckoutCommit);
    // A merge commit is picked or reverted against its first parent (`-m 1` in
    // `git_cli::history`), so these stay enabled for merges.
    let checkout = {
        let entity = entity.clone();
        let short = short.clone();
        PopupMenuItem::new(blocked_label(
            t!("history_ops.menu.checkout_commit").to_string(),
            checkout_blocked.as_deref(),
            shared,
        ))
        .disabled(checkout_blocked.is_some())
        .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let short = short.clone();
            let entity = entity.clone();
            // The context menu dismisses as this click returns and drops a
            // dialog opened in the same turn, the same way Revert did.
            window.defer(cx, move |window, cx| {
                entity.update(cx, |view, cx| {
                    view.confirm_checkout_commit(commit, short, window, cx);
                });
            });
        })
    };
    let cherry = {
        let entity = entity.clone();
        let short = short.clone();
        PopupMenuItem::new(blocked_label(
            t!("history_ops.menu.cherry_pick_onto", onto = onto).to_string(),
            cherry_blocked.as_deref(),
            shared,
        ))
        .disabled(cherry_blocked.is_some())
        .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let short = short.clone();
            entity.update(cx, |view, cx| view.cherry_pick(commit, short, window, cx));
        })
    };
    let revert = {
        let entity = entity.clone();
        let short = short.clone();
        PopupMenuItem::new(blocked_label(
            t!("history_ops.menu.revert").to_string(),
            revert_blocked.as_deref(),
            shared,
        ))
        .disabled(revert_blocked.is_some())
        .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let short = short.clone();
            let entity = entity.clone();
            // The context menu dismisses as this click returns and drops a
            // dialog opened in the same turn. Cherry-pick has no dialog, so
            // only Revert looked like a no-op.
            window.defer(cx, move |window, cx| {
                entity.update(cx, |view, cx| {
                    view.confirm_revert(commit, short, window, cx);
                });
            });
        })
    };
    let rebase = entity.read(cx).rebase_target(
        commit,
        t!("history_ops.menu.commit_label", short = short).to_string(),
    );
    menu = menu.separator().item(checkout).item(cherry).item(revert);
    if let Some(target) = rebase {
        menu = menu.item(rebase_menu_item(
            target,
            &onto,
            None,
            shared,
            entity.clone(),
        ));
    }

    // Saving a patch asks the helper for the bytes and writes the file here.
    // Creating a worktree runs on the helper. Both only wait out a busy
    // repository.
    let busy_reason = state.busy().then(|| t!("branch.reason.busy").to_string());
    let patch_reason = busy_reason.clone();
    let branch_blocked = state.blocked(HistoryAction::CreateBranch);
    let tag_blocked = state.blocked(HistoryAction::CreateTag);
    menu = menu.separator();
    menu = create_menu(
        menu,
        commit,
        short.clone(),
        branch_blocked,
        tag_blocked,
        busy_reason,
        patch_reason,
        shared.map(str::to_owned),
        entity.clone(),
        window,
        cx,
    );
    let delete_blocked = state.blocked(HistoryAction::DeleteTag);
    for tag in tags {
        let entity = entity.clone();
        let name = tag.clone();
        menu = menu.item(
            PopupMenuItem::new(blocked_label(
                t!("history_ops.menu.delete_tag", tag = tag).to_string(),
                delete_blocked.as_deref(),
                shared,
            ))
            .disabled(delete_blocked.is_some())
            .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let name = name.clone();
                entity.update(cx, |view, cx| view.confirm_delete_tag(name, window, cx));
            }),
        );
    }
    let edit = entity.read(cx).edit_context(commit);
    menu = menu.separator();
    menu = edit_menu(
        menu,
        commit,
        summary,
        state.clone(),
        edit,
        shared.map(str::to_owned),
        entity.clone(),
        window,
        cx,
    );
    menu = reset_menu(
        menu,
        commit,
        short.clone(),
        state.clone(),
        shared,
        entity.clone(),
        window,
        cx,
    );
    menu = compare_menu(
        menu,
        commit,
        state.head,
        shared.map(str::to_owned),
        entity,
        window,
        cx,
    );
    menu
}

/// The "Copy" submenu: the full id and the summary of the commit.
fn copy_menu(
    menu: PopupMenu,
    id: String,
    summary: String,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    menu.submenu(
        t!("history_ops.menu.copy").to_string(),
        window,
        cx,
        move |submenu, _, _| {
            let copy_id = id.clone();
            let copy_summary = summary.clone();
            submenu
                .item(
                    PopupMenuItem::new(t!("history_ops.menu.copy_id").to_string()).on_click(
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()));
                        },
                    ),
                )
                .item(
                    PopupMenuItem::new(t!("history_ops.menu.copy_summary").to_string()).on_click(
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_summary.clone()));
                        },
                    ),
                )
        },
    )
}

/// The "Create" submenu: a branch, a tag, a worktree or a patch at this commit.
#[allow(clippy::too_many_arguments)]
fn create_menu(
    menu: PopupMenu,
    commit: Oid,
    short: String,
    branch_blocked: Option<String>,
    tag_blocked: Option<String>,
    worktree_blocked: Option<String>,
    patch_blocked: Option<String>,
    shared: Option<String>,
    entity: Entity<RepoView>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    menu.submenu(
        t!("history_ops.menu.create").to_string(),
        window,
        cx,
        move |submenu, _, _| {
            let point = commit.to_string();
            let from = t!("history_ops.menu.from_commit", short = short).to_string();
            let branch = {
                let entity = entity.clone();
                PopupMenuItem::new(blocked_label(
                    t!("history_ops.menu.create_branch").to_string(),
                    branch_blocked.as_deref(),
                    shared.as_deref(),
                ))
                .disabled(branch_blocked.is_some())
                .on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let point = point.clone();
                        let from = from.clone();
                        entity.update(cx, |view, cx| {
                            view.open_create_dialog(Some(point), from, window, cx);
                        });
                    },
                )
            };
            let tag = {
                let entity = entity.clone();
                let short = short.clone();
                PopupMenuItem::new(blocked_label(
                    t!("history_ops.menu.create_tag").to_string(),
                    tag_blocked.as_deref(),
                    shared.as_deref(),
                ))
                .disabled(tag_blocked.is_some())
                .on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let short = short.clone();
                        entity.update(cx, |view, cx| {
                            view.open_tag_dialog(commit, short, window, cx)
                        });
                    },
                )
            };
            let worktree = {
                let entity = entity.clone();
                let short = short.clone();
                PopupMenuItem::new(blocked_label(
                    t!("history_ops.menu.create_worktree").to_string(),
                    worktree_blocked.as_deref(),
                    shared.as_deref(),
                ))
                .disabled(worktree_blocked.is_some())
                .on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |view, cx| {
                            view.open_worktree_from_commit(commit, short.clone(), window, cx)
                        });
                    },
                )
            };
            let patch = {
                let entity = entity.clone();
                let short = short.clone();
                PopupMenuItem::new(blocked_label(
                    t!("history_ops.menu.save_patch").to_string(),
                    patch_blocked.as_deref(),
                    shared.as_deref(),
                ))
                .disabled(patch_blocked.is_some())
                .on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let short = short.clone();
                        entity.update(cx, |view, cx| {
                            view.save_commit_as_patch(commit, short, window, cx)
                        });
                    },
                )
            };
            submenu.item(branch).item(tag).item(worktree).item(patch)
        },
    )
}

/// The "Edit" submenu, or one disabled "Edit" entry when every row is blocked
/// for the same reason.
#[allow(clippy::too_many_arguments)]
fn edit_menu(
    menu: PopupMenu,
    commit: Oid,
    summary: String,
    state: MenuState,
    edit: Option<EditContext>,
    shared: Option<String>,
    entity: Entity<RepoView>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let label = t!("history_ops.menu.edit").to_string();
    // `PopupMenu` cannot mark a submenu entry itself disabled, so a blocked
    // group is a plain disabled entry that names the one reason.
    if let Some(reason) = collapsed_edit_reason(&state, edit.as_ref()) {
        return menu.item(
            PopupMenuItem::new(blocked_label(
                label,
                Some(reason.as_str()),
                shared.as_deref(),
            ))
            .disabled(true),
        );
    }
    menu.submenu(label, window, cx, move |submenu, _, _| {
        let mut submenu = submenu;
        for entry in EditEntry::ALL {
            if entry == EditEntry::History {
                submenu = submenu.separator();
            }
            let blocked = edit_entry_blocked(&state, edit.as_ref(), entry);
            let mut item = PopupMenuItem::new(blocked_label(
                entry.label(),
                blocked.as_deref(),
                shared.as_deref(),
            ))
            .disabled(blocked.is_some());
            if blocked.is_none() {
                let entity = entity.clone();
                let summary = summary.clone();
                item = match entry {
                    EditEntry::Action(action) => {
                        item.on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            entity.update(cx, |view, cx| {
                                view.open_edit(action, commit, summary.clone(), window, cx)
                            });
                        })
                    }
                    EditEntry::History => {
                        item.on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            entity
                                .update(cx, |view, cx| view.open_edit_history(commit, window, cx));
                        })
                    }
                };
            }
            submenu = submenu.item(item);
        }
        submenu
    })
}

/// The "Reset" submenu, or one disabled entry when a reset cannot run.
#[allow(clippy::too_many_arguments)]
fn reset_menu(
    menu: PopupMenu,
    commit: Oid,
    short: String,
    state: MenuState,
    shared: Option<&str>,
    entity: Entity<RepoView>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let label = t!("history_ops.menu.reset").to_string();
    let Some(reason) = state.blocked(HistoryAction::Reset) else {
        return menu.submenu(label, window, cx, move |submenu, _, _| {
            let entry = |mode: ResetMode, label: String| {
                let entity = entity.clone();
                let short = short.clone();
                PopupMenuItem::new(label).on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let short = short.clone();
                        entity.update(cx, |view, cx| {
                            view.confirm_reset(commit, short, mode, window, cx)
                        });
                    },
                )
            };
            submenu
                .item(entry(
                    ResetMode::Soft,
                    t!("history_ops.menu.soft").to_string(),
                ))
                .item(entry(
                    ResetMode::Mixed,
                    t!("history_ops.menu.mixed").to_string(),
                ))
                .item(entry(
                    ResetMode::Hard,
                    t!("history_ops.menu.hard").to_string(),
                ))
                .item(entry(
                    ResetMode::Keep,
                    t!("history_ops.menu.keep").to_string(),
                ))
        });
    };
    menu.item(
        PopupMenuItem::new(blocked_label(label, Some(reason.as_str()), shared)).disabled(true),
    )
}

/// The "Compare" submenu: against HEAD and against the working tree.
fn compare_menu(
    menu: PopupMenu,
    commit: Oid,
    head: Option<Oid>,
    shared: Option<String>,
    entity: Entity<RepoView>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    // A menu only opens with a session, so the repository is there.
    let head_blocked = super::compare::with_head_blocked(true, Some(commit), head);
    let worktree_blocked = super::compare::with_worktree_blocked(true, Some(commit));
    menu.submenu(
        t!("history_ops.menu.compare").to_string(),
        window,
        cx,
        move |submenu, _, _| {
            let head_item = {
                let entity = entity.clone();
                PopupMenuItem::new(blocked_label(
                    t!("history_ops.menu.compare_head").to_string(),
                    head_blocked.as_deref(),
                    shared.as_deref(),
                ))
                .disabled(head_blocked.is_some())
                .on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |view, cx| {
                            view.open_compare_with_head(commit, window, cx)
                        });
                    },
                )
            };
            let worktree_item = {
                let entity = entity.clone();
                PopupMenuItem::new(blocked_label(
                    t!("history_ops.menu.compare_worktree").to_string(),
                    worktree_blocked.as_deref(),
                    shared.as_deref(),
                ))
                .disabled(worktree_blocked.is_some())
                .on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |view, cx| {
                            view.open_compare_with_worktree(commit, window, cx)
                        });
                    },
                )
            };
            submenu.item(head_item).item(worktree_item)
        },
    )
}

/// The context menu for a multi-selection: the one pick it can run and the ids
/// it can copy. Single-commit entries are left out rather than acting on the
/// focused row, which the user did not necessarily point at.
fn multi_commit_menu(
    mut menu: PopupMenu,
    multi: &MultiPickContext,
    shared: Option<&str>,
    entity: Entity<RepoView>,
) -> PopupMenu {
    let copy = multi.copy.clone();
    menu = menu
        .item(
            PopupMenuItem::new(t!("history_ops.menu.copy_ids").to_string()).on_click(
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                },
            ),
        )
        .separator();

    // Exactly two commits are a comparison; more would need a base to compare
    // them all against, which the entry does not offer.
    if let [a, b] = multi.commits.as_slice() {
        // A menu only opens with a session, so the repository is there.
        let blocked = super::compare::two_commit_blocked(true, &multi.commits);
        let (a, b) = (*a, *b);
        menu = menu
            .item(
                PopupMenuItem::new(blocked_label(
                    t!("history_ops.menu.compare_commits").to_string(),
                    blocked.as_deref(),
                    shared,
                ))
                .disabled(blocked.is_some())
                .on_click({
                    let entity = entity.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        entity.update(cx, |view, cx| view.open_compare_pair(a, b, window, cx));
                    }
                }),
            )
            .separator();
    }
    // Two commits and the uncommitted row are three sides too: the working
    // tree takes the place of the third commit.
    if let [a, b] = multi.commits.as_slice()
        && multi.uncommitted
    {
        let commits = vec![*a, *b];
        menu = menu
            .item(
                PopupMenuItem::new(t!("history_ops.menu.compare_three_way").to_string()).on_click(
                    {
                        let entity = entity.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let commits = commits.clone();
                            entity.update(cx, |view, cx| {
                                view.open_three_way_working_tree(commits, window, cx)
                            });
                        }
                    },
                ),
            )
            .separator();
    }
    // Three commits are a three-way comparison with one of them as the base,
    // which the compare tab lets the user change.
    if let [a, b, c] = multi.commits.as_slice() {
        let blocked = super::compare::three_commit_blocked(true, &multi.commits);
        let commits = vec![*a, *b, *c];
        menu = menu
            .item(
                PopupMenuItem::new(blocked_label(
                    t!("history_ops.menu.compare_three_way").to_string(),
                    blocked.as_deref(),
                    shared,
                ))
                .disabled(blocked.is_some())
                .on_click({
                    let entity = entity.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let commits = commits.clone();
                        entity.update(cx, |view, cx| {
                            view.open_three_way_commits(commits, window, cx)
                        });
                    }
                }),
            )
            .separator();
    }
    let label = blocked_label(
        t!(
            "history_ops.menu.cherry_pick_many",
            count = multi.count,
            onto = multi.branch
        )
        .to_string(),
        multi.blocked.as_deref(),
        shared,
    );
    let mut item = PopupMenuItem::new(label).disabled(multi.blocked.is_some());
    if multi.blocked.is_none() {
        let commits = multi.commits.clone();
        item = item.on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let commits = commits.clone();
            entity.update(cx, |view, cx| view.cherry_pick_many(commits, window, cx));
        });
    }
    menu.item(item)
}

/// What a reset confirmation says: which branch moves, what happens to the
/// commits above it and — for a hard reset — how many uncommitted paths are
/// discarded. Counting the commits would need another git read; the worktree
/// counts are already in the snapshot.
fn reset_detail(
    branch: &str,
    short: &str,
    mode: ResetMode,
    staged: usize,
    unstaged: usize,
) -> String {
    match mode {
        ResetMode::Soft => t!(
            "history_ops.reset.soft_detail",
            branch = branch,
            short = short
        )
        .to_string(),
        ResetMode::Mixed => t!(
            "history_ops.reset.mixed_detail",
            branch = branch,
            short = short
        )
        .to_string(),
        ResetMode::Hard => {
            let changes = staged + unstaged;
            let lost = match changes {
                0 => t!("history_ops.reset.no_changes").to_string(),
                1 => t!("history_ops.reset.one_path").to_string(),
                count => t!(
                    &crate::i18n::count_key("history_ops.reset.paths", count),
                    count = count
                )
                .to_string(),
            };
            t!(
                "history_ops.reset.hard_detail",
                branch = branch,
                short = short,
                lost = lost
            )
            .to_string()
        }
        ResetMode::Keep => t!(
            "history_ops.reset.keep_detail",
            branch = branch,
            short = short
        )
        .to_string(),
    }
}

/// git's `check-ref-format` rules for `refs/tags/<name>`, plus the two cases
/// only the app can know: a leading `-` (which git would read as an option) and
/// a name that is already taken.
pub(crate) fn validate_tag_name(name: &str, existing: &[String]) -> Result<(), String> {
    if name.is_empty() {
        return Err(t!("history_ops.tag.required").to_string());
    }
    if existing.iter().any(|tag| tag == name) {
        return Err(t!("history_ops.tag.exists").to_string());
    }
    if name == "@" {
        return Err(t!("history_ops.tag.shorthand").to_string());
    }
    if name.starts_with('-') {
        return Err(t!("history_ops.tag.leading_dash").to_string());
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err(t!("history_ops.tag.slash_edges").to_string());
    }
    if name.contains("//") {
        return Err(t!("history_ops.tag.double_slash").to_string());
    }
    if name.contains("..") {
        return Err(t!("history_ops.tag.dot_dot").to_string());
    }
    if name.contains("@{") {
        return Err(t!("history_ops.tag.at_brace").to_string());
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(t!("history_ops.tag.control").to_string());
    }
    if name.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err(t!("history_ops.tag.specials").to_string());
    }
    if name.ends_with('.') {
        return Err(t!("history_ops.tag.trailing_dot").to_string());
    }
    if name.split('/').any(|part| {
        part.is_empty() || part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock")
    }) {
        return Err(t!("history_ops.tag.part").to_string());
    }
    Ok(())
}

impl RepoView {
    /// What the commit list's menu may offer, or `None` without a session
    /// (`--fake` has nothing to act on).
    pub(crate) fn history_menu_state(&self) -> Option<MenuState> {
        let snapshot = self.session.as_ref()?.snapshot();
        Some(
            MenuState::read(snapshot.head.as_ref(), snapshot.state, self.is_busy())
                .with_worktree(&snapshot.status)
                .with_ssh(self.is_ssh()),
        )
    }

    /// The branches whose tip is `commit`, local branches first.
    ///
    /// Tags are left out: their entries live in the commit menu's own section
    /// and have different actions.
    pub(super) fn branches_at_commit(&self, commit: Oid) -> Vec<BranchAtCommit> {
        let Some(session) = self.session.as_ref() else {
            return Vec::new();
        };
        session
            .snapshot()
            .refs
            .iter()
            .filter(|reference| {
                reference.target == commit
                    && matches!(reference.kind, RefKind::LocalBranch | RefKind::RemoteBranch)
            })
            .map(|reference| BranchAtCommit {
                kind: reference.kind,
                name: reference.short_name.clone(),
            })
            .collect()
    }

    /// The multi-selection's pick menu, or `None` while at most one row is
    /// selected. The reachability walk runs once per menu open, not per frame.
    pub(super) fn multi_pick_context(&self) -> Option<MultiPickContext> {
        if !self.selection.is_multi() {
            return None;
        }
        let source = self.source()?;
        let order = self.selection.oldest_first();
        let state = self.history_menu_state();
        let branch = state
            .as_ref()
            .map_or_else(|| "HEAD".to_owned(), MenuState::branch_label);
        let reachable = self
            .session
            .as_ref()
            .map(|session| head_reachable_set(session.as_ref()))
            .unwrap_or_default();
        let mut commits = Vec::new();
        let mut copy = Vec::new();
        let mut picks = Vec::new();
        let mut has_real_commits = true;
        for index in order {
            match source.commit(index) {
                Some(commit) => {
                    commits.push(commit.id);
                    copy.push(commit.id.to_string());
                    picks.push(MultiPickRow {
                        merge: commit.parents.len() > 1,
                        reachable: reachable.contains(&commit.id),
                    });
                }
                None => {
                    // The synthetic source has ids to copy but nothing git can
                    // pick; the menu stays disabled with the reason below.
                    has_real_commits = false;
                    if let Some(row) = source.row(index) {
                        copy.push(row.id.to_string());
                    }
                }
            }
        }
        let blocked = match &state {
            Some(state) if has_real_commits => commit_selection::multi_pick_blocked(
                &picks,
                state.busy,
                state.pending,
                state.branch.is_none() && state.head.is_some(),
            ),
            Some(_) => Some(t!("history_ops.no_commits_to_pick").to_string()),
            None => Some(t!("history_ops.no_repository").to_string()),
        };
        let uncommitted = self.selection.includes_uncommitted();
        // Replaying commits cannot include the working tree, which has no
        // patch of its own to replay.
        let blocked = if uncommitted {
            Some(t!("commit_selection.uncommitted_selected").to_string())
        } else {
            blocked
        };
        Some(MultiPickContext {
            commits,
            copy: copy.join("\n"),
            count: self.selection.rows().count(),
            branch,
            blocked,
            uncommitted,
        })
    }

    /// Applies the selected commits oldest first on top of the current branch.
    pub(crate) fn cherry_pick_many(
        &mut self,
        commits: Vec<Oid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if commits.is_empty() {
            return;
        }
        let count = commits.len();
        let onto = self
            .history_menu_state()
            .map_or_else(|| "HEAD".to_owned(), |state| state.branch_label());
        let label = t!(
            "history_ops.op.cherry_pick_many",
            count = count,
            onto = onto
        )
        .to_string();
        self.run_operation(Operation::CherryPickMany { commits }, label, window, cx);
    }

    /// `(staged, unstaged)` path counts from the snapshot, for the hard-reset
    /// confirmation.
    fn worktree_changes(&self) -> (usize, usize) {
        self.session.as_ref().map_or((0, 0), |session| {
            let snapshot = session.snapshot();
            (snapshot.status.staged.len(), snapshot.status.unstaged.len())
        })
    }

    /// Applies `<commit>` on top of the current branch.
    pub(crate) fn cherry_pick(
        &mut self,
        commit: Oid,
        short: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("history_ops.op.cherry_pick", short = short).to_string();
        self.run_operation(Operation::CherryPick { commit }, label, window, cx);
    }

    /// Asks before undoing a commit with a new commit.
    pub(crate) fn confirm_revert(
        &mut self,
        commit: Oid,
        short: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.history_menu_state() else {
            self.note_history_refusal(self.no_session_reason(), cx);
            return;
        };
        if let Some(reason) = state.blocked_for(HistoryAction::Revert, commit) {
            self.note_history_refusal(reason, cx);
            return;
        }
        let branch = state.branch_label();
        let detail = t!("history_ops.revert_detail", branch = branch, short = short).to_string();
        // Skipping the dialog runs the revert in this update. `confirm` would
        // call its callback immediately, and that callback updates this view
        // again.
        if !ui::should_ask(Settings::get(cx).behaviour.confirm_destructive, false) {
            self.run_revert(commit, short, window, cx);
            return;
        }
        // Revert leaves its own commit behind and the reflog still has the
        // original, so OK is not painted as a destructive action.
        let mut confirm = ui::Confirm::new(
            t!("history_ops.confirm.revert", short = short).to_string(),
            detail,
        )
        .ok_label(t!("common.revert").to_string());
        confirm.danger = false;
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        // `confirm`'s callback is `Fn`, so the short id is cloned per click
        // instead of moved out of the capture.
        ui::confirm(window, cx, confirm, move |window, cx| {
            let short = short.clone();
            let _ = entity.update(cx, |view, cx| view.run_revert(commit, short, window, cx));
        });
    }

    fn run_revert(
        &mut self,
        commit: Oid,
        short: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("history_ops.op.revert", short = short).to_string();
        self.run_operation(Operation::Revert { commit }, label, window, cx);
    }

    /// Asks before leaving the current branch for `commit`.
    ///
    /// The confirmation always opens. Skipping it when destructive
    /// confirmations are off would detach HEAD without saying so.
    pub(crate) fn confirm_checkout_commit(
        &mut self,
        commit: Oid,
        short: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.history_menu_state() else {
            self.note_history_refusal(self.no_session_reason(), cx);
            return;
        };
        if let Some(reason) = state.blocked(HistoryAction::CheckoutCommit) {
            self.note_history_refusal(reason, cx);
            return;
        }
        let detail = t!("history_ops.checkout_commit_detail", short = short).to_string();
        let mut confirm = ui::Confirm::new(
            t!("history_ops.confirm.checkout_commit", short = short).to_string(),
            detail,
        )
        .ok_label(t!("history_ops.checkout_commit_button").to_string())
        .always_ask();
        // The branch stays where it is and the reflog still has it, so OK is
        // not painted as a destructive action.
        confirm.danger = false;
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        // `confirm`'s callback is `Fn`, so the short id is cloned per click.
        ui::confirm(window, cx, confirm, move |window, cx| {
            let short = short.clone();
            let _ = entity.update(cx, |view, cx| {
                view.run_checkout_commit(commit, short, window, cx)
            });
        });
    }

    fn run_checkout_commit(
        &mut self,
        commit: Oid,
        short: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("history_ops.op.checkout_commit", short = short).to_string();
        self.run_operation(Operation::CheckoutCommit { commit }, label, window, cx);
    }

    /// A menu action that cannot run still has to say why. Returning without
    /// a note is what made Revert look like a no-op.
    fn note_history_refusal(&mut self, reason: String, cx: &mut Context<Self>) {
        self.status_note = Some(reason);
        self.notify_if_active(cx);
    }

    /// Asks before moving the current branch back to `target`.
    pub(crate) fn confirm_reset(
        &mut self,
        target: Oid,
        short: String,
        mode: ResetMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.history_menu_state() else {
            return;
        };
        if state.blocked(HistoryAction::Reset).is_some() {
            return;
        }
        let branch = state.branch_label();
        let (staged, unstaged) = self.worktree_changes();
        let detail = reset_detail(&branch, &short, mode, staged, unstaged);
        let title = match mode {
            ResetMode::Soft => t!(
                "history_ops.confirm.soft_reset",
                branch = branch,
                short = short
            )
            .to_string(),
            ResetMode::Mixed => t!(
                "history_ops.confirm.mixed_reset",
                branch = branch,
                short = short
            )
            .to_string(),
            ResetMode::Hard => t!(
                "history_ops.confirm.hard_reset",
                branch = branch,
                short = short
            )
            .to_string(),
            ResetMode::Keep => t!(
                "history_ops.confirm.keep_reset",
                branch = branch,
                short = short
            )
            .to_string(),
        };
        let confirm = match mode {
            // Only a hard reset deletes work: the other modes keep the changes
            // in the index or the working tree, and the reflog keeps the
            // commits. A keep reset aborts instead of overwriting a change.
            ResetMode::Hard => ui::Confirm::new(title, detail)
                .ok_label(t!("history_ops.hard_reset").to_string())
                .always_ask(),
            ResetMode::Soft | ResetMode::Mixed | ResetMode::Keep => {
                let mut confirm =
                    ui::Confirm::new(title, detail).ok_label(t!("common.reset").to_string());
                confirm.danger = false;
                confirm
            }
        };
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        ui::confirm(window, cx, confirm, move |window, cx| {
            let _ = entity.update(cx, |view, cx| view.reset_to(target, mode, window, cx));
        });
    }

    fn reset_to(
        &mut self,
        target: Oid,
        mode: ResetMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let flag = match mode {
            ResetMode::Soft => "soft",
            ResetMode::Mixed => "mixed",
            ResetMode::Hard => "hard",
            ResetMode::Keep => "keep",
        };
        let label = t!(
            "history_ops.op.reset",
            flag = flag,
            short = crate::data::short_oid(target)
        )
        .to_string();
        self.run_operation(Operation::Reset { target, mode }, label, window, cx);
    }

    /// Opens the create-tag form for `commit`.
    pub(crate) fn open_tag_dialog(
        &mut self,
        commit: Oid,
        short: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        let existing = self
            .session
            .as_ref()
            .map(|session| {
                session
                    .snapshot()
                    .refs
                    .iter()
                    .filter(|reference| reference.kind == RefKind::Tag)
                    .map(|reference| reference.short_name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // The context menu that offered this is closing; the dialog restores
        // the focus it was opened with, which must outlive the menu.
        self.focus_root(window, cx);
        let view = cx.weak_entity();
        let dialog = cx.new(|cx| TagDialog::new(view, commit, short, existing, window, cx));
        let focus = dialog.read(cx).name_focus(cx);
        let content = dialog.clone();
        let footer = dialog.clone();
        let confirm = dialog;
        window.open_dialog(cx, move |card, _, cx| {
            let zoom = Settings::get(cx).appearance.zoom();
            let content = content.clone();
            let footer = footer.clone();
            let confirm = confirm.clone();
            card.title(t!("history_ops.tag.dialog").to_string())
                .w(px(440.0 * zoom))
                .content(move |body, _, _| body.child(content.clone()))
                .footer(
                    DialogFooter::new()
                        .child(
                            ui::button("tag-cancel", t!("common.cancel").to_string())
                                .size(ui::ButtonSize::Medium)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            ui::button("tag-create", t!("history_ops.tag.dialog").to_string())
                                .size(ui::ButtonSize::Medium)
                                .on_click(move |_, window, cx| {
                                    footer.update(cx, |dialog, cx| dialog.submit(window, cx));
                                }),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    // The dialog closes itself once the operation is on its
                    // way, so the confirm does not close it a second time.
                    confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                    false
                })
        });
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    /// Runs the validated tag form. A message makes the tag annotated.
    fn create_tag(
        &mut self,
        target: Oid,
        name: String,
        message: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = if message.is_some() {
            t!("history_ops.tag.op_annotated", name = name).to_string()
        } else {
            t!("history_ops.tag.op", name = name).to_string()
        };
        self.run_operation(
            Operation::CreateTag {
                name,
                target,
                message,
            },
            label,
            window,
            cx,
        );
    }

    /// Asks before deleting a tag; the commit it points at is not touched.
    pub(crate) fn confirm_delete_tag(
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
        ui::confirm(
            window,
            cx,
            ui::Confirm::new(
                t!("history_ops.tag.delete_title", name = name).to_string(),
                t!("history_ops.tag.delete_detail").to_string(),
            )
            .ok_label(t!("history_ops.tag.delete_button").to_string()),
            move |window, cx| {
                let name = name.clone();
                let _ = entity.update(cx, |view, cx| {
                    let label = t!("history_ops.tag.delete_op", name = name).to_string();
                    view.run_operation(Operation::DeleteTag { name }, label, window, cx);
                });
            },
        );
    }

    /// What the edit entries may do to `commit`, or `None` without a session.
    fn edit_context(&self, commit: Oid) -> Option<EditContext> {
        EditContext::read(self.session.as_deref()?, commit)
    }

    /// Remote-tracking branches that already contain one of `commits`.
    ///
    /// This is the same check that warns before a history edit; a rebase onto
    /// another ref builds its range without an edit action, so it asks here.
    pub(super) fn pushed_remotes_for(&self, commits: &[Oid]) -> Vec<String> {
        let Some(session) = self.session.as_ref() else {
            return Vec::new();
        };
        // The walk only needs HEAD's own context (remote tips and parents), so
        // it starts at HEAD and stops two commits down.
        let head = head_target(session.snapshot().head.as_ref());
        head.and_then(|head| self.edit_context(head))
            .map(|edit| edit.pushed_remotes_in(commits))
            .unwrap_or_default()
    }

    /// Runs one menu pick: an editor for a reword, a confirmation for a drop,
    /// and a direct run for the other three unless history was pushed.
    pub(crate) fn open_edit(
        &mut self,
        action: EditAction,
        commit: Oid,
        summary: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.history_menu_state() else {
            return;
        };
        let Some(edit) = self.edit_context(commit) else {
            return;
        };
        // The menu already disabled this entry; the check keeps a stale click
        // from running git after the repository moved under it.
        if edit_blocked(&state, Some(&edit), action).is_some() {
            return;
        }
        match action {
            EditAction::Reword => self.open_reword(commit, &edit, window, cx),
            EditAction::Drop => self.confirm_drop(commit, &summary, &edit, &state, window, cx),
            _ => self.confirm_or_run_edit(action, commit, &edit, window, cx),
        }
    }

    /// Opens the interactive-rebase dialog for `commit` and everything after it.
    fn open_edit_history(&mut self, commit: Oid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.history_menu_state() else {
            return;
        };
        let Some(edit) = self.edit_context(commit) else {
            return;
        };
        // Same guard as the single edits: a stale click must not open a dialog
        // for a repository that has since moved.
        if edit_entry_blocked(&state, Some(&edit), EditEntry::History).is_some() {
            return;
        }
        let Some((onto, commits)) = edit.plan(EditAction::Reword) else {
            return;
        };
        let request = RebaseRequest {
            onto,
            head: edit.head,
            commits: commits
                .into_iter()
                .map(|commit| (commit, edit.summary(commit)))
                .collect(),
            pushed: edit.pushed_remotes(EditAction::Reword),
            published: edit.upstream_containing(commit),
            repo: edit.repo.clone(),
        };
        rebase_dialog::open(self, request, window, cx);
    }

    /// Opens the message editor with the commit's full message filled in.
    fn open_reword(
        &mut self,
        commit: Oid,
        edit: &EditContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let message = match session.reader().commit_message(commit) {
            Ok(message) => message,
            Err(error) => {
                self.status_error = Some(
                    t!(
                        "history_ops.read_commit_failed",
                        id = short_oid(commit),
                        error = error
                    )
                    .to_string(),
                );
                cx.notify();
                return;
            }
        };
        let warning = published_warning(edit.upstream_containing(commit).as_deref());
        edit_dialog::open(
            self,
            commit,
            short_oid(commit),
            message,
            warning,
            window,
            cx,
        );
    }

    /// Asks before leaving a commit out of the branch.
    fn confirm_drop(
        &mut self,
        commit: Oid,
        summary: &str,
        edit: &EditContext,
        state: &MenuState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let above = edit.distance.unwrap_or(0);
        let branch = state.branch_label();
        let detail = match above {
            0 => t!(
                "history_ops.drop_detail",
                summary = summary,
                branch = branch
            )
            .to_string(),
            1 => t!(
                "history_ops.drop_rewrites.one",
                summary = summary,
                branch = branch
            )
            .to_string(),
            count => t!(
                "history_ops.drop_rewrites.other",
                summary = summary,
                branch = branch,
                count = count
            )
            .to_string(),
        };
        let detail = with_pushed_warning(detail, edit.upstream_containing(commit).as_deref());
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        ui::confirm(
            window,
            cx,
            ui::Confirm::new(EditAction::Drop.title(&short_oid(commit)), detail)
                .ok_label(t!("history_ops.drop_button").to_string()),
            move |window, cx| {
                let _ = entity.update(cx, |view, cx| {
                    view.run_edit(EditAction::Drop, commit, None, window, cx)
                });
            },
        );
    }

    /// Runs a squash, fixup or move. A confirmation opens when the range is
    /// already on a remote-tracking branch, or when the commit is on the
    /// branch's upstream and publishing the rewrite needs a force push.
    fn confirm_or_run_edit(
        &mut self,
        action: EditAction,
        commit: Oid,
        edit: &EditContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let remotes = edit.pushed_remotes(action);
        let published = edit.upstream_containing(commit);
        if remotes.is_empty() && published.is_none() {
            self.run_edit(action, commit, None, window, cx);
            return;
        }
        let mut detail = String::new();
        if !remotes.is_empty() {
            detail = t!("history_ops.force_push_note", remote = remotes.join(", ")).to_string();
        }
        if let Some(warning) = published_warning(published.as_deref()) {
            if !detail.is_empty() {
                detail.push_str("\n\n");
            }
            detail.push_str(&warning);
        }
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        ui::confirm(
            window,
            cx,
            ui::Confirm::new(action.title(&short_oid(commit)), detail)
                .ok_label(t!("history_ops.rewrite").to_string())
                .always_ask(),
            move |window, cx| {
                let _ = entity.update(cx, |view, cx| {
                    view.run_edit(action, commit, None, window, cx)
                });
            },
        );
    }

    /// Builds the rebase todo for `action` and hands it to the session, which
    /// runs it like every other mutation.
    pub(crate) fn run_edit(
        &mut self,
        action: EditAction,
        commit: Oid,
        message: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.history_menu_state() else {
            return;
        };
        let Some(edit) = self.edit_context(commit) else {
            return;
        };
        if let Some(reason) = edit_blocked(&state, Some(&edit), action) {
            self.status_error = Some(
                t!(
                    "history_ops.cannot_action",
                    action = action.verb(),
                    target = short_oid(commit),
                    reason = reason
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        // A reword only replaces one message, so a merge in the range or a root
        // commit is rebuilt with `git commit-tree` instead of replayed.
        let message = if action == EditAction::Reword {
            let message = message.unwrap_or_default();
            if message.trim().is_empty() {
                self.status_error = Some(t!("history_ops.empty_message").to_string());
                cx.notify();
                return;
            }
            // A clean linear reword still replays, so the user's commit-msg
            // hook runs as it would for `git rebase -i`; a dirty worktree
            // rules the replay out.
            if edit.view().reword_needs_rebuild() || state.dirty {
                if let Some(distance) = edit.distance {
                    self.edit_selection = Some(PendingSelection {
                        old_head: edit.head,
                        distance,
                    });
                }
                let label = format!("{} {}", action.verb(), short_oid(commit));
                self.run_operation(
                    Operation::RewordAcrossMerges { commit, message },
                    label,
                    window,
                    cx,
                );
                return;
            }
            Some(message)
        } else {
            None
        };
        if edit.view().needs_rebase_merges(action) {
            let across = match action {
                EditAction::Drop => AcrossMergesAction::Drop,
                EditAction::Squash => AcrossMergesAction::Squash,
                EditAction::Fixup => AcrossMergesAction::Fixup,
                EditAction::MoveUp => AcrossMergesAction::Move(MoveDirection::Up),
                EditAction::MoveDown => AcrossMergesAction::Move(MoveDirection::Down),
                EditAction::Reword => return,
            };
            // Recreated merges get new ids, and a merged-in commit has no
            // distance on the line, so the selection is not carried over.
            let label = format!("{} {}", action.verb(), short_oid(commit));
            self.run_operation(
                Operation::RewriteAcrossMerges {
                    commit,
                    action: across,
                },
                label,
                window,
                cx,
            );
            return;
        }
        let Some((onto, commits)) = edit.plan(action) else {
            return;
        };
        let steps = match action {
            EditAction::Reword => reword_commit(&commits, commit, message.unwrap_or_default()),
            EditAction::Squash => squash_commit_into_parent(&commits, commit),
            EditAction::Fixup => fixup_steps(&commits, commit),
            EditAction::Drop => drop_commit(&commits, commit),
            EditAction::MoveUp => move_commit(&commits, commit, MoveDirection::Up),
            EditAction::MoveDown => move_commit(&commits, commit, MoveDirection::Down),
        };
        let steps = match steps {
            Ok(steps) => steps,
            Err(error) => {
                self.status_error = Some(
                    t!("history_ops.cannot", action = action.verb(), error = error).to_string(),
                );
                cx.notify();
                return;
            }
        };
        if let Some(distance) = edit
            .distance
            .and_then(|distance| selection_distance(distance, action))
        {
            self.edit_selection = Some(PendingSelection {
                old_head: edit.head,
                distance,
            });
        }
        let label = format!("{} {}", action.verb(), short_oid(commit));
        self.run_operation(Operation::RebaseEdit { onto, steps }, label, window, cx);
    }

    /// Follows a rewritten commit to its new position once the reload that
    /// carries the rewrite has landed. HEAD changes with every rewrite, which
    /// is what tells the new snapshot from the one published before it.
    pub(super) fn settle_edit_selection(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.edit_selection else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            self.edit_selection = None;
            return;
        };
        let snapshot = session.snapshot();
        let Some(head) = head_target(snapshot.head.as_ref()) else {
            return;
        };
        if head == pending.old_head {
            return;
        }
        if let Some(index) = row_at_distance(&snapshot, head, pending.distance) {
            self.select(Some(index), cx);
        }
        self.edit_selection = None;
    }
}

/// The create-tag form: a name, an optional annotation and the last error.
///
/// It is its own entity so the form state lives as long as the dialog, without
/// `RepoView` needing a field for a dialog that is open only briefly.
pub(crate) struct TagDialog {
    /// Weak: the dialog outlives a `RepoView` the workspace has replaced.
    view: WeakEntity<RepoView>,
    /// Own id, so a failed submit can repaint the form without borrowing the
    /// entity it is already updating.
    entity: EntityId,
    target: Oid,
    short: String,
    name: Entity<InputState>,
    message: Entity<InputState>,
    existing: Vec<String>,
    error: Option<String>,
    _events: Vec<Subscription>,
}

impl TagDialog {
    fn new(
        view: WeakEntity<RepoView>,
        target: Oid,
        short: String,
        existing: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("history_ops.tag.placeholder").to_string())
        });
        let message = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("history_ops.tag.annotation").to_string())
        });
        let events = [&name, &message]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this, _input, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        // A corrected name must stop reading as wrong.
                        if this.error.is_some() {
                            this.error = None;
                            cx.notify();
                        }
                    }
                    // The input owns `Enter` while it has focus, which is where
                    // the user starts typing; the footer button still works.
                    InputEvent::PressEnter {
                        secondary: false, ..
                    } => {
                        let Some(handle) = cx.active_window() else {
                            return;
                        };
                        let _ = handle.update(cx, |_, window, cx| this.submit(window, cx));
                    }
                    _ => {}
                })
            })
            .collect();
        Self {
            view,
            entity: cx.entity_id(),
            target,
            short,
            name,
            message,
            existing,
            error: None,
            _events: events,
        }
    }

    fn name_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.name.focus_handle(cx)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut App) {
        let name = self.name.read(cx).value().trim().to_owned();
        if let Err(message) = validate_tag_name(&name, &self.existing) {
            self.error = Some(message);
            cx.notify(self.entity);
            return;
        }
        let message = self.message.read(cx).value().trim().to_owned();
        let message = (!message.is_empty()).then_some(message);
        let target = self.target;
        let _ = self.view.update(cx, |view, cx| {
            view.create_tag(target, name, message, window, cx);
        });
        // Closing here would drop this dialog while it is being updated.
        window.defer(cx, |window, cx| window.close_dialog(cx));
    }
}

impl Render for TagDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let mut form = div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child(t!("history_ops.tag.target", short = self.short)),
            )
            .child(Input::new(&self.name))
            .child(Input::new(&self.message));
        if let Some(error) = &self.error {
            form = form.child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.remove_text)
                    .child(error.clone()),
            );
        }
        form
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn branch(short_name: &str, target: Oid) -> HeadInfo {
        HeadInfo::Branch {
            short_name: short_name.into(),
            target: Some(target),
        }
    }

    fn empty_snapshot() -> RepoSnapshot {
        RepoSnapshot {
            generation: 0,
            head: None,
            refs: Arc::new(Vec::new()),
            refs_by_commit: Arc::new(std::collections::HashMap::new()),
            commits: lamina_core::session::CommitList::default(),
            commit_index: Arc::new(std::collections::HashMap::new()),
            status: Arc::new(WorktreeStatus::default()),
            stashes: Arc::new(Vec::new()),
            upstreams: Arc::new(std::collections::HashMap::new()),
            remotes: Vec::new(),
            state: RepoState::Normal,
            conflicts: Arc::new(Vec::new()),
            busy: None,
        }
    }

    #[test]
    fn an_ssh_ancestry_walk_never_reads_the_wire() {
        let snapshot = empty_snapshot();
        // A remote reader is a round trip that would hold the frame the menu
        // opens in, so a commit the snapshot does not carry stays unknown.
        assert_eq!(
            parents_in(&snapshot, true, oid(3), || panic!(
                "an ssh tab must not read parents from the host"
            )),
            None
        );
        // A local tab still reads the object through gix for one the loaded
        // snapshot left out.
        assert_eq!(
            parents_in(&snapshot, false, oid(3), || Some(vec![oid(4)])),
            Some(vec![oid(4)])
        );
    }

    #[test]
    fn the_checked_out_branch_only_offers_its_rename() {
        assert_eq!(
            branch_actions(RefKind::LocalBranch, true, false, false),
            vec![BranchAction::Rename, BranchAction::Solo],
            "checking out, merging or rebasing a branch onto itself is a no-op, and HEAD always walks"
        );
        assert_eq!(
            branch_actions(RefKind::LocalBranch, false, false, false),
            vec![
                BranchAction::Checkout,
                BranchAction::Merge,
                BranchAction::Rebase,
                BranchAction::Rename,
                BranchAction::Delete,
                BranchAction::Hide,
                BranchAction::Solo,
            ]
        );
        assert_eq!(
            branch_actions(RefKind::RemoteBranch, false, false, false),
            vec![
                BranchAction::Checkout,
                BranchAction::Merge,
                BranchAction::Rebase,
                BranchAction::Hide,
                BranchAction::Solo,
            ],
            "a remote branch cannot be renamed or deleted from here"
        );
        assert!(branch_actions(RefKind::Tag, false, false, false).is_empty());
    }

    #[test]
    fn a_hidden_branch_offers_the_way_back_instead_of_hiding_again() {
        assert_eq!(
            branch_actions(RefKind::LocalBranch, false, true, true),
            vec![
                BranchAction::Checkout,
                BranchAction::Merge,
                BranchAction::Rebase,
                BranchAction::Rename,
                BranchAction::Delete,
                BranchAction::Solo,
                BranchAction::ShowAll,
            ]
        );
        assert!(
            branch_actions(RefKind::LocalBranch, false, false, true)
                .contains(&BranchAction::ShowAll),
            "another hidden branch still offers the way back"
        );
        assert!(
            !branch_actions(RefKind::LocalBranch, false, false, false)
                .contains(&BranchAction::ShowAll),
            "nothing is hidden, so there is nothing to show"
        );
    }

    #[test]
    fn a_detached_head_blocks_the_actions_that_need_a_branch() {
        let state = MenuState::read(
            Some(&HeadInfo::Detached { target: oid(1) }),
            RepoState::Normal,
            false,
        );
        assert_eq!(state.branch_label(), "HEAD");
        for action in [
            HistoryAction::CherryPick,
            HistoryAction::Revert,
            HistoryAction::Reset,
        ] {
            assert_eq!(
                state.blocked(action),
                Some(t!("reflog.head_detached").to_string()),
                "{action:?} needs a branch"
            );
        }
        for action in [
            HistoryAction::CreateTag,
            HistoryAction::DeleteTag,
            HistoryAction::CreateBranch,
            HistoryAction::CheckoutCommit,
        ] {
            assert_eq!(state.blocked(action), None, "{action:?} needs no branch");
        }
    }

    #[test]
    fn a_remote_repository_keeps_tags_and_rewrites_unavailable() {
        let head = branch("main", oid(2));
        let reason = crate::remote_open::unavailable_reason();
        let state = MenuState::read(Some(&head), RepoState::Normal, false).with_ssh(true);
        assert_eq!(state.shared_reason(), None);
        assert_eq!(state.blocked(HistoryAction::Reset), None);
        assert_eq!(state.blocked(HistoryAction::CreateBranch), None);
        assert_eq!(
            state.blocked(HistoryAction::CreateTag).as_deref(),
            Some(reason.as_str())
        );
        assert_eq!(
            state.blocked(HistoryAction::DeleteTag).as_deref(),
            Some(reason.as_str())
        );
        // Picking HEAD is the same rule as a local repository.
        assert_eq!(
            state.blocked_for(HistoryAction::CherryPick, oid(2)),
            Some(t!("history_ops.reason.is_head").to_string())
        );
        // Revert of another commit is on the wire. A silent menu click was a
        // dialog that the closing menu discarded, not a remote block.
        assert_eq!(state.blocked_for(HistoryAction::Revert, oid(1)), None);
        assert_eq!(state.blocked_for(HistoryAction::CherryPick, oid(1)), None);
        // Detaching is a checkout the helper can run. A remote tab must not
        // hide it the way it hides tags.
        assert_eq!(state.blocked(HistoryAction::CheckoutCommit), None);
        let line = line_of(&[oid(1), oid(2)]);
        assert_eq!(
            edit_blocked_on_line(&state, view_at(&line, Some(0)), EditAction::Reword).as_deref(),
            Some(reason.as_str())
        );
        // The remote reason still wins over busy for a tag.
        let busy = MenuState::read(Some(&head), RepoState::Merging, true).with_ssh(true);
        assert_eq!(
            busy.blocked(HistoryAction::CreateTag).as_deref(),
            Some(reason.as_str())
        );
        assert_eq!(
            busy.shared_reason(),
            Some(t!("branch.reason.busy").to_string())
        );
    }

    #[test]
    fn an_operation_in_progress_or_running_blocks_history_edits() {
        let head = branch("main", oid(2));
        let pending = MenuState::read(Some(&head), RepoState::Merging, false);
        assert_eq!(
            pending.blocked(HistoryAction::Reset),
            Some(t!("reflog.finish_first").to_string())
        );
        assert_eq!(
            pending.blocked(HistoryAction::CreateTag),
            None,
            "a tag can point into the past while a merge runs"
        );
        assert_eq!(
            pending.blocked(HistoryAction::CheckoutCommit),
            Some(t!("reflog.finish_first").to_string()),
            "detaching during a merge would move HEAD out from under it"
        );

        let busy = MenuState::read(Some(&head), RepoState::Normal, true);
        for action in [
            HistoryAction::CherryPick,
            HistoryAction::Revert,
            HistoryAction::Reset,
            HistoryAction::CreateTag,
            HistoryAction::DeleteTag,
            HistoryAction::CreateBranch,
            HistoryAction::CheckoutCommit,
        ] {
            assert_eq!(
                busy.blocked(action),
                Some(t!("branch.reason.busy").to_string()),
                "{action:?} must wait for the worker"
            );
        }
    }

    #[test]
    fn picking_or_reverting_head_itself_is_disabled() {
        let head = branch("main", oid(2));
        let state = MenuState::read(Some(&head), RepoState::Normal, false);
        assert_eq!(
            state.blocked_for(HistoryAction::CherryPick, oid(2)),
            Some(t!("history_ops.reason.is_head").to_string())
        );
        assert_eq!(
            state.blocked_for(HistoryAction::Revert, oid(2)),
            Some(t!("history_ops.reason.is_head").to_string())
        );
        assert_eq!(
            state.blocked_for(HistoryAction::Reset, oid(2)),
            None,
            "resetting to HEAD is how local changes are discarded"
        );
        assert_eq!(state.blocked_for(HistoryAction::CherryPick, oid(3)), None);
        assert_eq!(state.branch_label(), "main");
    }

    #[test]
    fn a_loading_repository_says_there_is_no_branch_yet() {
        let state = MenuState::read(None, RepoState::Normal, false);
        assert_eq!(
            state.blocked(HistoryAction::Reset),
            Some(t!("history_ops.no_branch_to_move").to_string())
        );
        assert_eq!(
            state.blocked(HistoryAction::CheckoutCommit),
            Some(t!("history_ops.no_branch_to_move").to_string())
        );
    }

    #[test]
    fn a_disabled_entry_says_why() {
        assert_eq!(
            menu_label(t!("history_ops.menu.revert").to_string(), None::<&str>),
            t!("history_ops.menu.revert").to_string()
        );
        assert_eq!(
            menu_label(
                t!("history_ops.menu.revert").to_string(),
                Some(t!("history_ops.reason.is_head").to_string())
            ),
            "Revert commit (this is HEAD)"
        );
    }

    #[test]
    fn the_menu_wide_reason_is_not_repeated_on_every_entry() {
        let head = branch("main", oid(2));
        let pending = MenuState::read(Some(&head), RepoState::Merging, false);
        let shared = pending.shared_reason();
        let finish = t!("reflog.finish_first").to_string();
        assert_eq!(shared.as_deref(), Some(finish.as_str()));
        assert_eq!(
            blocked_label("Reset", Some(finish.as_str()), shared.as_deref()),
            "Reset",
            "the reason is already in the menu header"
        );
        assert_eq!(
            blocked_label("Revert", Some("this is HEAD"), shared.as_deref()),
            "Revert (this is HEAD)",
            "a different reason still belongs to its entry"
        );

        let busy = MenuState::read(Some(&head), RepoState::Normal, true);
        assert_eq!(
            busy.shared_reason(),
            Some(t!("branch.reason.busy").to_string())
        );

        let normal = MenuState::read(Some(&head), RepoState::Normal, false);
        assert_eq!(normal.shared_reason(), None);
    }

    #[test]
    fn only_actions_that_move_head_wait_for_an_unfinished_operation() {
        for action in [
            BranchAction::Checkout,
            BranchAction::Merge,
            BranchAction::Rebase,
        ] {
            assert!(action.needs_idle_repository(), "{action:?} moves HEAD");
        }
        for action in [
            BranchAction::Rename,
            BranchAction::Delete,
            BranchAction::Solo,
            BranchAction::Hide,
            BranchAction::ShowAll,
        ] {
            assert!(
                !action.needs_idle_repository(),
                "{action:?} only edits refs"
            );
        }
    }

    #[test]
    fn a_hard_reset_names_the_uncommitted_paths_it_discards() {
        let clean = reset_detail("main", "abc1234", ResetMode::Hard, 0, 0);
        assert!(clean.contains("no uncommitted changes"), "{clean}");
        let dirty = reset_detail("main", "abc1234", ResetMode::Hard, 1, 2);
        assert!(dirty.contains("3 uncommitted paths"), "{dirty}");
        let one = reset_detail("main", "abc1234", ResetMode::Hard, 1, 0);
        assert!(one.contains("1 uncommitted path "), "{one}");

        assert!(
            reset_detail("main", "abc1234", ResetMode::Soft, 3, 4).contains("stay staged"),
            "a soft reset keeps the changes staged"
        );
        assert!(
            reset_detail("main", "abc1234", ResetMode::Mixed, 3, 4).contains("unstaged"),
            "a mixed reset keeps the changes in the worktree"
        );
    }

    #[test]
    fn tag_names_follow_git_ref_rules() {
        for name in ["v1.0.0", "release-2026", "team/1.2", "a.b"] {
            assert_eq!(
                validate_tag_name(name, &[]),
                Ok(()),
                "{name} should be valid"
            );
        }

        for name in [
            "", " ", "v 1", "v\t1", "v..1", "v~1", "v^1", "v:1", "v?", "v*", "v[1]", "v\\1", "-v1",
            "/v1", "v1/", "v//1", "v@{1}", ".v1", "v1.lock", "v1.", "v./1", "@",
        ] {
            assert!(
                validate_tag_name(name, &[]).is_err(),
                "{name} should be rejected"
            );
        }

        assert_eq!(
            validate_tag_name("v1.0.0", &["v1.0.0".to_owned()]),
            Err(t!("history_ops.tag.exists").to_string()),
            "a tag name has to be free"
        );
    }

    /// A linear first-parent line: each commit's only parent is the next.
    fn line_of(ids: &[Oid]) -> Vec<LineCommit> {
        ids.iter()
            .enumerate()
            .map(|(index, id)| {
                let parents = ids.get(index + 1).copied().into_iter().collect();
                (*id, parents)
            })
            .collect()
    }

    fn worktree(kind: lamina_core::ChangeKind, staged: bool) -> WorktreeStatus {
        let change = lamina_core::FileChange {
            path: "file.txt".into(),
            old_path: None,
            kind,
        };
        if staged {
            WorktreeStatus {
                staged: vec![change],
                unstaged: Vec::new(),
            }
        } else {
            WorktreeStatus {
                staged: Vec::new(),
                unstaged: vec![change],
            }
        }
    }

    #[test]
    fn the_first_parent_walk_finds_a_commit_and_keeps_two_parents() {
        let ids = [oid(1), oid(2), oid(3), oid(4)];
        let parents = |id: Oid| {
            let index = ids.iter().position(|candidate| *candidate == id)?;
            Some(ids.get(index + 1).copied().into_iter().collect())
        };

        let (line, distance) =
            walk_first_parents(ids[0], ids[2], parents).expect("the line is readable");
        assert_eq!(distance, Some(2));
        assert_eq!(
            line.iter().map(|(commit, _)| *commit).collect::<Vec<_>>(),
            ids,
            "the target and the two commits below it are kept"
        );

        let (_, distance) =
            walk_first_parents(ids[0], oid(9), parents).expect("the line is readable");
        assert_eq!(
            distance, None,
            "a commit off the first-parent line is not found"
        );
        assert!(
            walk_first_parents(ids[0], ids[2], |_| None).is_none(),
            "an unreadable commit asks for the repository fallback"
        );
    }

    #[test]
    fn every_action_starts_after_the_commit_it_rewrites() {
        let ids = [oid(1), oid(2), oid(3), oid(4), oid(5), oid(6)];
        let line = line_of(&ids);
        let distance = 2;

        // Reword, drop, and moving up only replay the selected commit and the
        // commits above it; the parent stays outside the range.
        for action in [EditAction::Reword, EditAction::Drop, EditAction::MoveUp] {
            let onto = onto_index(distance, action).expect("an onto commit");
            assert_eq!(line[onto].0, oid(4), "{action:?} starts after the parent");
            assert_eq!(
                range_commits(&line, onto),
                [oid(3), oid(2), oid(1)],
                "{action:?} replays the range oldest first"
            );
        }

        // A squash rewrites its parent, and moving down swaps with the older
        // commit, so both start one commit deeper.
        for action in [EditAction::Squash, EditAction::Fixup, EditAction::MoveDown] {
            let onto = onto_index(distance, action).expect("an onto commit");
            assert_eq!(line[onto].0, oid(5), "{action:?} includes the parent");
            assert_eq!(
                range_commits(&line, onto),
                [oid(4), oid(3), oid(2), oid(1)],
                "{action:?} replays the parent too"
            );
        }

        assert_eq!(
            onto_index(0, EditAction::MoveUp),
            None,
            "the newest commit cannot move up"
        );
        assert_eq!(selection_distance(2, EditAction::Reword), Some(2));
        assert_eq!(
            selection_distance(2, EditAction::Squash),
            Some(2),
            "the combined commit takes the selected commit's place"
        );
        assert_eq!(selection_distance(2, EditAction::Drop), Some(2));
        assert_eq!(selection_distance(2, EditAction::MoveUp), Some(1));
        assert_eq!(selection_distance(2, EditAction::MoveDown), Some(3));
    }

    /// A view of `line` with the target `distance` steps down and HEAD able to
    /// reach it.
    fn view_at(line: &[LineCommit], distance: Option<usize>) -> LineView<'_> {
        LineView::at(line, distance, true)
    }

    #[test]
    fn edit_reasons_mirror_what_core_refuses() {
        let ids = [oid(1), oid(2), oid(3), oid(4)];
        let line = line_of(&ids);

        assert_eq!(
            blocked_on_line(LineView::at(&line, None, false), EditAction::Reword),
            Some(t!("history_ops.reason.not_on_branch").to_string())
        );
        assert_eq!(
            blocked_on_line(LineView::at(&line, None, true), EditAction::Reword),
            Some(t!("history_ops.reason.merged_in").to_string()),
            "a merge brought the commit in, so it is reachable without being on the line"
        );
        assert_eq!(
            blocked_on_line(view_at(&line, Some(3)), EditAction::Drop),
            Some(t!("history_ops.reason.root_commit").to_string()),
            "a root has no parent to rebase onto"
        );
        assert_eq!(
            blocked_on_line(view_at(&line, Some(3)), EditAction::Reword),
            None,
            "a reword rebuilds the commit, so a root needs no onto"
        );
        assert_eq!(
            blocked_on_line(view_at(&line, Some(0)), EditAction::MoveUp),
            Some(t!("history_ops.reason.already_newest").to_string())
        );
        assert_eq!(
            blocked_on_line(view_at(&line, Some(2)), EditAction::Squash),
            Some(t!("history_ops.reason.parent_root").to_string()),
            "squashing into the root would need an onto before it"
        );
        assert_eq!(
            blocked_on_line(view_at(&line, Some(2)), EditAction::MoveDown),
            Some(t!("history_ops.reason.already_oldest").to_string())
        );
        assert_eq!(
            blocked_on_line(view_at(&line, Some(2)), EditAction::Reword),
            None
        );

        // A merge at HEAD stops every replay, but not a reword, which keeps
        // every tree and rebuilds the commits instead.
        let mut merged = line_of(&ids);
        merged[0].1.push(oid(9));
        assert_eq!(
            blocked_on_line(view_at(&merged, Some(0)), EditAction::Reword),
            None
        );
        assert_eq!(
            blocked_on_line(view_at(&merged, Some(1)), EditAction::Reword),
            None,
            "a commit behind a merge can be reworded"
        );
        assert_eq!(
            blocked_on_line(view_at(&merged, Some(1)), EditAction::Drop),
            None,
            "a drop behind a merge replays with --rebase-merges"
        );
        assert!(view_at(&merged, Some(1)).needs_rebase_merges(EditAction::Drop));
        assert_eq!(
            blocked_on_line(view_at(&merged, Some(1)), EditAction::MoveUp),
            Some(t!("history_ops.reason.next_to_merge").to_string()),
            "the neighbour above is the merge, which is not a pick to swap with"
        );
        assert_eq!(
            blocked_on_line(view_at(&merged, Some(0)), EditAction::Drop),
            Some(t!("history_ops.reason.is_merge").to_string())
        );

        // A merge at the parent is outside a reword's range but inside a
        // squash's, which rewrites the parent.
        let mut merged_parent = line_of(&ids);
        merged_parent[1].1.push(oid(9));
        assert_eq!(
            blocked_on_line(view_at(&merged_parent, Some(0)), EditAction::Reword),
            None
        );
        assert_eq!(
            blocked_on_line(view_at(&merged_parent, Some(0)), EditAction::Squash),
            Some(t!("history_ops.reason.parent_merge").to_string())
        );
        assert_eq!(
            blocked_on_line(view_at(&merged_parent, Some(0)), EditAction::MoveDown),
            Some(t!("history_ops.reason.next_to_merge").to_string())
        );
        // Two commits further down, the merge is only in the range.
        let five = [oid(1), oid(2), oid(3), oid(4), oid(5)];
        let mut far = line_of(&five);
        far[0].1.push(oid(9));
        let view = view_at(&far, Some(2));
        assert!(view.needs_rebase_merges(EditAction::Squash));
        assert!(!view_at(&line, Some(1)).needs_rebase_merges(EditAction::Squash));
        for action in [
            EditAction::Squash,
            EditAction::Fixup,
            EditAction::MoveUp,
            EditAction::MoveDown,
            EditAction::Drop,
        ] {
            assert_eq!(blocked_on_line(view, action), None, "{action:?}");
        }
    }

    #[test]
    fn a_merged_in_commit_is_edited_within_its_own_branch() {
        let line = line_of(&[oid(1), oid(2), oid(3)]);
        let side = SideView {
            is_merge: false,
            parent_is_merge: false,
            parent_on_line: false,
        };
        let view = LineView::at(&line, None, true).with_side(side);
        assert!(view.needs_rebase_merges(EditAction::Drop));
        for action in [
            EditAction::Drop,
            EditAction::Squash,
            EditAction::Fixup,
            EditAction::MoveUp,
            EditAction::MoveDown,
        ] {
            assert_eq!(blocked_on_line(view, action), None, "{action:?}");
        }
        assert_eq!(
            blocked_on_line(view, EditAction::Reword),
            Some(t!("history_ops.reason.merged_in").to_string()),
            "the reword rebuild only follows the first-parent line"
        );

        let first = LineView::at(&line, None, true).with_side(SideView {
            parent_on_line: true,
            ..side
        });
        assert_eq!(blocked_on_line(first, EditAction::Drop), None);
        assert_eq!(
            blocked_on_line(first, EditAction::Squash),
            Some(t!("history_ops.reason.parent_outside").to_string())
        );
        assert_eq!(
            blocked_on_line(first, EditAction::MoveDown),
            Some(t!("history_ops.reason.already_oldest").to_string())
        );

        let after_merge = LineView::at(&line, None, true).with_side(SideView {
            parent_is_merge: true,
            ..side
        });
        assert_eq!(
            blocked_on_line(after_merge, EditAction::Fixup),
            Some(t!("history_ops.reason.parent_merge").to_string())
        );

        let merge = LineView::at(&line, None, true).with_side(SideView {
            is_merge: true,
            ..side
        });
        assert_eq!(
            blocked_on_line(merge, EditAction::Drop),
            Some(t!("history_ops.reason.is_merge").to_string())
        );
    }

    #[test]
    fn the_history_dialog_needs_a_range_a_replay_can_take() {
        let state = MenuState::read(Some(&branch("main", oid(1))), RepoState::Normal, false);
        let line = line_of(&[oid(1), oid(2), oid(3)]);
        assert_eq!(
            edit_entry_blocked_on_line(&state, view_at(&line, Some(0)), EditEntry::History),
            None
        );

        let mut merged = line_of(&[oid(1), oid(2), oid(3)]);
        merged[0].1.push(oid(9));
        assert_eq!(
            edit_entry_blocked_on_line(&state, view_at(&merged, Some(1)), EditEntry::History),
            Some(t!("history_ops.reason.merge").to_string()),
            "the dialog replays with `git rebase -i`, which a merge stops"
        );

        let root = line_of(&[oid(1), oid(2)]);
        assert_eq!(
            edit_entry_blocked_on_line(&state, view_at(&root, Some(1)), EditEntry::History),
            Some(t!("history_ops.reason.parent_root").to_string()),
            "the dialog has no onto commit below a root"
        );
    }

    #[test]
    fn the_edit_entry_collapses_only_when_every_row_shares_a_reason() {
        let state = MenuState::read(Some(&branch("main", oid(1))), RepoState::Normal, false);
        let line = line_of(&[oid(1), oid(2), oid(3)]);

        // (a) a commit on another branch blocks every row for the same reason.
        assert_eq!(
            collapsed_edit_reason_on_line(&state, LineView::at(&line, None, false)),
            Some(t!("history_ops.reason.not_on_branch").to_string())
        );
        // (b) a merged-in commit does too.
        assert_eq!(
            collapsed_edit_reason_on_line(&state, LineView::at(&line, None, true)),
            Some(t!("history_ops.reason.merged_in").to_string())
        );
        // A head commit has a reword and a move available, so the submenu
        // opens and only the inapplicable rows are disabled.
        assert_eq!(
            collapsed_edit_reason_on_line(&state, view_at(&line, Some(0))),
            None
        );

        let busy = MenuState::read(Some(&branch("main", oid(1))), RepoState::Normal, true);
        assert_eq!(
            collapsed_edit_reason_on_line(&busy, view_at(&line, Some(0))),
            Some(t!("branch.reason.busy").to_string())
        );

        // The disabled entry carries that one reason in its label.
        let reason = collapsed_edit_reason_on_line(&state, LineView::at(&line, None, true));
        assert_eq!(
            blocked_label(
                t!("history_ops.menu.edit").to_string(),
                reason.as_deref(),
                None
            ),
            format!("Edit ({})", t!("history_ops.reason.merged_in"))
        );
    }

    #[test]
    fn a_published_commit_warns_that_the_rewrite_needs_a_force_push() {
        assert_eq!(
            published_warning(Some("origin/main")),
            Some(
                "This commit is already on origin/main. Publishing the change needs a force push."
                    .to_owned()
            )
        );
        assert_eq!(published_warning(None), None);

        assert_eq!(with_pushed_warning("Body".to_owned(), None), "Body");
        let warned = with_pushed_warning("Body".to_owned(), Some("origin/main"));
        assert!(
            warned.starts_with("Body\n\n"),
            "the note is its own paragraph: {warned}"
        );
        assert!(
            warned.contains("already on origin/main"),
            "the note names the upstream: {warned}"
        );

        // Every rewrite confirmation and the history dialog show the same
        // note, so the wording is chosen once.
        for entry in EditEntry::ALL {
            let warned = with_pushed_warning(entry.label(), Some("upstream/main"));
            assert!(
                warned.ends_with("needs a force push."),
                "{entry:?} carries the note: {warned}"
            );
        }
    }

    #[test]
    fn repository_state_reasons_come_before_the_commit_rules() {
        let head = branch("main", oid(1));
        let line = line_of(&[oid(1), oid(2), oid(3)]);

        let pending = MenuState::read(Some(&head), RepoState::Rebasing, false);
        assert_eq!(
            edit_blocked_on_line(&pending, view_at(&line, Some(0)), EditAction::Reword),
            Some(t!("reflog.finish_first").to_string())
        );

        let busy = MenuState::read(Some(&head), RepoState::Normal, true);
        assert_eq!(
            edit_blocked_on_line(&busy, view_at(&line, Some(0)), EditAction::Reword),
            Some(t!("branch.reason.busy").to_string())
        );

        let detached = MenuState::read(
            Some(&HeadInfo::Detached { target: oid(1) }),
            RepoState::Normal,
            false,
        );
        assert_eq!(
            edit_blocked_on_line(&detached, view_at(&line, Some(0)), EditAction::Reword),
            Some(t!("reflog.head_detached").to_string())
        );
    }

    #[test]
    fn tracked_changes_block_a_replay_but_not_a_rebuilt_reword() {
        let head = branch("main", oid(1));
        let read = |status: WorktreeStatus| {
            MenuState::read(Some(&head), RepoState::Normal, false).with_worktree(&status)
        };
        let line = line_of(&[oid(1), oid(2), oid(3)]);

        let clean = read(WorktreeStatus::default());
        assert!(!clean.dirty);
        assert_eq!(
            edit_blocked_on_line(&clean, view_at(&line, Some(0)), EditAction::Reword),
            None
        );

        let untracked = read(worktree(lamina_core::ChangeKind::Untracked, false));
        assert!(!untracked.dirty, "untracked files do not block a rebase");

        let staged = read(worktree(lamina_core::ChangeKind::Modified, true));
        assert!(staged.dirty);
        assert_eq!(
            edit_blocked_on_line(&staged, view_at(&line, Some(1)), EditAction::Reword),
            None,
            "a dirty linear reword is rebuilt instead of replayed"
        );

        // A reword with a merge in its range is rebuilt as well: HEAD keeps
        // its tree, so the uncommitted change stays valid.
        let mut merged = line_of(&[oid(1), oid(2), oid(3)]);
        merged[0].1.push(oid(9));
        assert_eq!(
            edit_blocked_on_line(&staged, view_at(&merged, Some(1)), EditAction::Reword),
            None
        );
        assert_eq!(
            edit_blocked_on_line(&staged, view_at(&line, Some(0)), EditAction::Drop),
            Some(t!("reflog.commit_or_stash").to_string()),
            "a drop still replays the range"
        );

        let edited = read(worktree(lamina_core::ChangeKind::Modified, false));
        assert!(edited.dirty);
    }

    #[test]
    fn pushed_remotes_are_listed_once_and_sorted() {
        let range = [oid(1), oid(2)];
        let remotes = vec![
            ("upstream/main".to_owned(), oid(8)),
            ("origin/main".to_owned(), oid(9)),
            ("origin/topic".to_owned(), oid(7)),
            ("origin/behind".to_owned(), oid(6)),
        ];

        let names = remotes_containing(&range, &remotes, |commit, tip| {
            (commit == oid(1) && (tip == oid(9) || tip == oid(7)))
                || (commit == oid(2) && tip == oid(8))
        });

        assert_eq!(
            names,
            ["origin/main", "origin/topic", "upstream/main"],
            "one entry per remote that reaches any commit of the range"
        );
        assert!(
            remotes_containing(&[], &remotes, |_, _| true).is_empty(),
            "an empty range has no pushed commits"
        );
    }
}
