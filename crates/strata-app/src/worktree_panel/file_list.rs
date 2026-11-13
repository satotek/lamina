//! The working-tree file lists: the unstaged and staged section headers, their
//! rows and the context menus on those rows.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Entity, IntoElement, MouseButton, ParentElement, Styled, div, px, uniform_list,
};
use gpui_component::menu::PopupMenuItem;
use strata_core::session::RepoSnapshot;
use strata_core::{ChangeKind, FileChange};

use super::{Colors, Selection, Side, WorktreePanel};
use crate::repo_view::RepoView;
use crate::scrollbar::WithScrollbar as _;
use crate::style;

const LIST_WIDTH: f32 = 300.0;
/// Height of one file row, matching the commit list and the design.
const ROW_HEIGHT: f32 = 28.0;
/// A list stops growing here and scrolls instead.
const MAX_LIST_ROWS: usize = 8;

/// What every row of one section needs: the same selection, row order and
/// colors, cloned once per render instead of once per row.
struct RowContext {
    entity: Entity<RepoView>,
    /// The section's paths in row order, for shift-ranges and arrow moves.
    rows: Rc<Vec<String>>,
    selection: Selection,
    colors: Colors,
}

impl WorktreePanel {
    pub(super) fn lists(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        colors: &Colors,
        cx: &Context<RepoView>,
    ) -> AnyElement {
        let selected_conflict = self.selected_conflict();
        div()
            .flex()
            .flex_col()
            .w(px(LIST_WIDTH))
            .flex_none()
            .min_h_0()
            .border_r_1()
            .border_color(colors.border)
            .child(self.conflicts.section(
                entity.clone(),
                &snapshot.conflicts,
                selected_conflict,
                cx,
            ))
            .child(self.file_section(
                entity.clone(),
                Side::Unstaged,
                &snapshot.status.unstaged,
                colors,
            ))
            .child(self.file_section(entity, Side::Staged, &snapshot.status.staged, colors))
            .into_any_element()
    }

    fn file_section(
        &self,
        entity: Entity<RepoView>,
        side: Side,
        files: &[FileChange],
        colors: &Colors,
    ) -> AnyElement {
        // Unmerged paths are listed in the conflicts section; repeating them
        // under "unstaged" would offer stage/discard actions that cannot work.
        let files: Vec<&FileChange> = files
            .iter()
            .filter(|change| change.kind != ChangeKind::Conflicted)
            .collect();
        let title = match side {
            Side::Unstaged => "UNSTAGED",
            Side::Staged => "STAGED",
            Side::Conflict => "CONFLICTS",
        };
        let button = match side {
            Side::Unstaged => {
                let entity = entity.clone();
                crate::ui::button(
                    gpui::ElementId::named_usize("stage-all", side as usize),
                    "Stage all",
                )
                .kind(crate::ui::ButtonKind::Normal)
                // Header actions sit on a 11 px caption row; anything larger
                // overflows the 290 px column and clips "Unstage all".
                .size(crate::ui::ButtonSize::Tiny)
                .disabled(files.is_empty())
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.stage_all_worktree(cx));
                })
                .into_any_element()
            }
            Side::Staged => {
                let entity = entity.clone();
                crate::ui::button(
                    gpui::ElementId::named_usize("unstage-all", side as usize),
                    "Unstage all",
                )
                .kind(crate::ui::ButtonKind::Normal)
                // Header actions sit on a 11 px caption row; anything larger
                // overflows the 290 px column and clips "Unstage all".
                .size(crate::ui::ButtonSize::Tiny)
                .disabled(files.is_empty())
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.unstage_all_worktree(cx));
                })
                .into_any_element()
            }
            // The conflicts section has its own renderer.
            Side::Conflict => div().into_any_element(),
        };

        let mut section = div().flex().flex_col().flex_none().min_h_0().child(
            crate::ui::section_header(
                gpui::ElementId::named_usize("file-section", side as usize),
                title,
                &colors.design,
            )
            .count(files.len())
            .trailing(button)
            .px_3()
            .pt_3()
            .pb_1p5()
            .h(px(24.0))
            .rounded(px(0.0)),
        );

        if files.is_empty() {
            return section
                .child(
                    div()
                        .px_3()
                        .h(px(24.0))
                        .text_size(px(11.0))
                        .text_color(colors.muted_foreground)
                        .child("none"),
                )
                .into_any_element();
        }

        let context = RowContext {
            entity,
            rows: Rc::new(files.iter().map(|change| change.path.clone()).collect()),
            selection: self.selection.clone(),
            colors: colors.clone(),
        };
        let entries: Rc<Vec<(String, ChangeKind)>> = Rc::new(
            files
                .iter()
                .map(|change| (change.path.clone(), change.kind))
                .collect(),
        );
        let list_id = match side {
            Side::Unstaged => "unstaged-files",
            Side::Staged => "staged-files",
            Side::Conflict => "conflict-files",
        };
        let scroll = match side {
            Side::Unstaged => self.unstaged_scroll.clone(),
            Side::Staged => self.staged_scroll.clone(),
            Side::Conflict => self.unstaged_scroll.clone(),
        };

        // Lists stack from the top and only scroll once they get long; giving
        // each an equal share of the column left the staged header floating in
        // the middle of an empty pane.
        let visible_rows = entries.len().min(MAX_LIST_ROWS);
        let list = uniform_list(list_id, entries.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    let (path, kind) = entries[index].clone();
                    file_row(&context, side, index, path, kind)
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&scroll)
        .with_scrollbar(&scroll)
        .h(px(ROW_HEIGHT * visible_rows as f32))
        .flex_none();

        section = section.child(list);
        section.into_any_element()
    }
}

fn file_row(
    context: &RowContext,
    side: Side,
    index: usize,
    path: String,
    kind: ChangeKind,
) -> AnyElement {
    let is_selected = context.selection.contains(side, &path);
    let (name, dir) = style::path_parts(&path);

    let row = crate::ui::list_row(
        gpui::ElementId::named_usize(format!("worktree-file-{}", side as u8), index),
        crate::ui::RowState::NORMAL.selected(is_selected),
        &context.colors.design,
    )
    .child(crate::ui::change_badge(kind, &context.colors.design))
    .child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
    .when_some(dir, |row, dir| {
        row.child(
            div()
                .max_w(px(120.0))
                .flex_none()
                .truncate()
                .text_size(px(11.0))
                .text_color(context.colors.muted_foreground)
                .child(dir.to_owned()),
        )
    });

    let click_entity = context.entity.clone();
    let click_path = path.clone();
    let rows = context.rows.clone();
    let untracked = kind == ChangeKind::Untracked;
    let menu_entity = context.entity.clone();
    let menu_path = path.clone();
    let right_entity = context.entity.clone();
    let right_path = click_path.clone();

    let trigger = row
        .cursor_pointer()
        .on_click(move |event, window, cx| {
            let modifiers = event.modifiers();
            let shift = modifiers.shift;
            let secondary = modifiers.secondary(cx);
            let path = click_path.clone();
            let rows = rows.clone();
            click_entity.update(cx, |this, cx| {
                this.click_worktree_file(side, path, shift, secondary, rows.as_slice(), window, cx);
                if event.click_count() >= 2 {
                    this.toggle_worktree_selection(cx);
                }
            });
        })
        // Right-clicking a row outside the selection selects it before the
        // menu opens, so every menu entry acts on what the user pointed at.
        .on_mouse_down(MouseButton::Right, move |_, _, cx| {
            let path = right_path.clone();
            right_entity.update(cx, |this, cx| {
                this.ensure_worktree_selection(side, path, cx)
            });
        });
    crate::ui::context_menu(
        gpui::ElementId::named_usize(format!("worktree-file-menu-{}", side as u8), index),
        &context.colors.design,
        trigger,
        move |menu, _window, cx| {
            let count = menu_entity.read(cx).worktree.selection.len();
            // The menu acts on the pointed-at row even when the selection
            // holds several files.
            let editor_entity = menu_entity.clone();
            let editor_path = menu_path.clone();
            let mut menu = menu.item(PopupMenuItem::new("Open in Editor").on_click(
                move |_, _window, cx| {
                    let path = editor_path.clone();
                    editor_entity.update(cx, |this, cx| this.open_file_in_editor(&path, None, cx));
                },
            ));
            match side {
                Side::Unstaged => {
                    let stage_entity = menu_entity.clone();
                    let label = if count > 1 {
                        format!("Stage {count} files")
                    } else {
                        "Stage".to_owned()
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _window, cx| {
                        stage_entity.update(cx, |this, cx| this.stage_selected(cx));
                    }));

                    let discard_entity = menu_entity.clone();
                    let label = match count {
                        1 if untracked => "Delete file…".to_owned(),
                        1 => "Discard changes…".to_owned(),
                        _ => format!("Discard {count} files…"),
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        discard_entity
                            .update(cx, |this, cx| this.discard_unstaged_selection(window, cx));
                    }));
                }
                Side::Staged => {
                    let unstage_entity = menu_entity.clone();
                    let label = if count > 1 {
                        format!("Unstage {count} files")
                    } else {
                        "Unstage".to_owned()
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _window, cx| {
                        unstage_entity.update(cx, |this, cx| this.unstage_selected(cx));
                    }));

                    // Staged discard drops both the index and the worktree at
                    // these paths, so it is offered separately from unstage.
                    let discard_entity = menu_entity.clone();
                    menu = menu.item(PopupMenuItem::new("Discard changes…").on_click(
                        move |_, window, cx| {
                            discard_entity
                                .update(cx, |this, cx| this.discard_staged_selection(window, cx));
                        },
                    ));
                }
                // The conflict section has no context menu; its actions live in
                // the conflict view.
                Side::Conflict => {}
            }
            menu
        },
    )
    .into_any_element()
}
