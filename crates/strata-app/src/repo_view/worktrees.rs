//! Linked worktrees and submodules from the sidebar.
//!
//! The sections themselves are flattened in [`crate::branches`], next to the ref
//! sections they sit under. This module owns what a row's actions need and that
//! is too big for a context-menu closure: the "Add worktree" dialog and opening
//! a checkout as a repository of its own.
//!
//! Nothing here runs git. Every write is an [`Operation`] handed to
//! [`RepoView::run_operation`], like the branch sections' writes.

use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, Focusable as _, FontWeight, IntoElement, Render, Subscription,
    WeakEntity, Window, div, px,
};
use gpui_component::checkbox::Checkbox;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::notification::Notification;
use gpui_component::{ActiveTheme as _, Disableable as _, WindowExt as _};
use strata_core::session::Operation;
use strata_core::{HeadInfo, Oid, RefKind};

use crate::repo_view::RepoView;
use crate::settings::Settings;
use crate::style::Design;

impl RepoView {
    /// Whether any submodule is listed, so "Update all" has something to do.
    pub(crate) fn has_submodules(&self) -> bool {
        !self.extra_lists.submodules.is_empty()
    }

    /// Whether `git worktree prune` would drop a registration.
    pub(crate) fn has_prunable_worktree(&self) -> bool {
        self.extra_lists
            .worktrees
            .iter()
            .any(|worktree| worktree.prunable)
    }

    /// `git submodule update [--init] -- <path>` for one entry.
    pub(crate) fn update_submodule(
        &mut self,
        path: String,
        init: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = if init {
            t!("worktree.op.init_submodule", path = path)
        } else {
            t!("worktree.op.update_submodule", path = path)
        };
        self.run_operation(
            Operation::SubmoduleUpdate {
                paths: vec![path],
                init,
            },
            label,
            window,
            cx,
        );
    }

    /// Updates every submodule, cloning the ones that were never initialised.
    ///
    /// `--init` is what makes "update all" reach those at all; without it the
    /// command would silently skip exactly the entries that need it.
    pub(crate) fn update_all_submodules(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_operation(
            Operation::SubmoduleUpdate {
                paths: Vec::new(),
                init: true,
            },
            t!("worktree.op.update_all_submodules"),
            window,
            cx,
        );
    }

    pub(crate) fn prune_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_operation(
            Operation::WorktreePrune,
            t!("worktree.op.prune"),
            window,
            cx,
        );
    }

    /// Removes a linked worktree; `force` is the retry offered after git
    /// refused an unforced removal because the checkout has local changes.
    pub(crate) fn remove_worktree(
        &mut self,
        path: PathBuf,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = crate::branches::worktree_name(&path);
        let label = if force {
            t!("worktree.op.force_remove", name = name)
        } else {
            t!("worktree.op.remove", name = name)
        };
        self.run_operation(Operation::WorktreeRemove { path, force }, label, window, cx);
    }

    /// Asks before removing a worktree, then runs the removal.
    ///
    /// This is a dialog layer rather than a drawn card, so it holds focus and
    /// `Esc` cancels it instead of reaching the sidebar behind it.
    pub(crate) fn confirm_remove_worktree(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        // The context menu that offered the removal is closing, and it would
        // otherwise hand focus back to a handle that dies with it.
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        let name = crate::branches::worktree_name(&path);
        crate::ui::confirm(
            window,
            cx,
            crate::ui::Confirm::new(
                t!("branch.confirm.remove_worktree", path = name).to_string(),
                t!("worktree.confirm.remove_detail").to_string(),
            )
            .ok_label(t!("common.remove").to_string()),
            move |window, cx| {
                let path = path.clone();
                let _ = entity.update(cx, |view, cx| view.remove_worktree(path, false, window, cx));
            },
        );
    }

    /// Resolves a submodule (relative to the working tree) or worktree path.
    fn resolve_child_path(&self, path: &Path) -> PathBuf {
        let workdir = self
            .session
            .as_ref()
            .and_then(|session| session.repo().workdir().map(Path::to_path_buf));
        resolve_repo_relative(workdir.as_deref(), path)
    }

    /// Opens `path` in the current window, pushing this repository onto the
    /// workspace's navigation stack so the user can return to it.
    pub(crate) fn open_repository_in_window(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resolved = self.resolve_child_path(path);
        let Some(workspace) = self.workspace.clone() else {
            // Standalone RepoView (e.g. a test without a Workspace) has no
            // host to switch in place, so fall back to a new window.
            self.open_repository_in_new_window(&resolved, window, cx);
            return;
        };
        // Defer so this RepoView is no longer borrowed when Workspace swaps it
        // out and calls `detach_session` on it.
        window.defer(cx, move |window, cx| {
            let result = workspace.update(cx, |workspace, cx| {
                workspace.open_child(&resolved, window, cx)
            });
            if let Ok(Err(message)) = result {
                window.push_notification(
                    Notification::error(message).title(t!("chrome.could_not_open").to_string()),
                    cx,
                );
            }
        });
    }

    /// Opens `path` in a new tab of the current window (or switches to it if
    /// already open here). Falls back to a new window when standalone.
    pub(crate) fn open_repository_in_new_tab(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resolved = self.resolve_child_path(path);
        let Some(workspace) = self.workspace.clone() else {
            self.open_repository_in_new_window(&resolved, window, cx);
            return;
        };
        window.defer(cx, move |window, cx| {
            let result = workspace.update(cx, |workspace, cx| {
                workspace.open_path_in_new_tab(&resolved, window, cx)
            });
            if let Ok(Err(message)) = result {
                window.push_notification(
                    Notification::error(message).title(t!("chrome.could_not_open").to_string()),
                    cx,
                );
            }
        });
    }

    /// Opens `path` as a repository of its own, in a new window.
    ///
    /// A submodule's path comes from `.gitmodules` and is relative to the
    /// superproject, so a relative path is resolved against the worktree here.
    pub(crate) fn open_repository_in_new_window(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resolved = self.resolve_child_path(path);
        match crate::picker::resolve_repository(&resolved) {
            Ok(root) => open_repository_window(root, cx),
            Err(message) => window.push_notification(
                Notification::error(message).title(t!("chrome.could_not_open").to_string()),
                cx,
            ),
        }
    }

    /// Returns to the parent repository this view was opened from, if any.
    pub(crate) fn navigate_back_to_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            let result = workspace.update(cx, |workspace, cx| workspace.navigate_back(window, cx));
            if let Ok(Err(message)) = result {
                window.push_notification(
                    Notification::error(message).title(t!("chrome.could_not_open").to_string()),
                    cx,
                );
            }
        });
    }

    /// Opens whichever submodule or worktree row is currently selected in the
    /// sidebar, in the current window (bound to `Enter` in the sidebar list).
    pub(crate) fn activate_selected_extra(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.branches.selected_extra().cloned() else {
            return;
        };
        match selected {
            crate::branches::ExtraSelection::Submodule(path) => {
                let openable = self.extra_lists.submodules.iter().any(|submodule| {
                    submodule.path == path
                        && submodule.status != strata_core::repo::SubmoduleStatus::Uninitialised
                });
                if openable {
                    self.open_repository_in_window(Path::new(&path), window, cx);
                }
            }
            crate::branches::ExtraSelection::Worktree(path) => {
                let current_workdir = self.work_dir();
                let openable = self.extra_lists.worktrees.iter().any(|worktree| {
                    crate::branches::same_path(&worktree.path, &path)
                        && !worktree.prunable
                        && !current_workdir.as_deref().is_some_and(|current| {
                            crate::branches::same_path(current, &worktree.path)
                        })
                });
                if openable {
                    self.open_repository_in_window(&path, window, cx);
                }
            }
        }
    }

    /// The "Add worktree…" dialog: a directory, and either a branch that is not
    /// checked out anywhere or a new one.
    pub(crate) fn open_add_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() || self.is_busy() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        // A branch can only be checked out in one worktree; offering the ones
        // already on screen would only let git refuse the add.
        let checked_out: Vec<&str> = self
            .extra_lists
            .worktrees
            .iter()
            .filter_map(|worktree| match &worktree.head {
                Some(HeadInfo::Branch { short_name, .. }) => Some(short_name.as_str()),
                _ => None,
            })
            .collect();
        let candidates: Vec<String> = snapshot
            .refs
            .iter()
            .filter(|reference| reference.kind == RefKind::LocalBranch)
            .map(|reference| reference.short_name.clone())
            .filter(|name| !checked_out.iter().any(|checked| *checked == name))
            .collect();
        let branches: Vec<String> = crate::branches::natural_sorted(&candidates)
            .into_iter()
            .map(str::to_owned)
            .collect();

        // Where a new branch would start: the commit row the user is on, or
        // HEAD when the history selection is not a commit.
        let selected_commit = self
            .selected
            .and_then(|index| self.source().and_then(|source| source.commit_id(index)));
        let head_commit = snapshot.head.as_ref().and_then(|head| match head {
            HeadInfo::Branch { target, .. } => *target,
            HeadInfo::Detached { target } => Some(*target),
        });
        let start = selected_commit.or(head_commit);
        let start_label = match (selected_commit, snapshot.head.as_ref()) {
            (Some(commit), _) => t!(
                "history_ops.menu.from_commit",
                short = crate::data::short_oid(commit)
            )
            .to_string(),
            (None, Some(HeadInfo::Branch { short_name, .. })) => {
                t!("branch.op.from", branch = short_name).to_string()
            }
            (None, Some(HeadInfo::Detached { .. })) => t!("history.from_current").to_string(),
            (None, None) => t!("history.from_head").to_string(),
        };
        let root = self.add_worktree_root();

        let view = cx.weak_entity();
        let dialog = cx
            .new(|cx| AddWorktreeDialog::new(view, root, branches, start, start_label, window, cx));
        let focus = dialog.read(cx).input_focus(cx);
        let content = dialog.clone();
        let footer = dialog.clone();
        let confirm = dialog;
        window.open_dialog(cx, move |card, _, _| {
            let content = content.clone();
            let footer = footer.clone();
            let confirm = confirm.clone();
            card.title(t!("worktree.add.title").to_string())
                .w(px(520.0))
                .content(move |body, _, _| body.child(content.clone()))
                .footer(
                    DialogFooter::new()
                        .child(
                            crate::ui::button(
                                "worktree-add-cancel",
                                t!("common.cancel").to_string(),
                            )
                            .size(crate::ui::ButtonSize::Medium)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            crate::ui::button(
                                "worktree-add-submit",
                                t!("worktree.add.title").to_string(),
                            )
                            .size(crate::ui::ButtonSize::Medium)
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

    /// The directory a new worktree's default path is a sibling of: the main
    /// worktree, because a linked one is a checkout of the same repository.
    fn add_worktree_root(&self) -> PathBuf {
        if let Some(main) = self
            .extra_lists
            .worktrees
            .iter()
            .find(|worktree| worktree.is_main)
        {
            return main.path.clone();
        }
        self.session
            .as_ref()
            .and_then(|session| session.repo().workdir().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// The default checkout directory for `branch`: a sibling of the repository
/// named after it.
///
/// A branch's own separators are flattened, so `feat/login` gives
/// `<repo>-feat-login` instead of nesting the checkout inside a directory that
/// looks like another repository.
pub(crate) fn default_worktree_path(root: &Path, branch: &str) -> PathBuf {
    let repo = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "worktree".to_owned());
    let branch = branch.trim().replace(['/', '\\'], "-");
    let branch = branch.trim_matches('-');
    let suffix = if branch.is_empty() {
        "worktree"
    } else {
        branch
    };
    root.parent()
        .unwrap_or(root)
        .join(format!("{repo}-{suffix}"))
}

/// The "Add worktree" form.
///
/// It is its own entity so the form state lives as long as the dialog, without
/// `RepoView` needing a field for a dialog that is open only briefly.
pub(crate) struct AddWorktreeDialog {
    /// Weak: the dialog can outlive a `RepoView` the workspace has replaced.
    view: WeakEntity<RepoView>,
    /// The main worktree the default path is a sibling of.
    root: PathBuf,
    path: Entity<InputState>,
    new_branch: Entity<InputState>,
    /// A new branch at `start` instead of an existing one.
    create_branch: bool,
    /// Existing branches no worktree has checked out, in the sidebar's order.
    branches: Vec<String>,
    selected: usize,
    /// Commit a new branch starts from; `None` when the repository has no
    /// commit yet.
    start: Option<Oid>,
    start_label: String,
    error: Option<String>,
    /// Once the user edits the path, choosing another branch leaves it alone.
    path_edited: bool,
    _path_events: Subscription,
    _branch_events: Subscription,
}

impl AddWorktreeDialog {
    fn new(
        view: WeakEntity<RepoView>,
        root: PathBuf,
        branches: Vec<String>,
        start: Option<Oid>,
        start_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // With no branch that could be checked out again, the dialog has to
        // create one; that is also the useful default for a first worktree.
        let create_branch = branches.is_empty();
        let default_branch = branches.first().map(String::as_str).unwrap_or("worktree");
        let default_path = default_worktree_path(&root, default_branch)
            .display()
            .to_string();

        let path = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .placeholder(t!("worktree.add.path_placeholder").to_string());
            state.set_value(default_path, window, cx);
            state
        });
        let new_branch = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("worktree.add.branch_placeholder").to_string())
        });
        let _path_events = cx.subscribe(&path, |this, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.path_edited = true;
                this.error = None;
                cx.notify();
            }
        });
        let _branch_events = cx.subscribe(&new_branch, |this, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.error = None;
                // The default path follows the branch the user is naming, up to
                // the moment they edit the path themselves. An input event
                // carries no window, so the active one rewrites the path.
                if let Some(handle) = cx.active_window() {
                    let _ = handle.update(cx, |_, window, cx| this.sync_default_path(window, cx));
                }
                cx.notify();
            }
        });

        Self {
            view,
            root,
            path,
            new_branch,
            create_branch,
            branches,
            selected: 0,
            start,
            start_label,
            error: None,
            path_edited: false,
            _path_events,
            _branch_events,
        }
    }

    fn input_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.path.focus_handle(cx)
    }

    /// The branch the path would be named after: the typed one while creating
    /// a branch, the selected existing one otherwise.
    fn branch_name(&self, cx: &App) -> Option<String> {
        if self.create_branch {
            let name = self.new_branch.read(cx).value().trim().to_owned();
            return (!name.is_empty()).then_some(name);
        }
        self.branches.get(self.selected).cloned()
    }

    /// Rewrites the path input's default after the branch choice changes.
    fn sync_default_path(&mut self, window: &mut Window, cx: &mut App) {
        if self.path_edited {
            return;
        }
        let branch = self
            .branch_name(cx)
            .unwrap_or_else(|| "worktree".to_owned());
        let default = default_worktree_path(&self.root, &branch)
            .display()
            .to_string();
        if self.path.read(cx).value().as_ref() == default {
            return;
        }
        self.path
            .update(cx, |state, cx| state.set_value(default, window, cx));
    }

    fn select_branch(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = index;
        self.error = None;
        self.sync_default_path(window, cx);
        cx.notify();
    }

    /// Validates the form and starts the operation.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.path.read(cx).value().trim().to_owned();
        if text.is_empty() {
            self.fail(&t!("worktree.add.path_required"), cx);
            return;
        }
        // git resolves a relative path against the repository, so the dialog
        // does the same instead of against whatever directory the app runs in.
        let path = PathBuf::from(&text);
        let path = if path.is_absolute() {
            path
        } else {
            self.root.join(path)
        };
        if std::fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_some()) {
            self.fail(
                &t!("worktree.add.path_not_empty", path = path.display()),
                cx,
            );
            return;
        }

        let (branch, new_branch, commit) = if self.create_branch {
            let name = self.new_branch.read(cx).value().trim().to_owned();
            if let Err(message) = crate::branches::validate_branch_name(&name) {
                self.fail(&message, cx);
                return;
            }
            let Some(commit) = self.start else {
                self.fail(&t!("worktree.add.no_start_commit"), cx);
                return;
            };
            (None, Some(name), Some(commit))
        } else {
            let Some(branch) = self.branches.get(self.selected).cloned() else {
                self.fail(&t!("worktree.add.choose_branch"), cx);
                return;
            };
            (Some(branch), None, None)
        };

        let label = t!("worktree.op.add", path = path.display());
        let operation = Operation::WorktreeAdd {
            path,
            branch,
            new_branch,
            commit,
        };
        let _ = self.view.update(cx, |view, cx| {
            view.run_operation(operation, label, window, cx)
        });
        // Closing here would drop this dialog while it is being updated.
        window.defer(cx, |window, cx| window.close_dialog(cx));
    }

    fn fail(&mut self, message: &str, cx: &mut Context<Self>) {
        self.error = Some(message.to_owned());
        cx.notify();
    }
}

impl Render for AddWorktreeDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let muted = design.muted;
        let create_branch = self.create_branch;
        let start_label = self.start_label.clone();

        let toggle = cx.listener(|this, checked: &bool, window, cx| {
            this.create_branch = *checked;
            this.error = None;
            this.sync_default_path(window, cx);
            cx.notify();
        });
        // With every branch already checked out, the only way to add a worktree
        // is a new branch; leaving the box switchable would offer a dead end.
        let branches_available = !self.branches.is_empty();

        let mut form = div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(muted)
                    .child(t!("worktree.add.description").to_string()),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(t!("worktree.add.directory").to_string()),
            )
            .child(Input::new(&self.path))
            .child(
                Checkbox::new("worktree-create-branch")
                    .checked(create_branch)
                    .disabled(!branches_available)
                    .label(t!("worktree.add.create_branch").to_string())
                    .on_change(toggle),
            );

        if create_branch {
            form = form.child(Input::new(&self.new_branch)).child(
                div()
                    .text_size(px(11.0))
                    .text_color(muted)
                    .child(t!("worktree.add.start_description", start = start_label).to_string()),
            );
        } else if self.branches.is_empty() {
            form = form.child(
                div()
                    .text_size(px(11.0))
                    .text_color(muted)
                    .child(t!("worktree.add.all_checked_out").to_string()),
            );
        } else {
            let rows = self
                .branches
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    let on_click = cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                        this.select_branch(index, window, cx)
                    });
                    crate::ui::list_row(
                        ("worktree-branch", index),
                        crate::ui::RowState::NORMAL.selected(index == self.selected),
                        &design,
                    )
                    .cursor_pointer()
                    .on_click(on_click)
                    .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                })
                .collect::<Vec<_>>();
            form = form.child(
                div()
                    .id("worktree-branch-list")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(200.0))
                    .overflow_y_scroll()
                    .children(rows),
            );
        }

        if let Some(error) = &self.error {
            form = form.child(
                div()
                    .text_size(px(11.0))
                    .text_color(design.remove_text)
                    .child(error.clone()),
            );
        }
        form
    }
}

/// Opens `root` as a repository in a new window, the way the settings window
/// opens beside the one it was invoked from.
pub(crate) fn open_repository_window(root: PathBuf, cx: &mut App) {
    crate::workspace::open_new_window(root, cx);
}

/// Resolves a path reported by git (`submodule.path` is relative to the
/// superproject's workdir, whereas `worktree.path` is already absolute).
pub(crate) fn resolve_repo_relative(workdir: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_relative() {
        workdir
            .map(|dir| dir.join(path))
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_worktree_path_sits_beside_the_repository() {
        assert_eq!(
            default_worktree_path(Path::new("/src/strata"), "main"),
            PathBuf::from("/src/strata-main")
        );
    }

    #[test]
    fn a_branch_s_own_separators_do_not_nest_the_checkout() {
        assert_eq!(
            default_worktree_path(Path::new("/src/strata"), "feat/sidebar"),
            PathBuf::from("/src/strata-feat-sidebar")
        );
        assert_eq!(
            default_worktree_path(Path::new("/src/strata"), "feat/windows\\paths"),
            PathBuf::from("/src/strata-feat-windows-paths")
        );
    }

    #[test]
    fn an_empty_branch_name_still_yields_a_sibling() {
        assert_eq!(
            default_worktree_path(Path::new("/src/strata"), "  "),
            PathBuf::from("/src/strata-worktree")
        );
    }

    #[test]
    fn relative_submodule_paths_resolve_against_the_superproject() {
        assert_eq!(
            resolve_repo_relative(Some(Path::new("/src/strata")), Path::new("libs/ui")),
            PathBuf::from("/src/strata/libs/ui")
        );
        assert_eq!(
            resolve_repo_relative(
                Some(Path::new("/src/strata")),
                Path::new("/src/strata-feat")
            ),
            PathBuf::from("/src/strata-feat")
        );
        assert_eq!(
            resolve_repo_relative(None, Path::new("libs/ui")),
            PathBuf::from("libs/ui")
        );
    }
}
