//! Stashing from the UI: the stash dialog and the sidebar's stash entries.
//!
//! Nothing here runs git. The dialog and the sidebar's context menu build an
//! [`Operation::Stash…`] and hand it to [`RepoView::run_operation`], which gives
//! it to the session worker; the sidebar is redrawn from the snapshot the
//! session publishes afterwards.
//!
//! A pop or apply that conflicts leaves the working tree in a conflicted state,
//! which the staging view already lists; nothing extra is needed here.
//!
//! The row itself is built here too: a stash's age and source branch live in
//! the snapshot, and neither is in the string the sidebar sections receive, so
//! the tooltip and context menu read them through the view when they are built.

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, Focusable as _, IntoElement, ParentElement,
    Render, SharedString, Styled, Subscription, WeakEntity, Window, div, px,
};
use gpui_component::Disableable as _;
use gpui_component::checkbox::Checkbox;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::PopupMenuItem;
use gpui_component::notification::Notification;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, WindowExt as _};
use strata_core::session::Operation;
use strata_core::{ChangeKind, StashInfo};

use crate::repo_view::RepoView;
use crate::style;
use crate::ui::RowTipExt as _;

/// Title of the stash dialog and of the notifications it raises.
fn stash_title() -> String {
    t!("action.stash_changes").to_string()
}

/// Tallest the file checklist grows before it scrolls; a dialog that lists a
/// whole worktree must not grow past the window.
const CHECKLIST_HEIGHT: f32 = 220.0;

impl RepoView {
    /// `StashChanges`: asks for a message and stashes the working tree, or only
    /// the files still ticked in the dialog's checklist.
    pub(crate) fn stash_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            notify(
                window,
                cx,
                Notification::warning(t!("stash.needs_repo").to_string()),
            );
            return;
        }
        if self.branches.is_running()
            || self
                .session
                .as_ref()
                .is_some_and(|session| session.snapshot().busy.is_some())
        {
            notify(
                window,
                cx,
                Notification::warning(t!("stash.busy").to_string()),
            );
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        let status = &snapshot.status;
        if status.staged.is_empty() && status.unstaged.is_empty() {
            notify(
                window,
                cx,
                Notification::info(t!("stash.nothing").to_string()),
            );
            return;
        }
        // `git stash push` saves nothing when the only changes are untracked
        // files and `-u` is missing, so start with the box ticked in exactly
        // that case.
        let include_default = status.staged.is_empty()
            && status
                .unstaged
                .iter()
                .all(|change| change.kind == ChangeKind::Untracked);
        let files = checklist(status);

        let view = cx.weak_entity();
        let dialog = cx.new(|cx| StashDialog::new(view, include_default, files, window, cx));
        let focus = dialog.read(cx).input_focus(cx);
        let content = dialog.clone();
        let footer = dialog.clone();
        let confirm = dialog;
        window.open_dialog(cx, move |card, _, _| {
            let content = content.clone();
            let footer = footer.clone();
            let confirm = confirm.clone();
            card.title(stash_title())
                .w(px(520.0))
                .content(move |body, _, _| body.child(content.clone()))
                .footer(
                    DialogFooter::new()
                        .child(
                            crate::ui::button("stash-cancel", t!("common.cancel").to_string())
                                .size(crate::ui::ButtonSize::Medium)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            crate::ui::button("stash-submit", stash_title())
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
        // The input owns `Enter` while it has focus, which is where the user
        // starts typing; `Escape` and the footer still belong to the dialog.
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    /// Applies `stash@{index}` and keeps the stash.
    pub(crate) fn stash_apply(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("stash.apply_op", index = index).to_string();
        self.run_operation(Operation::StashApply { index }, label, window, cx);
    }

    /// Applies `stash@{index}` and drops it when the apply succeeded.
    pub(crate) fn stash_pop(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let label = t!("stash.pop_op", index = index).to_string();
        self.run_operation(Operation::StashPop { index }, label, window, cx);
    }

    /// Asks before deleting a stash entry; dropping is the one stash action git
    /// cannot undo.
    pub(crate) fn confirm_stash_drop(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The menu that offered this is already closing, and it would otherwise
        // leave the dialog with a focus handle that dies with it; the dialog
        // hands focus back to whatever was focused when it opened.
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        crate::ui::confirm(
            window,
            cx,
            crate::ui::Confirm::new(
                t!("stash.drop_title", index = index).to_string(),
                "The stashed changes are deleted. Unlike a pop, the changes are not \
                 applied to the working tree first.",
            )
            .ok_label(t!("stash.drop_button").to_string()),
            move |window, cx| {
                let _ = entity.update(cx, |view, cx| view.stash_drop(index, window, cx));
            },
        );
    }

    fn stash_drop(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let label = t!("stash.drop_op", index = index).to_string();
        self.run_operation(Operation::StashDrop { index }, label, window, cx);
    }

    /// `"on main · 2 hours ago"` for `stash@{index}`, or `None` when the
    /// snapshot has no such entry any more.
    pub(crate) fn stash_detail(&self, index: usize) -> Option<String> {
        let now = style::now_epoch();
        self.session.as_ref().and_then(|session| {
            session
                .snapshot()
                .stashes
                .iter()
                .find(|stash| stash.index == index)
                .map(|stash| stash_detail(stash, now))
        })
    }
}

/// One row of the stash dialog's checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StashFile {
    path: String,
    kind: ChangeKind,
    checked: bool,
}

/// Every changed path once, staged before unstaged, all ticked.
fn checklist(status: &strata_core::WorktreeStatus) -> Vec<StashFile> {
    let mut files: Vec<StashFile> = Vec::new();
    for change in status.staged.iter().chain(status.unstaged.iter()) {
        if files.iter().any(|file| file.path == change.path) {
            continue;
        }
        files.push(StashFile {
            path: change.path.clone(),
            kind: change.kind,
            checked: true,
        });
    }
    files
}

/// The operation the checklist describes.
///
/// `None` when nothing eligible is ticked: git reads an empty pathspec as
/// "everything", so the dialog must refuse instead of guessing. When every
/// eligible change is ticked the plain operation is used, so the stash behaves
/// exactly like the one without a checklist; otherwise only the ticked paths
/// are named. Untracked paths never take part without `include_untracked`,
/// because `git stash push -- <untracked>` is an error without `-u`.
fn stash_operation(
    message: Option<String>,
    include_untracked: bool,
    files: &[StashFile],
) -> Option<Operation> {
    let eligible = |file: &StashFile| include_untracked || file.kind != ChangeKind::Untracked;
    let selected: Vec<String> = files
        .iter()
        .filter(|file| eligible(file))
        .filter(|file| file.checked)
        .map(|file| file.path.clone())
        .collect();
    if selected.is_empty() {
        return None;
    }
    let total = files.iter().filter(|file| eligible(file)).count();
    if selected.len() == total {
        return Some(Operation::StashPush {
            message,
            include_untracked,
        });
    }
    Some(Operation::StashPushPaths {
        message,
        include_untracked,
        paths: selected,
    })
}

/// How the status bar names the stash the dialog started.
fn operation_label(operation: &Operation) -> String {
    match operation {
        Operation::StashPush { message, .. } => message.as_ref().map_or_else(
            || t!("stash.op.changes").to_string(),
            |message| t!("stash.op.message", message = message).to_string(),
        ),
        Operation::StashPushPaths { message, paths, .. } => {
            let count = if paths.len() == 1 {
                t!("stash.op.files.one", count = paths.len()).to_string()
            } else {
                t!("stash.op.files.other", count = paths.len()).to_string()
            };
            match message {
                Some(message) => {
                    t!("stash.op.count_as", count = count, message = message).to_string()
                }
                None => count,
            }
        }
        _ => t!("stash.op.changes").to_string(),
    }
}

/// `"on main · 2 hours ago"`, or just the age when the stash was made on a
/// detached HEAD.
fn stash_detail(stash: &StashInfo, now: i64) -> String {
    let age = style::relative_time(stash.created_time, now);
    match &stash.branch {
        Some(branch) => t!("stash.detail", branch = branch, age = age).to_string(),
        None => age,
    }
}

/// The sidebar's stash row: the message, with the entry's age and branch in a
/// tooltip and at the top of its context menu.
pub(crate) fn sidebar_row(
    name: &str,
    index: usize,
    revealed: bool,
    enabled: bool,
    busy: bool,
    colors: &crate::branches::Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let label = stash_message(name).to_owned();
    let tooltip_entity = entity.clone();
    let row = crate::ui::list_row(
        ("stash", index),
        crate::ui::RowState::NORMAL.revealed(revealed),
        &colors.design,
    )
    .h(px(28.0))
    .w_full()
    .min_w_0()
    .overflow_hidden()
    .mx(px(0.0))
    .pr(px(18.0))
    .rounded(px(7.0))
    .text_size(px(colors.design.small()))
    .text_color(colors.sidebar_foreground)
    .child(div().flex_1().min_w_0().truncate().child(label))
    .row_tooltip(move |_window, cx| {
        let text = tooltip_entity
            .read(cx)
            .stash_detail(index)
            .unwrap_or_default();
        cx.new(|_| Tooltip::new(text)).into()
    });
    let click_entity = entity.clone();
    let row = row.cursor_pointer().on_click(move |_, window, cx| {
        click_entity.update(cx, |view, cx| view.reveal_stash(index, window, cx));
    });
    if !enabled {
        return row.into_any_element();
    }

    crate::ui::context_menu(
        ("stash-menu", index),
        &colors.design,
        row,
        move |menu, _window, cx| {
            let detail = entity.read(cx).stash_detail(index);
            let menu = match detail {
                Some(detail) => menu
                    .item(PopupMenuItem::label(format!(
                        "stash@{{{index}}} · {detail}"
                    )))
                    .separator(),
                None => menu,
            };
            let reveal = {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| view.reveal_stash(index, window, cx));
                }
            };
            let apply = {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| view.stash_apply(index, window, cx));
                }
            };
            let pop = {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| view.stash_pop(index, window, cx));
                }
            };
            let drop = {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| view.confirm_stash_drop(index, window, cx));
                }
            };
            let blocked = busy.then_some(t!("branch.reason.busy").to_string());
            menu.item(PopupMenuItem::new(t!("stash.reveal").to_string()).on_click(reveal))
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("stash.apply").to_string(),
                        blocked.as_ref(),
                    ))
                    .disabled(blocked.is_some())
                    .on_click(apply),
                )
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("stash.pop_button").to_string(),
                        blocked.as_ref(),
                    ))
                    .disabled(blocked.is_some())
                    .on_click(pop),
                )
                .separator()
                .item(
                    PopupMenuItem::new(crate::ui::menu_label(
                        t!("stash.drop").to_string(),
                        blocked.as_ref(),
                    ))
                    .disabled(blocked.is_some())
                    .on_click(drop),
                )
        },
    )
    .into_any_element()
}

/// The sidebar lists `stash@{0}: message`; the row shows the message alone.
fn stash_message(name: &str) -> &str {
    match name
        .strip_prefix("stash@{")
        .and_then(|rest| rest.split_once("}: "))
    {
        Some((_, message)) => message,
        None => name,
    }
}

/// The stash dialog's form: a message, the file checklist and the
/// untracked-files checkbox.
///
/// It is its own entity so the form state lives as long as the dialog, without
/// `RepoView` needing a field for a dialog that is open only briefly.
pub struct StashDialog {
    /// Weak: the dialog outlives a `RepoView` the workspace has replaced.
    view: WeakEntity<RepoView>,
    input: Entity<InputState>,
    include_untracked: bool,
    files: Vec<StashFile>,
    _input_events: Subscription,
}

impl StashDialog {
    fn new(
        view: WeakEntity<RepoView>,
        include_untracked: bool,
        files: Vec<StashFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("stash.message_placeholder").to_string())
        });
        let _input_events = cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    ..
                }
            ) {
                let Some(handle) = cx.active_window() else {
                    return;
                };
                let _ = handle.update(cx, |_, window, cx| this.submit(window, cx));
            }
        });
        Self {
            view,
            input,
            include_untracked,
            files,
            _input_events,
        }
    }

    fn input_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.input.focus_handle(cx)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut App) {
        let message = self.input.read(cx).value().trim().to_owned();
        let message = (!message.is_empty()).then_some(message);
        let Some(operation) = stash_operation(message, self.include_untracked, &self.files) else {
            notify(
                window,
                cx,
                Notification::warning(t!("stash.tick_one").to_string()),
            );
            return;
        };
        let label = operation_label(&operation);
        let _ = self.view.update(cx, |view, cx| {
            view.run_operation(operation, label, window, cx);
        });
        // Closing here would drop this dialog while it is being updated.
        window.defer(cx, |window, cx| window.close_dialog(cx));
    }
}

impl Render for StashDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let include_untracked = self.include_untracked;
        let toggle = cx.listener(|this, checked: &bool, _window, cx| {
            this.include_untracked = *checked;
            cx.notify();
        });
        let ticked = self
            .files
            .iter()
            .filter(|file| include_untracked || file.kind != ChangeKind::Untracked)
            .filter(|file| file.checked)
            .count();
        let eligible = self
            .files
            .iter()
            .filter(|file| include_untracked || file.kind != ChangeKind::Untracked)
            .count();
        let summary = if ticked == eligible {
            t!("stash.all_files", count = eligible).to_string()
        } else {
            t!("stash.some_files", ticked = ticked, eligible = eligible).to_string()
        };
        let rows = self
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let untracked = file.kind == ChangeKind::Untracked;
                let on_change = cx.listener(move |this, checked: &bool, _window, cx| {
                    if let Some(file) = this.files.get_mut(index) {
                        file.checked = *checked;
                    }
                    cx.notify();
                });
                Checkbox::new(("stash-file", index))
                    // Untracked files are only part of the stash while the
                    // box below is ticked, so the row is shown unticked until
                    // then instead of claiming a file the stash will not take.
                    .checked(file.checked && (include_untracked || !untracked))
                    .disabled(untracked && !include_untracked)
                    .label(SharedString::from(file.path.clone()))
                    .on_change(on_change)
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(div().text_size(px(11.0)).text_color(muted).child(format!(
                "{summary} The staged and unstaged parts of each ticked file are \
                     stashed, and the rest of the working tree stays as it is."
            )))
            .child(Input::new(&self.input))
            .child(
                div()
                    .id("stash-file-list")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(CHECKLIST_HEIGHT))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                Checkbox::new("stash-include-untracked")
                    .checked(self.include_untracked)
                    .label(t!("stash.include_untracked").to_string())
                    .on_change(toggle),
            )
    }
}

fn notify(window: &mut Window, cx: &mut App, note: Notification) {
    window.push_notification(note.title(stash_title()), cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, kind: ChangeKind, checked: bool) -> StashFile {
        StashFile {
            path: path.to_owned(),
            kind,
            checked,
        }
    }

    /// The paths an operation names, or `None` for a plain stash-everything.
    fn paths_of(operation: &Operation) -> Option<Vec<String>> {
        match operation {
            Operation::StashPush { .. } => None,
            Operation::StashPushPaths { paths, .. } => Some(paths.clone()),
            _ => panic!("not a stash operation"),
        }
    }

    fn include_of(operation: &Operation) -> bool {
        match operation {
            Operation::StashPush {
                include_untracked, ..
            }
            | Operation::StashPushPaths {
                include_untracked, ..
            } => *include_untracked,
            _ => panic!("not a stash operation"),
        }
    }

    #[test]
    fn all_ticked_stashes_everything_without_a_pathspec() {
        let files = [
            file("src/a.rs", ChangeKind::Modified, true),
            file("src/b.rs", ChangeKind::Added, true),
        ];
        let operation = stash_operation(Some("wip".into()), true, &files).expect("a stash");
        assert_eq!(paths_of(&operation), None);
        assert!(include_of(&operation));
    }

    #[test]
    fn unticking_a_file_names_only_the_ticked_paths() {
        let files = [
            file("src/a.rs", ChangeKind::Modified, true),
            file("src/b.rs", ChangeKind::Modified, false),
            file("src/c.rs", ChangeKind::Deleted, true),
        ];
        let operation = stash_operation(None, false, &files).expect("a stash");
        assert_eq!(
            paths_of(&operation),
            Some(vec!["src/a.rs".to_owned(), "src/c.rs".to_owned()])
        );
        assert!(!include_of(&operation));
    }

    #[test]
    fn untracked_files_need_the_box_and_otherwise_do_not_change_the_pathspec() {
        let files = [
            file("src/a.rs", ChangeKind::Modified, true),
            file("notes.txt", ChangeKind::Untracked, true),
        ];
        // Without `-u` an untracked pathspec is an error, so the box decides
        // whether they are part of the stash at all.
        let without = stash_operation(None, false, &files).expect("a stash");
        assert_eq!(paths_of(&without), None, "the only tracked file is ticked");
        let with = stash_operation(None, true, &files).expect("a stash");
        assert_eq!(paths_of(&with), None);
        assert!(include_of(&with));

        let partial = [
            file("src/a.rs", ChangeKind::Modified, false),
            file("notes.txt", ChangeKind::Untracked, true),
        ];
        assert!(
            stash_operation(None, false, &partial).is_none(),
            "an untracked-only selection cannot be stashed without -u"
        );
        assert_eq!(
            paths_of(&stash_operation(None, true, &partial).expect("a stash")),
            Some(vec!["notes.txt".to_owned()])
        );
    }

    #[test]
    fn nothing_ticked_is_refused_instead_of_stashing_everything() {
        let files = [file("src/a.rs", ChangeKind::Modified, false)];
        assert!(
            stash_operation(None, false, &files).is_none(),
            "git would read an empty pathspec as everything"
        );
    }

    #[test]
    fn the_status_bar_names_a_path_limited_stash_by_its_count() {
        let operation = Operation::StashPushPaths {
            message: None,
            include_untracked: false,
            paths: vec!["a".into(), "b".into()],
        };
        assert_eq!(operation_label(&operation), "stash 2 files");
        let single = Operation::StashPushPaths {
            message: Some("wip".into()),
            include_untracked: false,
            paths: vec!["a".into()],
        };
        assert_eq!(operation_label(&single), "stash 1 file as \"wip\"");
        let plain = Operation::StashPush {
            message: None,
            include_untracked: true,
        };
        assert_eq!(operation_label(&plain), "stash changes");
    }

    #[test]
    fn a_stash_detail_names_its_branch_and_age() {
        let stash = StashInfo {
            index: 0,
            id: "00".repeat(20).parse().expect("an object id"),
            message: "WIP on main: 1234567 wip".into(),
            created_time: 1_000,
            branch: Some("main".into()),
        };
        assert_eq!(
            stash_detail(&stash, 1_000 + 2 * 3_600),
            "on main · 2 hours ago"
        );
        let detached = StashInfo {
            branch: None,
            ..stash
        };
        assert_eq!(stash_detail(&detached, 1_000 + 120), "2 min ago");
    }

    #[test]
    fn a_stash_row_shows_the_message_without_the_index() {
        assert_eq!(
            stash_message("stash@{0}: WIP on diff options"),
            "WIP on diff options"
        );
        assert_eq!(stash_message("main"), "main");
    }

    #[test]
    fn the_checklist_lists_each_changed_path_once() {
        use strata_core::{FileChange, WorktreeStatus};
        let status = WorktreeStatus {
            staged: vec![FileChange {
                path: "src/a.rs".into(),
                old_path: None,
                kind: ChangeKind::Modified,
            }],
            unstaged: vec![
                FileChange {
                    path: "src/a.rs".into(),
                    old_path: None,
                    kind: ChangeKind::Modified,
                },
                FileChange {
                    path: "notes.txt".into(),
                    old_path: None,
                    kind: ChangeKind::Untracked,
                },
            ],
        };
        let files = checklist(&status);
        assert_eq!(files.len(), 2, "a path in both halves is listed once");
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[1].kind, ChangeKind::Untracked);
        assert!(
            files.iter().all(|file| file.checked),
            "all ticked by default"
        );
    }
}
