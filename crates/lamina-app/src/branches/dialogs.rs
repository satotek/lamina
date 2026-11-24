//! The branch forms: create, rename, set upstream, and the remote dialogs.
//!
//! Each form is a `gpui_component` dialog whose body is an entity, so the
//! fields, the picked upstream and a refused submit redraw while it is open.

use std::path::Path;

use gpui::{
    App, AppContext as _, ClickEvent, Context, Div, Entity, EntityId, Focusable as _, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div,
};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, WindowExt as _};
use lamina_core::session::RemoteBranch;

use crate::repo_view::RepoView;
use crate::settings::Settings;
use crate::style::Design;

use super::refs::UpstreamChoice;
use super::validate::{validate_branch_name, validate_remote_name, validate_remote_url};
use super::{BranchStart, Branches, Colors, Retry};

/// What the rename dialog renames: the branch, and the remote branch it tracks
/// when it has one.
pub(crate) struct RenameSource {
    pub(crate) branch: String,
    /// The branch's configured remote branch, when it has one.
    pub(crate) upstream: Option<RemoteBranch>,
    /// `origin/topic`, for the dialog's "current upstream" line.
    pub(crate) upstream_label: Option<String>,
}

/// The open branch dialog: which form it shows, and the error the last submit
/// reported.
///
/// It is its own entity so the body redraws as the form changes — a checkbox,
/// the picked upstream, an error after a refused submit — without the view
/// needing a field for a dialog that is open only briefly.
pub(super) struct BranchDialog {
    /// Weak: the dialog asks the view to run the operation and can outlive it.
    view: WeakEntity<RepoView>,
    /// This dialog's own id, so a refused submit can redraw it from `&mut App`.
    entity: EntityId,
    kind: DialogKind,
    error: Option<String>,
    /// `Enter` inside a text field submits, as it does everywhere else in the
    /// app; the dialog's own `Enter` only covers the rest of the card.
    _input_events: Vec<Subscription>,
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
}

struct UpstreamForm {
    branch: String,
    choices: Vec<UpstreamChoice>,
    selected: Option<usize>,
    /// The upstream the branch has now, if any.
    current: Option<String>,
}

struct AddRemoteForm {
    name: Entity<InputState>,
    url: Entity<InputState>,
    fetch: bool,
}

/// Renaming a remote keeps its URLs and moves its tracking branches; the
/// dialog only needs the new name.
struct RenameRemoteForm {
    input: Entity<InputState>,
    name: String,
}

/// The edit-URL dialog. The push field only exists for a remote that already
/// pushes somewhere other than it fetches.
struct EditRemoteForm {
    name: String,
    fetch: Entity<InputState>,
    push: Option<Entity<InputState>>,
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
    /// Opens `kind` as a modal dialog over the window.
    ///
    /// The shared [`crate::ui::FormDialog`] builder traps focus, answers `Esc`
    /// with Cancel, and hands focus back to the root when the card closes. The
    /// form is an entity of its own so the body follows the fields while the
    /// dialog stays open.
    fn open_dialog(
        &mut self,
        kind: DialogKind,
        view: WeakEntity<RepoView>,
        window: &mut Window,
        cx: &mut App,
    ) {
        // The dialog restores whatever had focus when it opened, so the caller
        // hands focus to the root first: a card opened from a context menu
        // would otherwise give focus back to the closing menu.
        let dialog = cx.new(|cx| BranchDialog::new(view.clone(), kind, cx));
        let (title, ok_label) = dialog.read(cx).labels();
        let closed = view;
        crate::ui::FormDialog::new(title, 420.0)
            .content(dialog.clone())
            .cancel("branch-dialog-cancel")
            .submit("branch-dialog-ok", ok_label, |dialog, window, cx| {
                dialog.submit(window, cx);
            })
            .can_submit(|dialog, _| dialog.can_submit())
            .on_close(move |_, cx| {
                // `Esc`, the close button, Cancel and a backdrop click dismiss
                // the window dialog; keep the view in step with it.
                let _ = closed.update(cx, |view, _| view.branches.close_dialog());
            })
            .open(window, cx);
        self.dialog = Some(dialog);
    }

    pub(crate) fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    pub(crate) fn open_create(
        &mut self,
        view: WeakEntity<RepoView>,
        input: Entity<InputState>,
        start: BranchStart,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.open_dialog(
            DialogKind::Create(CreateForm {
                input,
                start,
                checkout: true,
            }),
            view,
            window,
            cx,
        );
    }

    pub(crate) fn close_dialog(&mut self) {
        self.dialog = None;
    }

    pub(crate) fn open_rename(
        &mut self,
        view: WeakEntity<RepoView>,
        input: Entity<InputState>,
        source: RenameSource,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.open_dialog(
            DialogKind::Rename(RenameForm {
                input,
                branch: source.branch,
                upstream: source.upstream,
                upstream_label: source.upstream_label,
                // Renaming the branch on its remote is a push and a delete
                // there, so it is an explicit second step, never the default.
                rename_remote: false,
            }),
            view,
            window,
            cx,
        );
    }

    pub(crate) fn open_upstream(
        &mut self,
        view: WeakEntity<RepoView>,
        branch: String,
        choices: Vec<UpstreamChoice>,
        current: Option<String>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let selected = initial_upstream_selection(&choices, current.as_deref());
        self.open_dialog(
            DialogKind::Upstream(UpstreamForm {
                branch,
                choices,
                selected,
                current,
            }),
            view,
            window,
            cx,
        );
    }

    pub(crate) fn open_add_remote(
        &mut self,
        view: WeakEntity<RepoView>,
        name: Entity<InputState>,
        url: Entity<InputState>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.open_dialog(
            DialogKind::AddRemote(AddRemoteForm {
                name,
                url,
                fetch: true,
            }),
            view,
            window,
            cx,
        );
    }

    pub(crate) fn open_rename_remote(
        &mut self,
        view: WeakEntity<RepoView>,
        input: Entity<InputState>,
        name: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.open_dialog(
            DialogKind::RenameRemote(RenameRemoteForm { input, name }),
            view,
            window,
            cx,
        );
    }

    pub(crate) fn open_edit_remote(
        &mut self,
        view: WeakEntity<RepoView>,
        name: String,
        fetch: Entity<InputState>,
        push: Option<Entity<InputState>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.open_dialog(
            DialogKind::EditRemote(EditRemoteForm { name, fetch, push }),
            view,
            window,
            cx,
        );
    }
}
/// The row the set-upstream dialog starts on: the branch's configured upstream
/// when it is among the choices, otherwise the first one, and nothing when
/// there is nothing to pick.
fn initial_upstream_selection(choices: &[UpstreamChoice], current: Option<&str>) -> Option<usize> {
    current
        .and_then(|current| choices.iter().position(|choice| choice.label == current))
        .or_else(|| (!choices.is_empty()).then_some(0))
}

impl BranchDialog {
    fn new(view: WeakEntity<RepoView>, kind: DialogKind, cx: &mut Context<Self>) -> Self {
        let entity = cx.entity_id();
        let inputs = match &kind {
            DialogKind::Create(form) => vec![form.input.clone()],
            DialogKind::Rename(form) => vec![form.input.clone()],
            DialogKind::Upstream(_) => Vec::new(),
            DialogKind::AddRemote(form) => vec![form.name.clone(), form.url.clone()],
            DialogKind::RenameRemote(form) => vec![form.input.clone()],
            DialogKind::EditRemote(form) => {
                let mut inputs = vec![form.fetch.clone()];
                inputs.extend(form.push.clone());
                inputs
            }
        };
        let mut input_events = Vec::new();
        for input in inputs {
            input_events.push(cx.subscribe(
                &input,
                move |dialog, _input, event: &InputEvent, cx| {
                    if !matches!(
                        event,
                        InputEvent::PressEnter {
                            secondary: false,
                            ..
                        }
                    ) {
                        return;
                    }
                    let Some(handle) = cx.active_window() else {
                        return;
                    };
                    let _ = handle.update(cx, |_, window, cx| dialog.submit(window, cx));
                },
            ));
        }
        Self {
            view,
            entity,
            kind,
            error: None,
            _input_events: input_events,
        }
    }

    /// Validates the form and hands the request to the view, which runs it and
    /// closes the dialog once the operation is on its way.
    fn submit(&mut self, window: &mut Window, cx: &mut App) {
        let request = match self.request(cx) {
            Ok(request) => request,
            // A refused submit leaves the dialog open with the reason.
            Err(message) => {
                self.error = Some(message);
                cx.notify(self.entity);
                return;
            }
        };
        let Some(view) = self.view.upgrade() else {
            return;
        };
        view.update(cx, |view, cx| view.run_branch_dialog(request, window, cx));
        // Closing here would drop this dialog while it is being updated.
        window.defer(cx, |window, cx| window.close_dialog(cx));
    }

    /// The dialog's heading and the footer button that submits it.
    fn labels(&self) -> (String, String) {
        match &self.kind {
            DialogKind::Create(_) => (
                t!("branch.dialog.create").to_string(),
                t!("branch.dialog.create").to_string(),
            ),
            DialogKind::Rename(_) => (
                t!("branch.dialog.rename").to_string(),
                t!("branch.dialog.rename").to_string(),
            ),
            DialogKind::Upstream(_) => (
                t!("branch.dialog.set_upstream").to_string(),
                t!("branch.dialog.set_upstream").to_string(),
            ),
            DialogKind::AddRemote(_) => (
                t!("branch.dialog.add_remote").to_string(),
                t!("branch.dialog.add_remote").to_string(),
            ),
            DialogKind::RenameRemote(_) => (
                t!("branch.dialog.rename_remote").to_string(),
                t!("branch.dialog.rename_remote").to_string(),
            ),
            DialogKind::EditRemote(form) => (
                t!("branch.dialog.edit_remote", name = form.name).to_string(),
                t!("branch.dialog.save_url").to_string(),
            ),
        }
    }

    /// Whether the form can be submitted as it stands. Only the upstream picker
    /// can offer nothing to submit; the other forms report a bad value on
    /// submit and stay open with the reason.
    fn can_submit(&self) -> bool {
        match &self.kind {
            DialogKind::Upstream(form) => form.selected.is_some(),
            _ => true,
        }
    }

    /// The form's contents, validated.
    fn request(&self, cx: &App) -> Result<DialogRequest, String> {
        match &self.kind {
            DialogKind::Create(form) => {
                let name = form.input.read(cx).value().to_string();
                validate_branch_name(&name)?;
                Ok(DialogRequest::Create(CreateRequest {
                    name,
                    start_point: form.start.point.clone(),
                    checkout: form.checkout,
                }))
            }
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
                    return Err(t!("branch.validation.remote_name_differs")
                        .to_string()
                        .to_owned());
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
        }
    }
}

impl Render for BranchDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let colors = Colors {
            sidebar_foreground: design.text,
            muted_foreground: design.muted,
            danger: design.remove_text,
            design,
        };
        let form = match &self.kind {
            DialogKind::Create(form) => create_body(form, &colors, cx),
            DialogKind::Rename(form) => rename_body(form, &colors, cx),
            DialogKind::Upstream(form) => upstream_body(form, &colors, cx),
            DialogKind::AddRemote(form) => add_remote_body(form, &colors, cx),
            DialogKind::RenameRemote(form) => rename_remote_body(form, &colors, cx),
            DialogKind::EditRemote(form) => edit_remote_body(form, &colors, cx),
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap(colors.design.px(8.0))
            .child(form);
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_size(colors.design.px(11.0))
                    .text_color(colors.danger)
                    .child(error.clone()),
            );
        }
        body
    }
}

/// The create form: the field, and whether the new branch is checked out.
fn create_body(form: &CreateForm, colors: &Colors, cx: &mut Context<BranchDialog>) -> Div {
    let toggle = cx.listener(|dialog: &mut BranchDialog, checked: &bool, _, cx| {
        if let DialogKind::Create(form) = &mut dialog.kind {
            form.checkout = *checked;
        }
        cx.notify();
    });
    div()
        .flex()
        .flex_col()
        .gap(colors.design.px(8.0))
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
        )
}

/// The rename form: the new name, and the option to rename the branch on its
/// remote too.
fn rename_body(form: &RenameForm, colors: &Colors, cx: &mut Context<BranchDialog>) -> Div {
    let toggle = cx.listener(|dialog: &mut BranchDialog, checked: &bool, _, cx| {
        if let DialogKind::Rename(form) = &mut dialog.kind {
            form.rename_remote = *checked;
        }
        cx.notify();
    });
    let mut body = div()
        .flex()
        .flex_col()
        .gap(colors.design.px(8.0))
        .child(
            div()
                .text_size(colors.design.px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!("branch.dialog.renaming", branch = form.branch)),
        )
        .child(Input::new(&form.input).w_full());
    match &form.upstream_label {
        Some(label) => {
            body = body.child(
                div()
                    .text_size(colors.design.px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(t!("branch.dialog.current_upstream", label = label)),
            );
            if form.upstream.is_some() {
                body = body
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
                            .child(t!("branch.dialog.rename_remote_detail").to_string()),
                    );
            }
        }
        None => {
            body = body.child(
                div()
                    .text_size(colors.design.px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(t!("branch.dialog.no_upstream").to_string()),
            );
        }
    }
    body
}

/// The set-upstream form: the choices, with the current upstream selected.
fn upstream_body(form: &UpstreamForm, colors: &Colors, cx: &mut Context<BranchDialog>) -> Div {
    let mut body = div().flex().flex_col().gap(colors.design.px(8.0)).child(
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
        body = body.child(
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
                let select =
                    cx.listener(move |dialog: &mut BranchDialog, _: &ClickEvent, _, cx| {
                        if let DialogKind::Upstream(form) = &mut dialog.kind
                            && index < form.choices.len()
                        {
                            form.selected = Some(index);
                            dialog.error = None;
                        }
                        cx.notify();
                    });
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
        body = body.child(
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
    body
}

/// The add-remote form: the name, the URL and whether to fetch right away.
fn add_remote_body(form: &AddRemoteForm, colors: &Colors, cx: &mut Context<BranchDialog>) -> Div {
    let toggle = cx.listener(|dialog: &mut BranchDialog, checked: &bool, _, cx| {
        if let DialogKind::AddRemote(form) = &mut dialog.kind {
            form.fetch = *checked;
        }
        cx.notify();
    });
    div()
        .flex()
        .flex_col()
        .gap(colors.design.px(8.0))
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
        )
}

/// The rename-remote form: the new name alone; the URLs and tracking branches
/// follow it.
fn rename_remote_body(
    form: &RenameRemoteForm,
    colors: &Colors,
    _cx: &mut Context<BranchDialog>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(colors.design.px(8.0))
        .child(
            div()
                .text_size(colors.design.px(11.0))
                .text_color(colors.muted_foreground)
                .child(t!("branch.dialog.renaming_remote", name = form.name)),
        )
        .child(Input::new(&form.input).w_full())
}

/// The edit-URL form: the fetch URL, and the push URL only when the remote
/// already pushes somewhere of its own.
fn edit_remote_body(
    form: &EditRemoteForm,
    colors: &Colors,
    _cx: &mut Context<BranchDialog>,
) -> Div {
    let mut body = div()
        .flex()
        .flex_col()
        .gap(colors.design.px(8.0))
        .child(
            div()
                .text_size(colors.design.px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .child(t!("branch.dialog.fetch_url").to_string()),
        )
        .child(Input::new(&form.fetch).w_full());
    match &form.push {
        Some(push) => {
            body = body
                .child(
                    div()
                        .text_size(colors.design.px(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .child(t!("branch.dialog.push_url").to_string()),
                )
                .child(Input::new(push).w_full());
        }
        None => {
            body = body.child(
                div()
                    .text_size(colors.design.px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(t!("branch.dialog.push_uses_fetch").to_string()),
            );
        }
    }
    body
}

impl RepoView {
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
        self.focus_root(window, cx);
        self.branches.open_rename(
            cx.weak_entity(),
            input,
            RenameSource {
                branch,
                upstream: target,
                upstream_label: label,
            },
            window,
            cx,
        );
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
        self.focus_root(window, cx);
        self.branches
            .open_upstream(cx.weak_entity(), branch, choices, current, window, cx);
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
        self.focus_root(window, cx);
        self.branches
            .open_add_remote(cx.weak_entity(), name, url, window, cx);
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
        self.focus_root(window, cx);
        self.branches
            .open_rename_remote(cx.weak_entity(), input, name, window, cx);
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
        self.focus_root(window, cx);
        self.branches
            .open_edit_remote(cx.weak_entity(), name, fetch, push, window, cx);
        window.defer(cx, move |window, cx| handle.focus(window, cx));
        cx.notify();
    }
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

        assert_eq!(
            initial_upstream_selection(&choices, Some("origin/topic")),
            Some(1),
            "the branch's current upstream is the starting choice"
        );
        assert_eq!(initial_upstream_selection(&choices, None), Some(0));
        assert_eq!(
            initial_upstream_selection(&[], None),
            None,
            "with nothing to choose there is no default upstream"
        );
        assert_eq!(
            initial_upstream_selection(&choices, Some("origin/gone")),
            Some(0),
            "an upstream that is no longer listed falls back to the first row"
        );
    }

    /// The branch forms are window dialogs now, so `Enter` submits, `Esc`
    /// cancels, and the view forgets the dialog once either happens.
    #[gpui::test]
    fn a_branch_dialog_submits_on_enter_and_cancels_on_escape(cx: &mut gpui::TestAppContext) {
        use gpui::VisualTestContext;
        use gpui_component::Root;

        use crate::Launch;

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

        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder = slot.clone();
        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), move |window, cx| {
                    let view =
                        cx.new(|cx| RepoView::new(Launch::open_quiet(&dir, None), window, cx));
                    *holder.borrow_mut() = Some(view.clone());
                    cx.new(|cx| Root::new(view, window, cx))
                })
            })
            .expect("repo window");
        let view = slot.borrow().clone().expect("the repository view");
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.run_until_parked();

        vcx.update(|window, cx| {
            view.update(cx, |view, cx| view.create_branch_from_head(window, cx));
        });
        vcx.run_until_parked();
        assert!(
            vcx.update(|window, cx| window.has_active_dialog(cx)),
            "the create form opens as a window dialog"
        );
        assert!(view.read_with(&vcx, |view, _| view.branches.dialog_open()));

        // An empty name is refused, and the dialog stays open with the reason
        // instead of running `git branch`.
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(view.read_with(&vcx, |view, _| view.branches.dialog_open()));
        let error = view.read_with(&vcx, |view, cx| {
            view.branches
                .dialog
                .as_ref()
                .expect("the dialog is open")
                .read(cx)
                .error
                .clone()
        });
        assert_eq!(
            error.as_deref(),
            Some(t!("branch.validation.name_required").to_string().as_str()),
            "Enter submits and reports why the form was refused"
        );

        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(
            !view.read_with(&vcx, |view, _| view.branches.dialog_open()),
            "Esc cancels and the view forgets the dialog"
        );
        assert!(!vcx.update(|window, cx| window.has_active_dialog(cx)));
    }
}
