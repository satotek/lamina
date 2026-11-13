//! History actions on a commit: cherry-pick, revert, reset and tags.
//!
//! The commit list's context menu and the tag form live here. Nothing in this
//! module runs git: every write becomes an [`Operation`] handed to
//! [`RepoView::run_operation`], whose session worker runs it and publishes the
//! new snapshot. An operation that stops on a conflict is a normal outcome (the
//! conflict view takes over from the published state), so the handlers only
//! report failures.
//!
//! The menu's enable rules depend on the commit, HEAD and the repository state
//! alone, so they are plain functions that the unit tests exercise without a
//! repository.

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
use strata_core::session::{Operation, ResetMode};
use strata_core::{HeadInfo, Oid, RefKind, RepoState};

use super::RepoView;
use crate::settings::Settings;
use crate::style::Design;
use crate::ui;

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
        }
    }

    /// What reset, cherry-pick and revert name: the branch, or HEAD when there
    /// is no branch to move.
    pub(crate) fn branch_label(&self) -> String {
        self.branch.clone().unwrap_or_else(|| "HEAD".to_owned())
    }

    /// Why `action` cannot run, or `None` when it can.
    ///
    /// A tag can be created or deleted while another operation is in progress
    /// (it only writes a ref), but not while git is busy with one.
    pub(crate) fn blocked(&self, action: HistoryAction) -> Option<&'static str> {
        if self.busy {
            return Some("an operation is running");
        }
        match action {
            HistoryAction::CreateTag | HistoryAction::DeleteTag | HistoryAction::CreateBranch => {
                None
            }
            _ if self.pending => Some("finish the operation in progress first"),
            _ if self.branch.is_none() && self.head.is_some() => Some("HEAD is detached"),
            _ if self.branch.is_none() => Some("no branch to move"),
            _ => None,
        }
    }

    /// [`Self::blocked`] plus the commit-specific rule: picking or reverting
    /// HEAD itself has nothing to apply.
    pub(crate) fn blocked_for(&self, action: HistoryAction, commit: Oid) -> Option<&'static str> {
        if matches!(action, HistoryAction::CherryPick | HistoryAction::Revert)
            && self.head == Some(commit)
        {
            return Some("this is HEAD");
        }
        self.blocked(action)
    }
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

/// The context menu for one commit row.
///
/// Clipboard entries are always available; the history actions need a real
/// commit and a session, and say why they are disabled when they cannot run.
/// Popup items carry no tooltip, so the reason is folded into the label.
pub(crate) fn commit_menu(
    mut menu: PopupMenu,
    target: CommitTarget,
    state: Option<MenuState>,
    entity: Entity<RepoView>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let copy_id = target
        .id
        .map_or_else(|| target.short.clone(), |commit| commit.to_string());
    let copy_summary = target.summary.clone();
    menu = menu
        .item(PopupMenuItem::new("Copy commit id").on_click(
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()));
            },
        ))
        .item(PopupMenuItem::new("Copy summary").on_click(
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_summary.clone()));
            },
        ));

    let (Some(commit), Some(state)) = (target.id, state) else {
        return menu;
    };
    let short = target.short;
    let tags = target.tags;

    let onto = state.branch_label();
    let cherry_blocked = state.blocked_for(HistoryAction::CherryPick, commit);
    let revert_blocked = state.blocked_for(HistoryAction::Revert, commit);
    let reset_blocked = state.blocked(HistoryAction::Reset);
    let tag_blocked = state.blocked(HistoryAction::CreateTag);
    let delete_blocked = state.blocked(HistoryAction::DeleteTag);
    let branch_blocked = state.blocked(HistoryAction::CreateBranch);

    // A merge commit is picked or reverted against its first parent (`-m 1` in
    // `git_cli::history`), so these stay enabled for merges.
    let cherry = {
        let entity = entity.clone();
        let short = short.clone();
        PopupMenuItem::new(menu_label(
            format!("Cherry-pick onto {onto}"),
            cherry_blocked,
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
        PopupMenuItem::new(menu_label("Revert commit", revert_blocked))
            .disabled(revert_blocked.is_some())
            .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let short = short.clone();
                entity.update(cx, |view, cx| {
                    view.confirm_revert(commit, short, window, cx)
                });
            })
    };

    menu = menu.separator().item(cherry).item(revert).separator();

    let reset_label = format!("Reset {onto} to here");
    menu = match reset_blocked {
        // `PopupMenu` cannot mark the submenu entry itself disabled, so a
        // blocked reset is a plain disabled entry that names the reason.
        Some(reason) => {
            menu.item(PopupMenuItem::new(menu_label(reset_label, Some(reason))).disabled(true))
        }
        None => {
            let entity = entity.clone();
            let short = short.clone();
            menu.submenu(reset_label, window, cx, move |submenu, _, _| {
                let entry = |mode: ResetMode, label: &'static str| {
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
                    .item(entry(ResetMode::Soft, "Soft — keep the changes staged"))
                    .item(entry(ResetMode::Mixed, "Mixed — keep the changes unstaged"))
                    .item(entry(ResetMode::Hard, "Hard — discard the changes"))
            })
        }
    };

    let create_branch = {
        let entity = entity.clone();
        let point = commit.to_string();
        let from = format!("from commit {short}");
        PopupMenuItem::new("Create branch here…")
            .disabled(branch_blocked.is_some())
            .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let point = point.clone();
                let from = from.clone();
                entity.update(cx, |view, cx| {
                    view.open_create_dialog(Some(point), from, window, cx);
                });
            })
    };
    let create_tag = {
        let entity = entity.clone();
        let short = short.clone();
        PopupMenuItem::new(menu_label("Create tag here…", tag_blocked))
            .disabled(tag_blocked.is_some())
            .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let short = short.clone();
                entity.update(cx, |view, cx| {
                    view.open_tag_dialog(commit, short, window, cx)
                });
            })
    };

    menu = menu.separator().item(create_branch).item(create_tag);
    for tag in tags {
        let entity = entity.clone();
        let name = tag.clone();
        menu = menu.item(
            PopupMenuItem::new(menu_label(format!("Delete tag {tag}"), delete_blocked))
                .disabled(delete_blocked.is_some())
                .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let name = name.clone();
                    entity.update(cx, |view, cx| view.confirm_delete_tag(name, window, cx));
                }),
        );
    }
    menu
}

/// A menu label that says why it is disabled; a popup item has no tooltip, so
/// the reason has to travel in the text.
pub(crate) fn menu_label(base: impl Into<String>, blocked: Option<&'static str>) -> String {
    let base = base.into();
    match blocked {
        Some(reason) => format!("{base} ({reason})"),
        None => base,
    }
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
        ResetMode::Soft => format!(
            "{branch} moves back to {short}. The commits above it stay in the reflog, and their changes stay staged."
        ),
        ResetMode::Mixed => format!(
            "{branch} moves back to {short}. The commits above it stay in the reflog, and their changes stay in the working tree, unstaged."
        ),
        ResetMode::Hard => {
            let changes = staged + unstaged;
            let lost = match changes {
                0 => "The working tree has no uncommitted changes to lose.".to_owned(),
                1 => "1 uncommitted path is discarded as well.".to_owned(),
                count => format!("{count} uncommitted paths are discarded as well."),
            };
            format!(
                "{branch} moves back to {short}. The commits above it leave the branch (the reflog keeps them) and the working tree is reset. {lost}"
            )
        }
    }
}

/// git's `check-ref-format` rules for `refs/tags/<name>`, plus the two cases
/// only the app can know: a leading `-` (which git would read as an option) and
/// a name that is already taken.
pub(crate) fn validate_tag_name(name: &str, existing: &[String]) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Enter a tag name.");
    }
    if existing.iter().any(|tag| tag == name) {
        return Err("A tag with that name already exists.");
    }
    if name == "@" {
        return Err("'@' is shorthand for HEAD, not a tag name.");
    }
    if name.starts_with('-') {
        return Err("A tag name must not start with '-'.");
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("A tag name must not start or end with '/'.");
    }
    if name.contains("//") {
        return Err("A tag name must not contain '//'.");
    }
    if name.contains("..") {
        return Err("A tag name must not contain '..'.");
    }
    if name.contains("@{") {
        return Err("A tag name must not contain '@{'.");
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("A tag name must not contain spaces or control characters.");
    }
    if name.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err("A tag name must not contain any of ~ ^ : ? * [ \\.");
    }
    if name.ends_with('.') {
        return Err("A tag name must not end with '.'.");
    }
    if name.split('/').any(|part| {
        part.is_empty() || part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock")
    }) {
        return Err("Each part must not start with '.' or end with '.' or '.lock'.");
    }
    Ok(())
}

impl RepoView {
    /// What the commit list's menu may offer, or `None` without a session
    /// (`--fake` has nothing to act on).
    pub(super) fn history_menu_state(&self) -> Option<MenuState> {
        let snapshot = self.session.as_ref()?.snapshot();
        Some(MenuState::read(
            snapshot.head.as_ref(),
            snapshot.state,
            self.is_busy(),
        ))
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
        let label = format!("cherry-pick {short}");
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
            return;
        };
        if state.blocked_for(HistoryAction::Revert, commit).is_some() {
            return;
        }
        let branch = state.branch_label();
        let detail = format!(
            "A new commit on {branch} undoes {short}. If git stops on a conflict, resolve it from the conflict view and continue."
        );
        // Revert leaves its own commit behind and the reflog still has the
        // original, so OK is not painted as a destructive action.
        let mut confirm =
            ui::Confirm::new(format!("Revert commit {short}?"), detail).ok_label("Revert");
        confirm.danger = false;
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        ui::confirm(window, cx, confirm, move |window, cx| {
            let _ = entity.update(cx, |view, cx| {
                let label = format!("revert {short}");
                view.run_operation(Operation::Revert { commit }, label, window, cx);
            });
        });
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
            ResetMode::Soft => format!("Soft reset {branch} to {short}?"),
            ResetMode::Mixed => format!("Mixed reset {branch} to {short}?"),
            ResetMode::Hard => format!("Hard reset {branch} to {short}?"),
        };
        let confirm = match mode {
            // Only a hard reset deletes work: the other two keep the changes in
            // the index or the working tree, and the reflog keeps the commits.
            ResetMode::Hard => ui::Confirm::new(title, detail)
                .ok_label("Hard reset")
                .always_ask(),
            ResetMode::Soft | ResetMode::Mixed => {
                let mut confirm = ui::Confirm::new(title, detail).ok_label("Reset");
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
        };
        let label = format!("reset --{flag} to {}", crate::data::short_oid(target));
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
        window.open_dialog(cx, move |card, _, _| {
            let content = content.clone();
            let footer = footer.clone();
            let confirm = confirm.clone();
            card.title("Create tag")
                .w(px(440.0))
                .content(move |body, _, _| body.child(content.clone()))
                .footer(
                    DialogFooter::new()
                        .child(
                            ui::button("tag-cancel", "Cancel")
                                .size(ui::ButtonSize::Medium)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            ui::button("tag-create", "Create tag")
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
            format!("create annotated tag {name}")
        } else {
            format!("create tag {name}")
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
                format!("Delete tag {name}?"),
                "The tag is removed. The commit it points at stays in the repository.",
            )
            .ok_label("Delete tag"),
            move |window, cx| {
                let name = name.clone();
                let _ = entity.update(cx, |view, cx| {
                    let label = format!("delete tag {name}");
                    view.run_operation(Operation::DeleteTag { name }, label, window, cx);
                });
            },
        );
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
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("tag name"));
        let message = cx.new(|cx| InputState::new(window, cx).placeholder("Annotation (optional)"));
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
            self.error = Some(message.to_owned());
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
                    .child(format!(
                        "Tag commit {}. A message creates an annotated tag.",
                        self.short
                    )),
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
                Some("HEAD is detached"),
                "{action:?} needs a branch"
            );
        }
        for action in [
            HistoryAction::CreateTag,
            HistoryAction::DeleteTag,
            HistoryAction::CreateBranch,
        ] {
            assert_eq!(state.blocked(action), None, "{action:?} needs no branch");
        }
    }

    #[test]
    fn an_operation_in_progress_or_running_blocks_history_edits() {
        let head = branch("main", oid(2));
        let pending = MenuState::read(Some(&head), RepoState::Merging, false);
        assert_eq!(
            pending.blocked(HistoryAction::Reset),
            Some("finish the operation in progress first")
        );
        assert_eq!(
            pending.blocked(HistoryAction::CreateTag),
            None,
            "a tag can point into the past while a merge runs"
        );

        let busy = MenuState::read(Some(&head), RepoState::Normal, true);
        for action in [
            HistoryAction::CherryPick,
            HistoryAction::Revert,
            HistoryAction::Reset,
            HistoryAction::CreateTag,
            HistoryAction::DeleteTag,
            HistoryAction::CreateBranch,
        ] {
            assert_eq!(
                busy.blocked(action),
                Some("an operation is running"),
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
            Some("this is HEAD")
        );
        assert_eq!(
            state.blocked_for(HistoryAction::Revert, oid(2)),
            Some("this is HEAD")
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
            Some("no branch to move")
        );
    }

    #[test]
    fn a_disabled_entry_says_why() {
        assert_eq!(menu_label("Revert commit", None), "Revert commit");
        assert_eq!(
            menu_label("Revert commit", Some("this is HEAD")),
            "Revert commit (this is HEAD)"
        );
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
            Err("A tag with that name already exists."),
            "a tag name has to be free"
        );
    }
}
