//! The working-tree file lists: the unstaged and staged section headers, their
//! rows and the context menus on those rows.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Entity, IntoElement, MouseButton, ParentElement, Styled, div, px, uniform_list,
};
use gpui_component::menu::PopupMenuItem;
use strata_core::lfs::{LfsInfo, LfsLock};
use strata_core::session::{Operation, RepoSnapshot};
use strata_core::{ChangeKind, FileChange};

use super::ignore;
use super::{Colors, PendingKind, Selection, Side, WorktreePanel};
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
    lfs: Arc<HashMap<String, LfsInfo>>,
    locks: Rc<HashMap<String, LfsLock>>,
    locking_available: bool,
}

impl WorktreePanel {
    #[allow(
        clippy::too_many_arguments,
        reason = "the lists need the snapshot, palette and LFS state together"
    )]
    pub(super) fn lists(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        colors: &Colors,
        lfs: &Arc<HashMap<String, LfsInfo>>,
        locks: &Rc<HashMap<String, LfsLock>>,
        locking_available: bool,
        cx: &Context<RepoView>,
    ) -> AnyElement {
        let selected_conflict = self.selected_conflict();
        div()
            .flex()
            .flex_col()
            .w(colors.design.px(LIST_WIDTH))
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
                lfs,
                locks,
                locking_available,
            ))
            .child(self.file_section(
                entity,
                Side::Staged,
                &snapshot.status.staged,
                colors,
                lfs,
                locks,
                locking_available,
            ))
            .into_any_element()
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "a section carries the row state and the LFS maps it draws"
    )]
    fn file_section(
        &self,
        entity: Entity<RepoView>,
        side: Side,
        files: &[FileChange],
        colors: &Colors,
        lfs: &Arc<HashMap<String, LfsInfo>>,
        locks: &Rc<HashMap<String, LfsLock>>,
        locking_available: bool,
    ) -> AnyElement {
        // Unmerged paths are listed in the conflicts section; repeating them
        // under "unstaged" would offer stage/discard actions that cannot work.
        let files: Vec<&FileChange> = files
            .iter()
            .filter(|change| change.kind != ChangeKind::Conflicted)
            .collect();
        let title = match side {
            Side::Unstaged => t!("worktree.section.unstaged"),
            Side::Staged => t!("worktree.section.staged"),
            Side::Conflict => t!("worktree.section.conflicts"),
        };
        let button = match side {
            Side::Unstaged => {
                let entity = entity.clone();
                let staging = matches!(self.pending, Some((_, PendingKind::Stage)));
                crate::ui::button(
                    gpui::ElementId::named_usize("stage-all", side as usize),
                    t!("worktree.stage_all").to_string(),
                )
                .kind(crate::ui::ButtonKind::Normal)
                // Header actions sit on a 11 px caption row; anything larger
                // overflows the 290 px column and clips "Unstage all".
                .size(crate::ui::ButtonSize::Tiny)
                .disabled(files.is_empty())
                .loading(staging)
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.stage_all_worktree(cx));
                })
                .into_any_element()
            }
            Side::Staged => {
                let entity = entity.clone();
                let unstaging = matches!(self.pending, Some((_, PendingKind::Unstage)));
                crate::ui::button(
                    gpui::ElementId::named_usize("unstage-all", side as usize),
                    t!("worktree.unstage_all").to_string(),
                )
                .kind(crate::ui::ButtonKind::Normal)
                // Header actions sit on a 11 px caption row; anything larger
                // overflows the 290 px column and clips "Unstage all".
                .size(crate::ui::ButtonSize::Tiny)
                .disabled(files.is_empty())
                .loading(unstaging)
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
            .h(colors.design.px(24.0))
            .rounded(px(0.0)),
        );

        if files.is_empty() {
            return section
                .child(
                    div()
                        .px_3()
                        .h(colors.design.px(24.0))
                        .text_size(colors.design.px(11.0))
                        .text_color(colors.muted_foreground)
                        .child(t!("common.none").to_string()),
                )
                .into_any_element();
        }

        let context = RowContext {
            entity,
            rows: Rc::new(files.iter().map(|change| change.path.clone()).collect()),
            selection: self.selection.clone(),
            colors: colors.clone(),
            lfs: Arc::clone(lfs),
            locks: Rc::clone(locks),
            locking_available,
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
        .h(colors.design.px(ROW_HEIGHT * visible_rows as f32))
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
    .when(context.lfs.contains_key(&path), |row| {
        row.child(crate::ui::lfs_badge(&context.colors.design))
    })
    .when_some(dir, |row, dir| {
        row.child(
            div()
                .max_w(context.colors.design.px(120.0))
                .flex_none()
                .truncate()
                .text_size(context.colors.design.px(11.0))
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
    let menu_lfs_tracked = context.lfs.contains_key(&path);
    let menu_locks = Rc::clone(&context.locks);
    let locking_available = context.locking_available;
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
        move |menu, window, cx| {
            let count = menu_entity.read(cx).worktree.selection.len();
            let ssh = menu_entity.read(cx).is_ssh();
            let remote_reason = ssh.then(crate::remote_open::unavailable_reason);
            // The menu acts on the pointed-at row even when the selection
            // holds several files.
            let editor_entity = menu_entity.clone();
            let editor_path = menu_path.clone();
            let mut menu = menu.item(
                PopupMenuItem::new(crate::ui::menu_label(
                    t!("history.open_in_editor").to_string(),
                    remote_reason.as_ref(),
                ))
                .disabled(ssh)
                .on_click(move |_, _window, cx| {
                    let path = editor_path.clone();
                    editor_entity.update(cx, |this, cx| this.open_file_in_editor(&path, None, cx));
                }),
            );

            if !menu_lfs_tracked {
                let track_entity = menu_entity.clone();
                let track_path = menu_path.clone();
                menu = menu.item(PopupMenuItem::new(t!("lfs.track").to_string()).on_click(
                    move |_, window, cx| {
                        let path = track_path.clone();
                        track_entity.update(cx, |this, cx| {
                            this.run_operation(
                                Operation::LfsTrack { path },
                                t!("lfs.track").to_string(),
                                window,
                                cx,
                            );
                        });
                    },
                ));
            }
            if locking_available {
                match menu_locks.get(&menu_path) {
                    Some(lock) => {
                        let unlock_entity = menu_entity.clone();
                        let unlock_path = menu_path.clone();
                        let label = match lock.owner.as_deref() {
                            Some(owner) => t!("lfs.unlock_owner", owner = owner).to_string(),
                            None => t!("lfs.unlock").to_string(),
                        };
                        menu =
                            menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                let path = unlock_path.clone();
                                unlock_entity.update(cx, |this, cx| {
                                    let label = t!("lfs.op_unlock", path = path).to_string();
                                    this.run_operation(
                                        Operation::LfsUnlock { path },
                                        label,
                                        window,
                                        cx,
                                    );
                                });
                            }));
                    }
                    None => {
                        let lock_entity = menu_entity.clone();
                        let lock_path = menu_path.clone();
                        menu = menu.item(PopupMenuItem::new(t!("lfs.lock").to_string()).on_click(
                            move |_, window, cx| {
                                let path = lock_path.clone();
                                lock_entity.update(cx, |this, cx| {
                                    let label = t!("lfs.op_lock", path = path).to_string();
                                    this.run_operation(
                                        Operation::LfsLock { path },
                                        label,
                                        window,
                                        cx,
                                    );
                                });
                            },
                        ));
                    }
                }
            }

            match side {
                Side::Unstaged => {
                    let stage_entity = menu_entity.clone();
                    let label = if count > 1 {
                        t!(
                            &crate::i18n::count_key("worktree.stage_count", count),
                            count = count
                        )
                        .to_string()
                    } else {
                        t!("common.stage").to_string()
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _window, cx| {
                        stage_entity.update(cx, |this, cx| this.stage_selected(cx));
                    }));

                    let discard_entity = menu_entity.clone();
                    let label = match count {
                        1 if untracked => t!("worktree.delete_file").to_string(),
                        1 => t!("worktree.discard_changes").to_string(),
                        _ => t!(
                            &crate::i18n::count_key("worktree.discard_count", count),
                            count = count
                        )
                        .to_string(),
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        discard_entity
                            .update(cx, |this, cx| this.discard_unstaged_selection(window, cx));
                    }));

                    // Ignoring writes a plain `.gitignore` line, so it is only
                    // offered for a path git does not track yet.
                    if untracked {
                        let patterns = ignore::patterns(&menu_path);
                        let ignore_entity = menu_entity.clone();
                        menu = menu.submenu(
                            t!("worktree.ignore").to_string(),
                            window,
                            cx,
                            move |submenu, _window, _cx| {
                                let mut submenu = submenu;
                                for entry in &patterns {
                                    let entity = ignore_entity.clone();
                                    let pattern = entry.pattern.clone();
                                    submenu = submenu.item(
                                        PopupMenuItem::new(entry.label.clone()).on_click(
                                            move |_, _window, cx| {
                                                let pattern = pattern.clone();
                                                entity.update(cx, |this, cx| {
                                                    this.ignore_worktree_path(pattern, cx)
                                                });
                                            },
                                        ),
                                    );
                                }
                                submenu
                            },
                        );

                        let edit_entity = menu_entity.clone();
                        menu = menu.item(
                            PopupMenuItem::new(t!("worktree.ignore.edit").to_string()).on_click(
                                move |_, _window, cx| {
                                    edit_entity.update(cx, |this, cx| this.edit_gitignore(cx));
                                },
                            ),
                        );
                    }
                }
                Side::Staged => {
                    let unstage_entity = menu_entity.clone();
                    let label = if count > 1 {
                        t!(
                            &crate::i18n::count_key("worktree.unstage_count", count),
                            count = count
                        )
                        .to_string()
                    } else {
                        t!("common.unstage").to_string()
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _window, cx| {
                        unstage_entity.update(cx, |this, cx| this.unstage_selected(cx));
                    }));

                    // Staged discard drops both the index and the worktree at
                    // these paths, so it is offered separately from unstage.
                    let discard_entity = menu_entity.clone();
                    menu = menu.item(
                        PopupMenuItem::new(t!("worktree.discard_changes").to_string()).on_click(
                            move |_, window, cx| {
                                discard_entity.update(cx, |this, cx| {
                                    this.discard_staged_selection(window, cx)
                                });
                            },
                        ),
                    );
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
