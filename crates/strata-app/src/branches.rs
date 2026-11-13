//! Branch management: the sidebar's ref sections, their context menus, and the
//! create/delete dialogs.
//!
//! Nothing here runs git. Every write is an [`Operation`] handed to the
//! window's [`RepoView`], which gives it to the session worker; the sections
//! only read the snapshot the view has already published. The helpers that do
//! not need a window (name validation, upstream labels, remote-to-local name
//! mapping) are unit tested without a repository.

use std::collections::HashMap;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Pixels, Size,
    StatefulInteractiveElement as _, Styled, Window, actions, anchored, deferred, div, hsla, point,
    px,
};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Input, InputState},
    menu::{ContextMenuExt as _, PopupMenuItem},
};
use gpui_kit_assets::IconName;
use strata_core::session::{Operation, OperationId};
use strata_core::{RefKind, UpstreamInfo};

use crate::actions::REPO_VIEW;
use crate::data::SidebarData;
use crate::repo_view::RepoView;
use crate::style::{self, Design};

/// Key context the dialog sets while it is open, so `Enter` and `Escape` mean
/// confirm and cancel only there.
const BRANCH_DIALOG: &str = "BranchDialog";

actions!(
    strata,
    [
        CreateBranchFromHead,
        ConfirmBranchDialog,
        CancelBranchDialog,
    ]
);

// `KeyBinding::new` parses without an OS, so `secondary-` is rejected there.
#[cfg(target_os = "macos")]
const CREATE_BRANCH_KEY: &str = "cmd-shift-b";
#[cfg(not(target_os = "macos"))]
const CREATE_BRANCH_KEY: &str = "ctrl-shift-b";

/// Binds the branch commands. Their handlers live on `RepoView`'s root
/// element, which is on the focus path whenever the history view has focus.
pub fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(CREATE_BRANCH_KEY, CreateBranchFromHead, Some(REPO_VIEW)),
        KeyBinding::new("enter", ConfirmBranchDialog, Some(BRANCH_DIALOG)),
        KeyBinding::new("escape", CancelBranchDialog, Some(BRANCH_DIALOG)),
    ]);
}

/// The colours the sections and dialogs paint with, taken from the window
/// chrome's palette so the sidebar reads as one surface.
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Local,
    Remote,
    Tags,
    Stashes,
}

impl Section {
    const ALL: [Section; 4] = [
        Section::Local,
        Section::Remote,
        Section::Tags,
        Section::Stashes,
    ];

    fn index(self) -> usize {
        match self {
            Section::Local => 0,
            Section::Remote => 1,
            Section::Tags => 2,
            Section::Stashes => 3,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Section::Local => "BRANCHES",
            Section::Remote => "REMOTES",
            Section::Tags => "TAGS",
            Section::Stashes => "STASHES",
        }
    }

    fn icon(self) -> &'static [u8] {
        match self {
            Section::Local => style::glyph::BRANCH,
            Section::Remote => style::glyph::CLOUD,
            Section::Tags => style::glyph::TAG,
            Section::Stashes => style::glyph::ARCHIVE,
        }
    }

    fn empty_label(self) -> &'static str {
        match self {
            Section::Local => "no local branches",
            Section::Remote => "no remote branches",
            Section::Tags => "no tags",
            Section::Stashes => "no stashes",
        }
    }

    /// Whether `history` settings hide this section.
    fn hidden(self, design: &Design) -> bool {
        match self {
            Section::Local => false,
            Section::Remote => !design.show_remote_branches,
            Section::Tags => !design.show_tags,
            Section::Stashes => !design.show_stashes,
        }
    }
}

/// Where a new branch should start. The label is user-facing, e.g. `from main`
/// or `from commit 4f2a1c9`.
pub(crate) struct BranchStart {
    pub(crate) point: Option<String>,
    pub(crate) label: String,
}

/// Everything the branch UI owns between frames.
#[derive(Default)]
pub(crate) struct Branches {
    collapsed: [bool; 4],
    /// Remote name -> collapsed, so each remote's branch list folds on its own.
    remote_groups: HashMap<String, bool>,
    dialog: Option<Dialog>,
    /// Operations this module started, keyed by the id the session returned.
    /// The label is what the status bar prefixes a failure with.
    running: HashMap<OperationId, Running>,
    /// Last viewport size seen, so a dialog opened from an operation result can
    /// still paint a window-sized backdrop.
    viewport: Size<Pixels>,
    /// Focus target for dialog cards. Created once by the view because a
    /// `FocusHandle` needs an `App`, and reused: only one dialog is open and it
    /// is the only element tracking the handle while it lives.
    dialog_focus: Option<FocusHandle>,
}

struct Running {
    label: String,
    /// Set for a delete, which can be offered again with `force`.
    delete: Option<RunningDelete>,
}

struct RunningDelete {
    name: String,
    forced: bool,
    viewport: Size<Pixels>,
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
    Delete(DeleteForm),
}

struct CreateForm {
    input: Entity<InputState>,
    start: BranchStart,
    checkout: bool,
    error: Option<String>,
}

struct DeleteForm {
    name: String,
    /// `true` once the plain delete has failed and the button must force.
    force: bool,
    error: Option<String>,
}

/// The validated contents of the create dialog.
pub(crate) struct CreateRequest {
    pub(crate) name: String,
    pub(crate) start_point: Option<String>,
    pub(crate) checkout: bool,
}

impl Branches {
    /// Renders the scrollable ref sections and, when one is open, the dialog on
    /// top of the window.
    pub(crate) fn render(
        &self,
        data: &SidebarData,
        upstreams: &HashMap<String, UpstreamInfo>,
        enabled: bool,
        busy: bool,
        colors: &Colors,
        entity: &Entity<RepoView>,
    ) -> AnyElement {
        let head = data.head_branch.as_deref();

        let mut column = div()
            .id("branch-sections")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();

        let mut remote_row_index = 0;
        for section in Section::ALL {
            if section.hidden(&colors.design) {
                continue;
            }
            let entries = match section {
                Section::Local => &data.local,
                Section::Remote => &data.remote,
                Section::Tags => &data.tags,
                Section::Stashes => &data.stashes,
            };
            column =
                column.child(self.section_header(section, entries.len(), busy, colors, entity));

            if self.collapsed[section.index()] {
                continue;
            }
            if entries.is_empty() {
                column = column.child(empty_row(section.empty_label(), colors));
                continue;
            }

            // Remote branches are grouped under their remote, which is the
            // first path component of `origin/main`.
            if section == Section::Remote {
                for (group, branches) in remote_groups(entries) {
                    column = column.child(self.remote_group_header(
                        &group,
                        branches.len(),
                        colors,
                        entity,
                    ));
                    if self.remote_groups.get(&group).copied().unwrap_or(false) {
                        continue;
                    }
                    for name in &branches {
                        column = column.child(self.remote_row(
                            name,
                            remote_row_index,
                            &data.local,
                            enabled,
                            busy,
                            colors,
                            entity,
                        ));
                        remote_row_index += 1;
                    }
                }
                continue;
            }

            for (index, name) in entries.iter().enumerate() {
                column = column.child(match section {
                    Section::Local => self.local_row(
                        name,
                        index,
                        head == Some(name.as_str()),
                        upstreams.get(name),
                        enabled,
                        busy,
                        colors,
                        entity,
                    ),
                    Section::Remote => unreachable!("remote rows are grouped above"),
                    Section::Tags => {
                        let tag = name.clone();
                        let entity = entity.clone();
                        row_with_icon(Some(style::glyph::TAG), name, colors)
                            .id(("tag", index))
                            .cursor_pointer()
                            .on_click(move |_, window, cx| {
                                entity.update(cx, |view, cx| {
                                    view.reveal_ref(RefKind::Tag, &tag, window, cx)
                                });
                            })
                            .into_any_element()
                    }
                    // Stash rows are listed in stash@{N} order. A click shows the
                    // stash commit in the history; the menu applies, pops or
                    // drops the entry.
                    Section::Stashes => {
                        let menu_source = entity.clone();
                        let click_entity = entity.clone();
                        let row_element = row(name, colors)
                            .id(("stash", index))
                            .cursor_pointer()
                            .on_click(move |_, window, cx| {
                                click_entity
                                    .update(cx, |view, cx| view.reveal_stash(index, window, cx));
                            });
                        if !enabled {
                            row_element.into_any_element()
                        } else {
                            row_element
                                .context_menu(move |menu, _window, _cx| {
                                    let menu_entity = menu_source.clone();
                                    let apply = {
                                        let entity = menu_entity.clone();
                                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                            entity.update(cx, |view, cx| {
                                                view.stash_apply(index, window, cx)
                                            });
                                        }
                                    };
                                    let pop = {
                                        let entity = menu_entity.clone();
                                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                            entity.update(cx, |view, cx| {
                                                view.stash_pop(index, window, cx)
                                            });
                                        }
                                    };
                                    let drop =
                                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                            menu_entity.update(cx, |view, cx| {
                                                view.confirm_stash_drop(index, window, cx)
                                            });
                                        };
                                    menu.item(
                                        PopupMenuItem::new("Apply").disabled(busy).on_click(apply),
                                    )
                                    .item(PopupMenuItem::new("Pop").disabled(busy).on_click(pop))
                                    .separator()
                                    .item(PopupMenuItem::new("Drop…").disabled(busy).on_click(drop))
                                })
                                .into_any_element()
                        }
                    }
                });
            }
        }

        if let Some(dialog) = self.dialog_overlay(colors, entity) {
            column = column.child(dialog);
        }
        column.into_any_element()
    }

    fn section_header(
        &self,
        section: Section,
        count: usize,
        busy: bool,
        colors: &Colors,
        entity: &Entity<RepoView>,
    ) -> AnyElement {
        let open = !self.collapsed[section.index()];
        let index = section.index();
        let mut header = div()
            .id(("branch-section", index))
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(26.0))
            .px_2()
            .rounded(px(7.0))
            .cursor_pointer()
            .text_size(px(colors.design.tiny()))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(colors.muted_foreground)
            .hover(|style| style.bg(colors.design.selected))
            .child(icon(
                if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                12.0,
                colors.muted_foreground,
            ))
            .child(glyph(section.icon(), 14.0, colors.muted_foreground))
            .child(div().flex_1().min_w_0().truncate().child(section.title()))
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::NORMAL)
                    .child(count.to_string()),
            );
        if busy && section == Section::Local {
            header = header.child(
                div()
                    .flex_none()
                    .text_size(px(9.0))
                    .font_weight(FontWeight::NORMAL)
                    .child("working…"),
            );
        }

        let entity = entity.clone();
        header
            .on_click(move |_, _, cx| {
                entity.update(cx, |view, cx| {
                    view.branches.toggle(index);
                    cx.notify();
                });
            })
            .into_any_element()
    }

    /// A remote's own collapsing header, above its branch list.
    fn remote_group_header(
        &self,
        group: &str,
        count: usize,
        colors: &Colors,
        entity: &Entity<RepoView>,
    ) -> AnyElement {
        let collapsed = self.remote_groups.get(group).copied().unwrap_or(false);
        let group = group.to_owned();
        let click_group = group.clone();
        let entity = entity.clone();
        div()
            .id(format!("remote-group-{group}"))
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(28.0))
            .px_2()
            .rounded(px(7.0))
            .cursor_pointer()
            .text_size(px(colors.design.small()))
            .text_color(colors.sidebar_foreground)
            .hover(|style| style.bg(colors.design.selected))
            .on_click(move |_, _, cx| {
                let group = click_group.clone();
                entity.update(cx, |view, cx| {
                    view.branches.toggle_remote_group(group);
                    cx.notify();
                });
            })
            .child(icon(
                if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                },
                12.0,
                colors.muted_foreground,
            ))
            .child(glyph(style::glyph::CLOUD, 14.0, colors.sidebar_foreground))
            .child(div().flex_1().min_w_0().truncate().child(group))
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(colors.muted_foreground)
                    .child(count.to_string()),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn local_row(
        &self,
        name: &str,
        index: usize,
        active: bool,
        upstream: Option<&UpstreamInfo>,
        enabled: bool,
        busy: bool,
        colors: &Colors,
        entity: &Entity<RepoView>,
    ) -> AnyElement {
        let mut row = row_with_icon(Some(style::glyph::BRANCH), name, colors)
            .id(("local-branch", index))
            .cursor_pointer()
            .when(active, |row| {
                row.bg(colors.design.selected)
                    .font_weight(FontWeight::SEMIBOLD)
            });
        if let Some(label) = upstream.and_then(upstream_label) {
            row = row.child(
                div()
                    .flex_none()
                    .text_size(px(colors.design.tiny()))
                    .text_color(colors.muted_foreground)
                    .child(label),
            );
        }

        if !enabled {
            return row.into_any_element();
        }

        // One click shows the branch in the graph; a second one checks it out.
        let click_entity = entity.clone();
        let branch = name.to_owned();
        row = row.on_click(move |event, window, cx| {
            let branch = branch.clone();
            click_entity.update(cx, |view, cx| {
                if event.click_count() < 2 {
                    view.reveal_ref(RefKind::LocalBranch, &branch, window, cx);
                } else if !busy && !active {
                    let label = format!("check out {branch}");
                    view.run_operation(Operation::Checkout { branch }, label, window, cx);
                }
            });
        });

        let entity_menu = entity.clone();
        let branch = name.to_owned();
        row.context_menu(move |menu, _window, _cx| {
            menu.item(
                PopupMenuItem::new("Checkout")
                    .disabled(busy || active)
                    .on_click({
                        let entity = entity_menu.clone();
                        let branch = branch.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let branch = branch.clone();
                            entity.update(cx, |view, cx| {
                                let label = format!("check out {branch}");
                                view.run_operation(
                                    Operation::Checkout { branch },
                                    label,
                                    window,
                                    cx,
                                );
                            });
                        }
                    }),
            )
            .item(
                PopupMenuItem::new("Merge into current branch")
                    .disabled(busy || active)
                    .on_click({
                        let entity = entity_menu.clone();
                        let branch = branch.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let branch = branch.clone();
                            entity.update(cx, |view, cx| {
                                let label = format!("merge {branch} into the current branch");
                                view.run_operation(
                                    Operation::Merge {
                                        branch,
                                        no_ff: false,
                                    },
                                    label,
                                    window,
                                    cx,
                                );
                            });
                        }
                    }),
            )
            .item(
                PopupMenuItem::new("Create branch from here…")
                    .disabled(busy)
                    .on_click({
                        let entity = entity_menu.clone();
                        let point = branch.clone();
                        let from = format!("from {branch}");
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
                PopupMenuItem::new("Delete…")
                    .disabled(busy || active)
                    .on_click({
                        let entity = entity_menu.clone();
                        let branch = branch.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let branch = branch.clone();
                            entity.update(cx, |view, cx| {
                                view.open_delete_dialog(branch, window, cx);
                            });
                        }
                    }),
            )
        })
        .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn remote_row(
        &self,
        name: &str,
        index: usize,
        local: &[String],
        enabled: bool,
        busy: bool,
        colors: &Colors,
        entity: &Entity<RepoView>,
    ) -> AnyElement {
        let label = local_name_for_remote(name).unwrap_or(name);
        let row_element = row(label, colors).id(("remote-branch", index)).pl(px(42.0));
        let Some(local_name) = local_name_for_remote(name).map(str::to_owned) else {
            return row_element.into_any_element();
        };
        if !enabled {
            return row_element.into_any_element();
        }

        // Double-clicking `origin/foo` switches to `foo` when it exists and
        // creates it from the remote ref otherwise.
        let existing = local.iter().any(|branch| branch == &local_name);
        let row_element = {
            let click_entity = entity.clone();
            let remote = name.to_owned();
            let target = local_name.clone();
            row_element
                .cursor_pointer()
                .on_click(move |event, window, cx| {
                    let remote = remote.clone();
                    let target = target.clone();
                    click_entity.update(cx, |view, cx| {
                        if event.click_count() < 2 {
                            view.reveal_ref(RefKind::RemoteBranch, &remote, window, cx);
                        } else if !busy {
                            checkout_remote(view, &remote, &target, existing, window, cx)
                        }
                    });
                })
        };

        let entity_menu = entity.clone();
        let remote = name.to_owned();
        row_element
            .context_menu(move |menu, _window, _cx| {
                menu.item(PopupMenuItem::new("Checkout").disabled(busy).on_click({
                    let entity = entity_menu.clone();
                    let remote = remote.clone();
                    let target = local_name.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let remote = remote.clone();
                        let target = target.clone();
                        entity.update(cx, |view, cx| {
                            checkout_remote(view, &remote, &target, existing, window, cx)
                        });
                    }
                }))
                .item(
                    PopupMenuItem::new("Merge into current branch")
                        .disabled(busy)
                        .on_click({
                            let entity = entity_menu.clone();
                            let remote = remote.clone();
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let remote = remote.clone();
                                entity.update(cx, |view, cx| {
                                    let label = format!("merge {remote} into the current branch");
                                    view.run_operation(
                                        Operation::Merge {
                                            branch: remote,
                                            no_ff: false,
                                        },
                                        label,
                                        window,
                                        cx,
                                    );
                                });
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new("Create branch from here…")
                        .disabled(busy)
                        .on_click({
                            let entity = entity_menu.clone();
                            let remote = remote.clone();
                            let from = format!("from {remote}");
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let remote = remote.clone();
                                let from = from.clone();
                                entity.update(cx, |view, cx| {
                                    view.open_create_dialog(Some(remote), from, window, cx);
                                });
                            }
                        }),
                )
            })
            .into_any_element()
    }

    fn dialog_overlay(&self, colors: &Colors, entity: &Entity<RepoView>) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let card = match &dialog.kind {
            DialogKind::Create(form) => create_card(form, colors, entity, dialog.focus.as_ref()),
            DialogKind::Delete(form) => delete_card(form, colors, entity, dialog.focus.as_ref()),
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

    pub(crate) fn start(
        &mut self,
        id: OperationId,
        label: String,
        delete: Option<(String, bool)>,
        viewport: Size<Pixels>,
    ) {
        let delete = delete.map(|(name, forced)| RunningDelete {
            name,
            forced,
            viewport,
        });
        self.running.insert(id, Running { label, delete });
    }

    /// Records that an operation finished. Returns the label it was started
    /// with so the caller can name the operation in the status bar, and opens
    /// the force-delete offer when git refused an unforced delete.
    pub(crate) fn finish(&mut self, id: OperationId, error: Option<&str>) -> Option<String> {
        let running = self.running.remove(&id)?;
        if let (Some(error), Some(delete)) = (error, running.delete)
            && !delete.forced
            && is_not_fully_merged(error)
        {
            self.dialog = Some(Dialog {
                kind: DialogKind::Delete(DeleteForm {
                    name: delete.name,
                    force: true,
                    error: Some("The branch is not fully merged.".to_owned()),
                }),
                viewport: delete.viewport,
                focus: self.dialog_focus.clone(),
            });
        }
        Some(running.label)
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.running.is_empty()
    }

    pub(crate) fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    pub(crate) fn toggle(&mut self, index: usize) {
        if let Some(collapsed) = self.collapsed.get_mut(index) {
            *collapsed = !*collapsed;
        }
    }

    pub(crate) fn toggle_remote_group(&mut self, group: String) {
        let collapsed = self.remote_groups.entry(group).or_insert(false);
        *collapsed = !*collapsed;
    }

    pub(crate) fn set_viewport(&mut self, viewport: Size<Pixels>) {
        self.viewport = viewport;
    }

    pub(crate) fn set_dialog_focus(&mut self, focus: FocusHandle) {
        self.dialog_focus = Some(focus);
    }

    /// The handle a dialog card tracks, if the view has created one.
    pub(crate) fn dialog_focus(&self) -> Option<FocusHandle> {
        self.dialog_focus.clone()
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

    pub(crate) fn open_delete(&mut self, name: String) {
        self.dialog = Some(Dialog {
            kind: DialogKind::Delete(DeleteForm {
                name,
                force: false,
                error: None,
            }),
            viewport: self.viewport,
            focus: self.dialog_focus.clone(),
        });
    }

    pub(crate) fn close_dialog(&mut self) {
        self.dialog = None;
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

    pub(crate) fn set_create_error(&mut self, message: String) {
        if let Some(Dialog {
            kind: DialogKind::Create(form),
            ..
        }) = &mut self.dialog
        {
            form.error = Some(message);
        }
    }

    /// The create dialog's input, validated. `None` when it is not open.
    pub(crate) fn create_request(&self, cx: &App) -> Option<Result<CreateRequest, String>> {
        let DialogKind::Create(form) = &self.dialog.as_ref()?.kind else {
            return None;
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

    /// The delete dialog's target, or `None` when it is not open.
    pub(crate) fn delete_request(&self) -> Option<(String, bool)> {
        let DialogKind::Delete(form) = &self.dialog.as_ref()?.kind else {
            return None;
        };
        Some((form.name.clone(), form.force))
    }
}

/// Checks out `remote` as `local`, creating `local` from the remote ref when it
/// does not exist yet.
fn checkout_remote(
    view: &mut RepoView,
    remote: &str,
    local: &str,
    existing: bool,
    window: &mut Window,
    cx: &mut Context<RepoView>,
) {
    if existing {
        let label = format!("check out {local}");
        view.run_operation(
            Operation::Checkout {
                branch: local.to_owned(),
            },
            label,
            window,
            cx,
        );
    } else {
        let label = format!("create branch {local} from {remote}");
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

fn row(name: &str, colors: &Colors) -> Div {
    row_with_icon(None, name, colors)
}

fn row_with_icon(item_icon: Option<&'static [u8]>, name: &str, colors: &Colors) -> Div {
    let name = strip_stash_prefix(name);
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .h(px(28.0))
        .px_2()
        .rounded(px(7.0))
        .text_size(px(colors.design.small()))
        .text_color(colors.sidebar_foreground)
        .when_some(item_icon, |row, item_icon| {
            row.child(glyph(item_icon, 14.0, colors.muted_foreground))
        })
        .child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
}

fn icon(name: IconName, size: f32, color: Hsla) -> Icon {
    Icon::new(name).with_size(px(size)).text_color(color)
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

/// `origin/main`, `origin/feat/x`, `upstream/main` grouped by their first path
/// component, preserving the order the refs arrived in.
fn remote_groups(entries: &[String]) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for name in entries {
        let group = name.split('/').next().unwrap_or(name).to_owned();
        match groups.iter_mut().find(|(existing, _)| *existing == group) {
            Some((_, branches)) => branches.push(name.clone()),
            None => groups.push((group, vec![name.clone()])),
        }
    }
    groups
}

fn empty_row(text: &str, colors: &Colors) -> Div {
    div()
        .pl(px(38.0))
        .pr_2()
        .h(px(24.0))
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
                .child("Create branch"),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(format!("New branch {}", form.start.label)),
        )
        .child(Input::new(&form.input).w_full())
        .child(
            Checkbox::new("branch-create-checkout")
                .checked(form.checkout)
                .label("Check out after creating")
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
                Button::new("branch-create-cancel")
                    .label("Cancel")
                    .on_click(cancel),
            )
            .child(
                Button::new("branch-create-ok")
                    .label("Create branch")
                    .primary()
                    .on_click(confirm),
            ),
    )
    .into_any_element()
}

fn delete_card(
    form: &DeleteForm,
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

    let mut card = dialog_card(colors, entity, focus)
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("Delete branch {}", form.name)),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(if form.force {
                    "The branch is not fully merged. Forcing the delete loses the commits it points at."
                } else {
                    "Delete this branch? Its commits stay in the repository until they are garbage collected."
                }),
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
                Button::new("branch-delete-cancel")
                    .label("Cancel")
                    .on_click(cancel),
            )
            .child(
                Button::new("branch-delete-ok")
                    .label(if form.force { "Force delete" } else { "Delete" })
                    .danger()
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
        .w(px(380.0))
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

/// `git check-ref-format` rules that apply to `refs/heads/<name>`, plus the one
/// rule the CLI enforces by accident: a leading `-` would parse as an option.
pub(crate) fn validate_branch_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Enter a branch name.");
    }
    if name == "@" {
        return Err("'@' is shorthand for HEAD, not a branch name.");
    }
    if name.starts_with('-') {
        return Err("A branch name must not start with '-'.");
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("A branch name must not start or end with '/'.");
    }
    if name.contains("//") {
        return Err("A branch name must not contain '//'.");
    }
    if name.contains("..") {
        return Err("A branch name must not contain '..'.");
    }
    if name.contains("@{") {
        return Err("A branch name must not contain '@{'.");
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("A branch name must not contain spaces or control characters.");
    }
    if name.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err("A branch name must not contain any of ~ ^ : ? * [ \\.");
    }
    if name.ends_with('.') {
        return Err("A branch name must not end with '.'.");
    }
    if name.split('/').any(|part| {
        part.is_empty() || part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock")
    }) {
        return Err("Each part must not start with '.' or end with '.' or '.lock'.");
    }
    Ok(())
}

/// The sidebar label for a branch's upstream: `↑2 ↓1`, `↑3`, `↓1`, `gone`, or
/// nothing when the branch is level with its upstream.
pub(crate) fn upstream_label(upstream: &UpstreamInfo) -> Option<String> {
    if upstream.gone {
        return Some("gone".to_owned());
    }
    let mut label = String::new();
    if upstream.ahead > 0 {
        label.push_str(&format!("↑{}", upstream.ahead));
    }
    if upstream.behind > 0 {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push_str(&format!("↓{}", upstream.behind));
    }
    (!label.is_empty()).then_some(label)
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

/// `git branch -d` refuses a branch with commits that are not reachable from
/// HEAD; that message is what turns the delete dialog into a force offer.
fn is_not_fully_merged(error: &str) -> bool {
    error.contains("not fully merged")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(name: &str, ahead: u32, behind: u32, gone: bool) -> UpstreamInfo {
        UpstreamInfo {
            name: name.to_owned(),
            ahead,
            behind,
            gone,
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
            upstream_label(&upstream("origin/gone", 0, 0, true)).as_deref(),
            Some("gone"),
            "a deleted upstream is more important than the counts"
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
    fn a_failed_unforced_delete_opens_the_force_offer() {
        let mut branches = Branches::default();
        branches.start(
            OperationId(7),
            "delete branch topic".to_owned(),
            Some(("topic".to_owned(), false)),
            Size::default(),
        );
        let label = branches.finish(
            OperationId(7),
            Some("error: the branch 'topic' is not fully merged"),
        );
        assert_eq!(label.as_deref(), Some("delete branch topic"));
        assert_eq!(
            branches.delete_request(),
            Some(("topic".to_owned(), true)),
            "the offer must rerun the delete with force"
        );
    }

    #[test]
    fn a_forced_delete_failure_does_not_loop() {
        let mut branches = Branches::default();
        branches.start(
            OperationId(8),
            "delete branch topic".to_owned(),
            Some(("topic".to_owned(), true)),
            Size::default(),
        );
        branches.finish(
            OperationId(8),
            Some("error: the branch 'topic' is not fully merged"),
        );
        assert_eq!(branches.delete_request(), None);
    }
}
