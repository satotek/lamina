//! Plain rebase: replay the current branch onto another commit or branch.
//!
//! The entries live in the commit list's menu ("onto here") and in a branch
//! row's menu ("onto <branch>"). Both open the same confirmation, which says
//! how many commits will be replayed, warns when a remote already has them and
//! offers to autostash a dirty working tree. Nothing here runs git: the rebase
//! becomes `Operation::Rebase`, and conflicts stop in the repository state the
//! in-progress banner already handles.
//!
//! The enable rules depend on the menu state and the replay count alone, so
//! they are plain functions the tests exercise without a repository.

use gpui::{App, AppContext as _, ClickEvent, Context, Entity, IntoElement, Render, Window};
use gpui_component::checkbox::Checkbox;
use gpui_component::menu::PopupMenuItem;
use strata_core::session::Operation;
use strata_core::{Oid, RefKind};

use super::RepoView;
use super::history_ops::{MenuState, PendingSelection, blocked_label, head_target};
use crate::data::short_oid;
use crate::ui;

/// What one rebase menu entry offers before the user confirms it.
pub(crate) struct RebaseTarget {
    /// The commit to replay onto.
    onto: Oid,
    /// How the confirmation names the target: "commit a1b2c3d", "topic", or
    /// "origin/main".
    onto_label: String,
    /// Why the entry is disabled, or `None` when it can run.
    blocked: Option<String>,
}

/// Why a plain rebase cannot start, or `None` when it can.
///
/// `up_to_date` is whether the target is already reachable from HEAD; `None`
/// means git could not answer, which leaves the entry disabled too.
pub(crate) fn rebase_blocked(state: &MenuState, up_to_date: Option<bool>) -> Option<String> {
    if state.busy() {
        return Some(t!("branch.reason.busy").to_string());
    }
    if state.pending() {
        return Some(t!("reflog.finish_first").to_string());
    }
    if state.detached() {
        return Some(t!("reflog.head_detached").to_string());
    }
    if state.branch().is_none() {
        return Some(t!("history_ops.no_branch_to_move").to_string());
    }
    match up_to_date {
        Some(true) => Some(t!("rebase.already_up_to_date").to_string()),
        Some(false) => None,
        None => Some(t!("rebase.cannot_read_branch").to_string()),
    }
}

/// The confirmation's count line.
pub(crate) fn replay_detail(count: usize, onto_label: &str) -> String {
    match count {
        0 => t!("rebase.no_commits", onto = onto_label).to_string(),
        1 => t!("rebase.one_commit", onto = onto_label).to_string(),
        count => t!(
            &crate::i18n::count_key("rebase.commits", count),
            count = count,
            onto = onto_label
        )
        .to_string(),
    }
}

/// The warning a confirmation adds when a remote already has the range.
pub(crate) fn pushed_warning(remotes: &[String]) -> Option<String> {
    (!remotes.is_empty())
        .then(|| t!("history_ops.force_push_note", remote = remotes.join(", ")).to_string())
}

/// One menu entry for a rebase, disabled with its reason when it cannot run.
///
/// `shared` is the reason the menu already shows once at its top, if any; it is
/// not repeated after the label.
pub(crate) fn rebase_menu_item(
    target: RebaseTarget,
    branch: &str,
    entry_target: Option<&str>,
    shared: Option<&str>,
    entity: Entity<RepoView>,
) -> PopupMenuItem {
    // "here" gets its own sentence: Japanese spaces a ref name off from the
    // particles around it, which reads wrong around the word for "here".
    let text = match entry_target {
        Some(entry_target) => t!(
            "history_ops.menu.rebase",
            branch = branch,
            target = entry_target
        ),
        None => t!("history_ops.menu.rebase_here", branch = branch),
    };
    let label = blocked_label(text.to_string(), target.blocked.as_deref(), shared);
    let mut item = PopupMenuItem::new(label).disabled(target.blocked.is_some());
    if target.blocked.is_none() {
        let onto = target.onto;
        let onto_label = target.onto_label.clone();
        item = item.on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let onto_label = onto_label.clone();
            entity.update(cx, |view, cx| {
                view.confirm_rebase(onto, onto_label, window, cx)
            });
        });
    }
    item
}

impl RepoView {
    /// The rebase entry for `onto`, or `None` without a session.
    pub(crate) fn rebase_target(
        &self,
        onto: Oid,
        onto_label: impl Into<String>,
    ) -> Option<RebaseTarget> {
        let state = self.history_menu_state()?;
        let range = self.replay_range(onto);
        Some(RebaseTarget {
            onto,
            onto_label: onto_label.into(),
            blocked: rebase_blocked(&state, range.as_ref().map(|(_, up_to_date)| *up_to_date)),
        })
    }

    /// The branch-row entry: the current branch's name and the row's target.
    pub(crate) fn branch_rebase_target(
        &self,
        kind: RefKind,
        short_name: &str,
    ) -> Option<(String, RebaseTarget)> {
        let branch = self.history_menu_state()?.branch_label();
        let tip = self.branch_tip(kind, short_name)?;
        let target = self.rebase_target(tip, short_name.to_owned())?;
        Some((branch, target))
    }

    /// The tip of a local or remote branch in the snapshot.
    fn branch_tip(&self, kind: RefKind, short_name: &str) -> Option<Oid> {
        self.session
            .as_ref()?
            .snapshot()
            .refs
            .iter()
            .find(|reference| reference.kind == kind && reference.short_name == short_name)
            .map(|reference| reference.target)
    }

    /// `onto..HEAD` and whether `onto` is already reachable from HEAD.
    ///
    /// Neither read is a rebase decision on its own: an ancestor is "already
    /// up to date", while zero commits with a target ahead of HEAD still moves
    /// the branch forward.
    fn replay_range(&self, onto: Oid) -> Option<(Vec<Oid>, bool)> {
        let session = self.session.as_ref()?;
        let head = head_target(session.snapshot().head.as_ref())?;
        let repo = session.reader();
        let commits = repo.commits_between(onto, head).ok()?;
        let up_to_date = repo.is_ancestor(onto, head).ok()?;
        Some((commits, up_to_date))
    }

    /// Asks before replaying the current branch onto `onto`.
    pub(crate) fn confirm_rebase(
        &mut self,
        onto: Oid,
        onto_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.history_menu_state() else {
            return;
        };
        let Some((commits, up_to_date)) = self.replay_range(onto) else {
            self.status_error = Some(t!("rebase.cannot_read_commits").to_string());
            cx.notify();
            return;
        };
        // The menu already disabled this entry; the check keeps a stale click
        // from rewriting a branch that moved while the menu was open.
        if let Some(reason) = rebase_blocked(&state, Some(up_to_date)) {
            self.status_error = Some(t!("rebase.cannot_rebase", reason = reason).to_string());
            cx.notify();
            return;
        }

        let branch = state.branch_label();
        let mut detail = replay_detail(commits.len(), &onto_label);
        let pushed = self.pushed_remotes_for(&commits);
        if let Some(warning) = pushed_warning(&pushed) {
            detail.push(' ');
            detail.push_str(&warning);
        }
        let title = t!("rebase.confirm_title", branch = branch, onto = onto_label).to_string();
        let mut confirm = ui::Confirm::new(title, detail)
            .ok_label(t!("history_ops.menu.rebase_button").to_string());
        // Rewriting a range a remote already has has to be asked about even
        // when the settings turned confirmations off.
        if !pushed.is_empty() {
            confirm.always_ask = true;
        }

        let entity = cx.weak_entity();
        self.focus_root(window, cx);
        // A dirty tree is the only case that needs the extra control, so a
        // clean worktree gets the plain confirmation.
        if state.dirty() {
            let option = cx.new(|_| AutostashOption::new(true));
            let content = option.clone();
            let on_ok = move |window: &mut Window, cx: &mut App| {
                let autostash = option.read(cx).checked();
                let _ = entity.update(cx, |view, cx| {
                    view.start_rebase(onto, onto_label.clone(), autostash, window, cx)
                });
            };
            ui::confirm_with(
                window,
                cx,
                confirm,
                move |_, _| content.clone().into_any_element(),
                on_ok,
            );
        } else {
            let on_ok = move |window: &mut Window, cx: &mut App| {
                let _ = entity.update(cx, |view, cx| {
                    view.start_rebase(onto, onto_label.clone(), false, window, cx)
                });
            };
            ui::confirm(window, cx, confirm, on_ok);
        }
    }

    /// Runs the confirmed rebase and keeps the selection on the branch tip.
    fn start_rebase(
        &mut self,
        onto: Oid,
        onto_label: String,
        autostash: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        // The confirmation re-checked the state before it opened, but another
        // operation can still have started while the dialog was up.
        if self.is_busy() {
            return;
        }
        let Some(old_head) = head_target(session.snapshot().head.as_ref()) else {
            return;
        };
        // The tip is rewritten, so the selection follows it back once the
        // snapshot that carries the result arrives.
        self.edit_selection = Some(PendingSelection::at(old_head, 0));
        let label = t!("rebase.op", onto = onto_label, short = short_oid(old_head)).to_string();
        self.run_operation(
            Operation::Rebase {
                onto,
                upstream: None,
                autostash,
            },
            label,
            window,
            cx,
        );
    }
}

/// The "Autostash local changes" tick of the confirmation.
///
/// Its own entity because the dialog rebuilds its layer on every frame: the
/// tick has to survive that, and changing it has to repaint the checkbox.
struct AutostashOption {
    checked: bool,
}

impl AutostashOption {
    fn new(checked: bool) -> Self {
        Self { checked }
    }

    fn checked(&self) -> bool {
        self.checked
    }
}

impl Render for AutostashOption {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let on_change = cx.listener(|this, checked: &bool, _window, cx| {
            this.checked = *checked;
            cx.notify();
        });
        Checkbox::new("rebase-autostash")
            .checked(self.checked)
            .label(t!("rebase.autostash").to_string())
            .on_change(on_change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::{HeadInfo, Oid, RepoState};

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

    fn state(head: Option<&HeadInfo>, repo_state: RepoState, busy: bool) -> MenuState {
        MenuState::read(head, repo_state, busy)
    }

    #[test]
    fn a_rebase_needs_a_branch_and_a_target_that_is_not_behind_head() {
        let head = branch("topic", oid(1));
        let on_branch = state(Some(&head), RepoState::Normal, false);
        assert_eq!(rebase_blocked(&on_branch, Some(false)), None);
        assert_eq!(
            rebase_blocked(&on_branch, Some(true)).as_deref(),
            Some("already up to date"),
            "the target is already reachable from HEAD"
        );
        assert_eq!(
            rebase_blocked(&on_branch, None).as_deref(),
            Some("cannot read the branch")
        );
    }

    #[test]
    fn detached_head_busy_and_pending_block_a_rebase() {
        let detached = state(
            Some(&HeadInfo::Detached { target: oid(1) }),
            RepoState::Normal,
            false,
        );
        assert_eq!(
            rebase_blocked(&detached, Some(false)).as_deref(),
            Some("HEAD is detached")
        );

        let no_head = state(None, RepoState::Normal, false);
        assert_eq!(
            rebase_blocked(&no_head, Some(false)).as_deref(),
            Some("no branch to move")
        );

        let head = branch("topic", oid(1));
        let merging = state(Some(&head), RepoState::Merging, false);
        assert_eq!(
            rebase_blocked(&merging, Some(false)).as_deref(),
            Some("finish the operation in progress first")
        );

        let busy = state(Some(&head), RepoState::Normal, true);
        assert_eq!(
            rebase_blocked(&busy, Some(false)).as_deref(),
            Some("an operation is running")
        );
    }

    #[test]
    fn the_confirmation_counts_the_commits_it_replays() {
        assert_eq!(
            replay_detail(1, "topic"),
            "1 commit will be replayed onto topic."
        );
        assert_eq!(
            replay_detail(4, "origin/main"),
            "4 commits will be replayed onto origin/main."
        );
        assert_eq!(
            replay_detail(0, "origin/main"),
            "No commits will be replayed onto origin/main; the branch moves to it."
        );
    }

    #[test]
    fn a_pushed_range_warns_with_every_remote() {
        assert_eq!(pushed_warning(&[]), None);
        let warning = pushed_warning(&["origin/main".to_owned(), "upstream/main".to_owned()])
            .expect("a warning");
        assert!(warning.contains("origin/main, upstream/main"), "{warning}");
        assert!(warning.contains("force push"), "{warning}");
    }
}
