//! The branch forms: what each one asks for, how it validates an answer, and
//! the card it draws while it is open.

use std::path::Path;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Div, Entity, FocusHandle,
    Focusable as _, FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels, Size,
    StatefulInteractiveElement as _, Styled, Window, anchored, deferred, div, hsla, point, px,
};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};

use lamina_core::session::RemoteBranch;

use crate::repo_view::RepoView;

use super::refs::UpstreamChoice;
use super::validate::{validate_branch_name, validate_remote_name, validate_remote_url};
use super::{BranchStart, Branches, CancelBranchDialog, Colors, ConfirmBranchDialog, Retry};

/// Key context the dialog sets while it is open, so `Enter` and `Escape` mean
/// confirm and cancel only there.
const BRANCH_DIALOG: &str = "BranchDialog";

pub(super) struct Dialog {
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
                .text_size(colors.design.px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.create").to_string()),
        )
        .child(
            div()
                .text_size(colors.design.px(11.0))
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
                .text_size(colors.design.px(11.0))
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
        .w(colors.design.px(420.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .bg(colors.overlay)
        .border_1()
        .border_color(colors.border)
        .text_size(colors.design.px(12.0))
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
                .text_size(colors.design.px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.rename").to_string()),
        )
        .child(
            div()
                .text_size(colors.design.px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!("branch.dialog.renaming", branch = form.branch)),
        )
        .child(Input::new(&form.input).w_full());
    match &form.upstream_label {
        Some(label) => {
            card = card.child(
                div()
                    .text_size(colors.design.px(11.0))
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
                            .text_size(colors.design.px(10.0))
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
                    .text_size(colors.design.px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(t!("branch.dialog.no_upstream").to_string()),
            );
        }
    }
    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(colors.design.px(11.0))
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
                .text_size(colors.design.px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.set_upstream").to_string()),
        )
        .child(
            div()
                .text_size(colors.design.px(11.0))
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
                .text_size(colors.design.px(11.0))
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
                .max_h(colors.design.px(200.0))
                .overflow_y_scroll()
                .children(rows),
        );
    }

    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(colors.design.px(11.0))
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
                .text_size(colors.design.px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.add_remote").to_string()),
        )
        .child(
            div()
                .text_size(colors.design.px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!("branch.dialog.add_remote_detail").to_string()),
        )
        .child(
            div()
                .text_size(colors.design.px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .child(t!("branch.dialog.name").to_string()),
        )
        .child(Input::new(&form.name).w_full())
        .child(
            div()
                .text_size(colors.design.px(11.0))
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
                .text_size(colors.design.px(11.0))
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
                .text_size(colors.design.px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.rename_remote").to_string()),
        )
        .child(
            div()
                .text_size(colors.design.px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!("branch.dialog.renaming_remote", name = form.name).to_string()),
        )
        .child(Input::new(&form.input).w_full());
    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(colors.design.px(11.0))
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
                .text_size(colors.design.px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("branch.dialog.edit_remote", name = form.name)),
        )
        .child(
            div()
                .text_size(colors.design.px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .child(t!("branch.dialog.fetch_url").to_string()),
        )
        .child(Input::new(&form.fetch).w_full());
    match &form.push {
        Some(push) => {
            card = card
                .child(
                    div()
                        .text_size(colors.design.px(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .child(t!("branch.dialog.push_url").to_string()),
                )
                .child(Input::new(push).w_full());
        }
        None => {
            card = card.child(
                div()
                    .text_size(colors.design.px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(t!("branch.dialog.push_uses_fetch").to_string()),
            );
        }
    }
    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(colors.design.px(11.0))
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

/// The row the set-upstream dialog starts on: the branch's configured upstream
/// when it is among the choices, otherwise the first one, and nothing when
/// there is nothing to pick.
fn initial_upstream_selection(choices: &[UpstreamChoice], current: Option<&str>) -> Option<usize> {
    current
        .and_then(|current| choices.iter().position(|choice| choice.label == current))
        .or_else(|| (!choices.is_empty()).then_some(0))
}

impl Branches {
    pub(super) fn dialog_overlay(
        &self,
        colors: &Colors,
        entity: &Entity<RepoView>,
    ) -> Option<AnyElement> {
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

    pub(crate) fn dialog_open(&self) -> bool {
        self.dialog.is_some()
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
        let selected = initial_upstream_selection(&choices, current.as_deref());
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

impl RepoView {
    /// Focuses a dialog that has no text field to hand focus to.
    fn focus_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(focus) = self.branches.dialog_focus.clone() {
            window.defer(cx, move |window, cx| window.focus(&focus, cx));
        }
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
                    .reader()
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
