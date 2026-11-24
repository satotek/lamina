//! The rows the sidebar draws, the menus they offer, and the branch drop menu.

use std::path::{Path, PathBuf};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Div, ElementId, Entity,
    FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Point,
    Render, Stateful, StatefulInteractiveElement as _, Styled, Window, anchored, deferred, div,
    point, px,
};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, Sizable as _};
use gpui_kit_assets::IconName;
use lamina_core::repo::{SubmoduleInfo, SubmoduleStatus, WorktreeInfo};
use lamina_core::session::{Operation, RepoLocation};
use lamina_core::{HeadInfo, RefKind};

use crate::pull_requests::BranchPullRequests;
use crate::repo_view::RepoView;
use crate::repo_view::rebase_onto::rebase_menu_item;
use crate::style::{self, Design};
use crate::ui::RowTipExt as _;

use super::refs::{local_name_for_remote, ref_full_name};
use super::tree::SidebarItem;
use super::{Branches, Colors, ExtraSelection, ITEM_HEIGHT, RevealState, Section, same_path};

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
pub(super) struct DropMenu {
    position: Point<Pixels>,
    source: DraggedBranch,
    target: String,
    entries: Vec<DropEntry>,
}

/// Builds the element for one visible list item.
#[allow(clippy::too_many_arguments)]
pub(super) fn item_element(
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
            empty_row_at(text, colors.design.px(42.0), colors).into_any_element()
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
                colors.design.scale(14.0),
                colors.muted_foreground,
            ))
            // A list item is only as tall as the heading measured, so every
            // section heading must match a row's height.
            .h(colors.design.px(ITEM_HEIGHT))
            .pr(colors.design.px(18.0))
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
                .text_size(colors.design.px(9.0))
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
    .h(colors.design.px(ITEM_HEIGHT))
    .w_full()
    .min_w_0()
    .overflow_hidden()
    .mx(px(0.0))
    .pr(colors.design.px(18.0))
    .pl(colors.design.px(base + depth as f32 * 14.0))
    .rounded(colors.design.px(7.0))
    .text_size(px(colors.design.small()))
    .text_color(colors.sidebar_foreground)
    .cursor_pointer()
    .child(
        Icon::new(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .with_size(colors.design.px(12.0))
        .text_color(colors.muted_foreground),
    )
    .child(
        Icon::new(if open {
            IconName::FolderOpen
        } else {
            IconName::FolderClosed
        })
        .with_size(colors.design.px(13.0))
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
            .p(self.design.px(2.0))
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

/// Actions the helper does not run stay unavailable on a remote repository,
/// ahead of a merely busy local one.
fn prefer_ssh(ssh: bool, reason: Option<String>) -> Option<String> {
    if ssh {
        Some(crate::remote_open::unavailable_reason())
    } else {
        reason
    }
}

/// The section-level actions of the two lists git stores outside the refs.
fn section_menu(
    menu: PopupMenu,
    section: Section,
    busy: bool,
    entity: &Entity<RepoView>,
    cx: &App,
) -> PopupMenu {
    let ssh = entity.read(cx).is_ssh();
    match section {
        Section::Remote => {
            let add = {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |view, cx| view.open_add_remote(window, cx));
                }
            };
            let blocked = prefer_ssh(ssh, busy.then_some(t!("branch.reason.busy").to_string()));
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
            // Update runs on the helper. Opening a submodule as a local
            // checkout stays closed; that is the row menu, not this one.
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
                colors.design.scale(14.0),
                colors.sidebar_foreground,
            ))
            .text_size(px(colors.design.small()))
            .text_color(colors.sidebar_foreground)
            .h(colors.design.px(ITEM_HEIGHT))
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
        move |menu, _window, cx| {
            let ssh = menu_entity.read(cx).is_ssh();
            let fetch_blocked = busy.then_some(t!("branch.reason.busy").to_string());
            let manage_blocked = prefer_ssh(ssh, fetch_blocked.clone());
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
                    fetch_blocked.as_ref(),
                ))
                .disabled(fetch_blocked.is_some())
                .on_click(fetch),
            )
            .separator()
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.rename").to_string(),
                    manage_blocked.as_ref(),
                ))
                .disabled(manage_blocked.is_some())
                .on_click(rename),
            )
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.edit_url").to_string(),
                    manage_blocked.as_ref(),
                ))
                .disabled(manage_blocked.is_some())
                .on_click(edit_url),
            )
            .separator()
            .item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.remove").to_string(),
                    manage_blocked.as_ref(),
                ))
                .disabled(manage_blocked.is_some())
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
    pull_request: Option<&lamina_core::forge::PullRequestSummary>,
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
    .pl(colors.design.px(depth as f32 * 14.0))
    .cursor_pointer();
    if upstream_gone {
        let mut marker = div()
            .id(("local-branch-upstream-gone", index))
            .flex_none()
            .flex()
            .items_center()
            .child(
                Icon::new(IconName::Unlink)
                    .with_size(colors.design.px(12.0))
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
                .with_size(colors.design.px(12.0))
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
            let three_way_blocked =
                crate::repo_view::compare::three_way_with_current_branch_blocked(
                    view.session.is_some(),
                    active,
                    busy,
                    head,
                    view.compare_branch_tip(&branch),
                    // The ancestry read is only worth it when the entry could
                    // run; the permanent reasons win over it anyway.
                    if active || busy {
                        None
                    } else {
                        view.compare_branch_merge_base(&branch)
                    },
                );
            let three_way = {
                let entity = entity_menu.clone();
                let branch = branch.clone();
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("branch.menu.compare_three_way").to_string(),
                    three_way_blocked.as_ref(),
                ))
                .disabled(three_way_blocked.is_some())
                .on_click(
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let branch = branch.clone();
                        entity.update(cx, |view, cx| {
                            view.open_three_way_with_current_branch(&branch, window, cx)
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
                .item(three_way)
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
    .pl(colors.design.px(42.0 + depth as f32 * 14.0));
    if hidden {
        row_element = row_element.child(
            Icon::new(IconName::EyeOff)
                .with_size(colors.design.px(12.0))
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
    .pl(colors.design.px(depth as f32 * 14.0))
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
        move |menu, _window, cx| {
            let blocked = prefer_ssh(
                menu_entity.read(cx).is_ssh(),
                busy.then_some(t!("branch.reason.busy").to_string()),
            );
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
    .h(colors.design.px(ITEM_HEIGHT))
    .w_full()
    .min_w_0()
    .overflow_hidden()
    .mx(px(0.0))
    .pr(colors.design.px(18.0))
    .rounded(colors.design.px(7.0))
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
        move |menu, _window, cx| {
            let ssh = menu_entity.read(cx).is_ssh();
            let open_blocked = prefer_ssh(
                ssh,
                (status == SubmoduleStatus::Uninitialised)
                    .then_some(t!("branch.submodule.not_initialised").to_string()),
            );
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
    .h(colors.design.px(ITEM_HEIGHT))
    .w_full()
    .min_w_0()
    .overflow_hidden()
    .mx(px(0.0))
    .pr(colors.design.px(18.0))
    .rounded(colors.design.px(7.0))
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
        move |menu, _window, cx| {
            let ssh = menu_entity.read(cx).is_ssh();
            let open_in_window_blocked = prefer_ssh(
                ssh,
                if prunable {
                    Some(t!("branch.reason.missing_worktree").to_string())
                } else if current {
                    Some(t!("branch.reason.already_open").to_string())
                } else {
                    None
                },
            );
            let open_new_window_blocked = prefer_ssh(
                ssh,
                prunable.then_some(t!("branch.reason.missing_worktree").to_string()),
            );
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
pub(super) fn submodule_badge(status: SubmoduleStatus) -> Option<(String, crate::ui::BadgeTone)> {
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
pub(super) fn submodule_detail(submodule: &SubmoduleInfo) -> String {
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

fn commit_label(commit: Option<lamina_core::Oid>) -> String {
    commit.map_or_else(
        || t!("branch.detail.none").to_string().to_owned(),
        crate::data::short_oid,
    )
}

/// A worktree row's label: the branch it has checked out, or the commit a
/// detached checkout is on.
pub(super) fn worktree_label(worktree: &WorktreeInfo) -> String {
    match &worktree.head {
        Some(HeadInfo::Branch { short_name, .. }) => short_name.clone(),
        Some(HeadInfo::Detached { target }) => {
            let detached = t!(
                "branch.detail.detached",
                oid = crate::data::short_oid(*target)
            )
            .to_string();
            // Two detached checkouts named only by their commit are
            // indistinguishable; the directory's last component tells them
            // apart while the row's tooltip keeps the full path.
            match worktree
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
            {
                Some(name) => t!(
                    "branch.detail.detached_named",
                    detached = detached,
                    name = name
                )
                .to_string(),
                None => detached,
            }
        }
        None => t!("branch.detail.unknown_head").to_string().to_owned(),
    }
}

/// A worktree row's tooltip: its path, and why it cannot be removed when git
/// says so.
pub(super) fn worktree_detail(worktree: &WorktreeInfo) -> String {
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
        .h(colors.design.px(ITEM_HEIGHT))
        // A list item is laid out at its content width unless it is told to
        // fill, which would leave most of the row outside its hitbox.
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .mx(px(0.0))
        // Leave room on the right so trailing labels stay clear of the
        // overlay vertical scrollbar.
        .pr(colors.design.px(18.0))
        .rounded(colors.design.px(7.0))
        .text_size(px(colors.design.small()))
        .text_color(colors.sidebar_foreground);
    let row = match item_icon {
        Some(item_icon) => row.child(
            glyph(
                item_icon,
                colors.design.scale(14.0),
                colors.muted_foreground,
            )
            .flex_none(),
        ),
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

/// A section's "nothing here" line. Its indent follows the place it belongs
/// to: branch rows sit under the section labels, remote branches under a
/// remote's own header.
fn empty_row_at(text: &str, indent: Pixels, colors: &Colors) -> Div {
    div()
        .pl(indent)
        .pr_2()
        .h(colors.design.px(ITEM_HEIGHT))
        .flex()
        .items_center()
        .text_size(colors.design.px(11.0))
        .text_color(colors.muted_foreground)
        .child(text.to_owned())
}

impl Branches {
    /// The menu a branch dropped on another branch opened, at the drop point.
    ///
    /// It is a small card rather than a context menu because it is not the
    /// consequence of a right-click; the rows are the same list rows the rest
    /// of the sidebar uses. Clicking anywhere else dismisses it.
    pub(super) fn drop_menu_overlay(
        &self,
        colors: &Colors,
        entity: &Entity<RepoView>,
    ) -> Option<AnyElement> {
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
            .gap(design.px(1.0))
            .min_w(design.px(220.0))
            .max_w(design.px(340.0))
            .p(design.px(4.0))
            .rounded(design.px(8.0))
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
            .h(design.px(26.0))
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .mx(px(0.0))
            .px_2()
            .rounded(design.px(5.0))
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
}

impl RepoView {
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
        if source.kind == RefKind::RemoteBranch
            && matches!(session.location(), RepoLocation::Ssh { .. })
        {
            // The ancestry read is one round trip; the menu opens when the
            // answer lands instead of freezing the window on the wire.
            match self.drop_tips(&source.name, &target) {
                Some((target_tip, source_tip)) => {
                    let reader = session.reader();
                    cx.spawn(async move |this, cx| {
                        let ready = cx
                            .background_executor()
                            .spawn(async move { reader.is_ancestor(target_tip, source_tip).ok() })
                            .await;
                        let _ = this.update(cx, |view, cx| {
                            // Another drop or a dialog may have started while
                            // the read was in flight.
                            if !view.branches.dialog_open() {
                                view.finish_branch_drop_menu(
                                    source, target, position, head, ready, None, cx,
                                );
                            }
                        });
                    })
                    .detach();
                }
                None => self.finish_branch_drop_menu(
                    source,
                    target,
                    position,
                    head,
                    None,
                    Some(window),
                    cx,
                ),
            }
            return;
        }
        let fast_forward = (source.kind == RefKind::RemoteBranch)
            .then(|| self.fast_forward_ready(&source.name, &target))
            .flatten();
        self.finish_branch_drop_menu(
            source,
            target,
            position,
            head,
            fast_forward,
            Some(window),
            cx,
        );
    }

    /// Installs the drop menu the entry list describes and focuses it.
    ///
    /// `window` is the drop event's own window on the synchronous path. Focus
    /// has to go through it directly: gpui takes the window out of
    /// `cx.windows()` while it dispatches the event, so a focus through the
    /// stored handle would silently miss. Only the asynchronous remote answer,
    /// which runs after dispatch, has no window and uses the handle.
    #[allow(clippy::too_many_arguments)]
    fn finish_branch_drop_menu(
        &mut self,
        source: DraggedBranch,
        target: String,
        position: Point<Pixels>,
        head: Option<String>,
        fast_forward: Option<bool>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
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
            match window {
                Some(window) => window.focus(&focus, cx),
                None => {
                    if let Some(handle) = self.window() {
                        let _ = handle.update(cx, |_, window, cx| window.focus(&focus, cx));
                    }
                }
            }
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
        let (target_tip, source_tip) = self.drop_tips(source, target)?;
        session.reader().is_ancestor(target_tip, source_tip).ok()
    }

    /// The tips a drop's ancestry check compares, or `None` when either side
    /// is missing from the snapshot.
    fn drop_tips(
        &self,
        source: &str,
        target: &str,
    ) -> Option<(lamina_core::Oid, lamina_core::Oid)> {
        let session = self.session.as_ref()?;
        let snapshot = session.snapshot();
        let tip = |kind: RefKind, name: &str| {
            snapshot
                .refs
                .iter()
                .find(|info| info.kind == kind && info.short_name == name)
                .map(|info| info.target)
        };
        Some((
            tip(RefKind::LocalBranch, target)?,
            tip(RefKind::RemoteBranch, source)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    #[test]
    fn stash_rows_show_the_message_without_the_index() {
        assert_eq!(
            strip_stash_prefix("stash@{0}: WIP on diff options"),
            "WIP on diff options"
        );
        assert_eq!(strip_stash_prefix("main"), "main");
        assert_eq!(strip_stash_prefix("stash@{0}"), "stash@{0}");
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

    #[gpui::test]
    fn narrow_sidebar_rows_truncate_and_keep_trailing_markers_clear_of_scrollbar(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::cell::RefCell;

        use gpui::{Bounds, VisualTestContext};
        use gpui_component::Root;
        use lamina_core::forge::{
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

    /// A drop is handled inside the event that delivered it, and gpui takes the
    /// window out of `cx.windows()` for the duration of that dispatch. The menu
    /// therefore has to take focus through the event's own window; a focus
    /// through the stored handle silently misses and the menu stays unfocused.
    #[gpui::test]
    fn a_local_branch_drop_menu_focuses_while_the_window_is_dispatching(
        cx: &mut gpui::TestAppContext,
    ) {
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

        let repo_win = cx
            .update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    cx.new(|cx| RepoView::new(Launch::open_quiet(&dir, None), window, cx))
                })
            })
            .expect("repo window");

        repo_win
            .update(cx, |view, window, cx| {
                let menu_focus = view
                    .branches
                    .dialog_focus
                    .clone()
                    .expect("the view has a dialog focus handle");
                window.blur(cx);
                assert!(window.focused(cx).is_none(), "the test starts unfocused");

                let source = DraggedBranch {
                    kind: RefKind::LocalBranch,
                    name: "topic".to_owned(),
                };
                view.open_branch_drop_menu(
                    source,
                    "main".to_owned(),
                    point(px(12.0), px(12.0)),
                    window,
                    cx,
                );

                assert!(
                    view.branches.drop_menu.is_some(),
                    "a local branch dropped on another opens the menu"
                );
                assert_eq!(
                    window.focused(cx),
                    Some(menu_focus),
                    "the menu takes focus through the window that dispatched the drop"
                );
            })
            .expect("drop");
    }
}
