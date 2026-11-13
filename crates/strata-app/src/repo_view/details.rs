//! The commit details column: summary, author, id and parents, the files
//! summary and the changed-file list.

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Styled, Window, div, px, uniform_list,
};
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit_assets::IconName;
use strata_core::diff::LineKind;
use strata_core::{ChangeKind, FileChange};

use super::RepoView;
use super::common::icon;
use crate::data;
use crate::scrollbar::WithScrollbar as _;
use crate::style::{self, Design};

impl RepoView {
    /// The commit's summary, author, id and parents at the top of the details
    /// column. Empty while nothing is selected.
    pub(super) fn commit_details(
        &self,
        commit: Option<&strata_core::CommitInfo>,
        design: &Design,
    ) -> AnyElement {
        let Some(source) = self.source() else {
            return div().into_any_element();
        };
        let Some(row) = self.selected.and_then(|index| source.row(index)) else {
            return div().into_any_element();
        };
        let now = style::now_epoch();
        let date = style::date_label(row.time, now, design.date_format);
        let initials = style::initials(&row.author);
        let avatar_color = design.avatar_color(&row.email);
        let id = row.id.to_string();
        let copy_id = id.clone();
        let parents = commit
            .map(|commit| {
                commit
                    .parents
                    .iter()
                    .map(|parent| data::short_oid(*parent))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut block = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(design.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(gpui::relative(1.35))
                    .child(row.summary.to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.0))
                    .text_color(design.muted)
                    .child(crate::ui::avatar(initials.clone(), avatar_color, 18.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(format!("{} · {date}", row.author)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .font_family(design.mono.clone())
                    .text_size(px(design.code))
                    .text_color(design.muted)
                    .child(id)
                    .child(
                        div()
                            .id("copy-commit-id")
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(22.0))
                            .h(px(22.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(design.field))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()));
                            })
                            .child(icon(IconName::Copy, 13.0, design.muted)),
                    )
                    .when(!parents.is_empty(), |row| {
                        row.child(div().child(format!("parent {}", parents.join(" · "))))
                    }),
            );

        if self.fake.is_some() {
            block = block.child(
                div()
                    .text_size(px(11.0))
                    .text_color(design.muted)
                    .child("synthetic diff"),
            );
        }
        block.into_any_element()
    }

    /// "N files" and the selected diff's line counts.
    pub(super) fn files_summary(&self, design: &Design) -> AnyElement {
        let files = self.files.as_ref().map_or(0, Vec::len);
        let mut row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3p5()
            .pt_2()
            .pb_1()
            .text_size(px(11.0));
        if files == 0 {
            return row
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(design.muted)
                        .child("0 files"),
                )
                .into_any_element();
        }
        row = row.child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(design.muted)
                .child(format!("{files} file{}", if files == 1 { "" } else { "s" })),
        );
        if let Some((added, removed)) = self.diff_stats() {
            row = row.child(
                div()
                    .flex()
                    .gap_1()
                    .child(div().text_color(design.add_text).child(format!("+{added}")))
                    .child(
                        div()
                            .text_color(design.remove_text)
                            .child(format!("−{removed}")),
                    ),
            );
        }
        row.into_any_element()
    }

    /// Added and removed lines in the diff on screen. The session loads one
    /// file at a time, so this is the selected file's contribution.
    fn diff_stats(&self) -> Option<(u32, u32)> {
        let view = self.diff.view()?;
        let mut added = 0;
        let mut removed = 0;
        for hunk in &view.diff.hunks {
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Added => added += 1,
                    LineKind::Removed => removed += 1,
                    LineKind::Context => {}
                }
            }
        }
        Some((added, removed))
    }

    pub(super) fn file_list(&self, design: &Design) -> AnyElement {
        let Some(files) = &self.files else {
            return div().into_any_element();
        };
        let entity = self.entity.clone();
        let selected = self.selected_file;
        let list_design = design.clone();
        let count = files.len();
        let entries: Vec<(String, ChangeKind)> = files
            .iter()
            .map(|change: &FileChange| (change.path.clone(), change.kind))
            .collect();

        uniform_list("changed-files", count, move |range, _window, _cx| {
            range
                .map(|index| {
                    let entity = entity.clone();
                    let (path, kind) = &entries[index];
                    let (name, dir) = style::path_parts(path);
                    let is_selected = selected == Some(index);
                    let row = crate::ui::list_row(
                        ("changed-file", index),
                        crate::ui::RowState::NORMAL.selected(is_selected),
                        &list_design,
                    )
                    .child(crate::ui::change_badge(*kind, &list_design))
                    .child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
                    .when_some(dir, |row, dir| {
                        row.child(
                            div()
                                .max_w(px(110.0))
                                .flex_none()
                                .truncate()
                                .text_size(px(11.0))
                                .text_color(list_design.muted)
                                .child(dir.to_owned()),
                        )
                    });
                    let click_entity = entity.clone();
                    let menu_entity = entity;
                    let menu_path = path.clone();
                    row.cursor_pointer()
                        .on_click(move |_event, _window, cx| {
                            click_entity.update(cx, |this, cx| this.select_file(index, cx));
                        })
                        .context_menu(move |menu, _window, _cx| {
                            let entity = menu_entity.clone();
                            let path = menu_path.clone();
                            menu.item(PopupMenuItem::new("Show file history").on_click(
                                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                    let path = path.clone();
                                    entity.update(cx, |this, cx| {
                                        this.open_file_history(path, window, cx);
                                    });
                                },
                            ))
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.file_scroll)
        .with_scrollbar(&self.file_scroll)
        .flex_1()
        .min_h_0()
        .into_any_element()
    }
}
