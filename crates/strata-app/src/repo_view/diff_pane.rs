//! The diff half of the history view: the details column beside it, the diff
//! header and the rows or message below it.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Axis, FontWeight, Hsla, IntoElement, ParentElement, Styled, div, px, relative,
};
use gpui_component::Selectable as _;
use gpui_kit_assets::IconName;
use strata_core::session::{DiffView, Operation};

use super::RepoView;
use crate::diff_view::{self, DiffColors, Row as DiffRow};
use crate::style::{self, Design};
use crate::ui::{self, ButtonSize, split_handle};
use crate::worktree_panel::MainMode;

impl RepoView {
    pub(super) fn diff_pane(&self, design: &Design, cx: &mut App) -> AnyElement {
        // Blame replaces the whole pane: it needs the width the details column
        // would take for its annotations.
        if self.blame.is_open() {
            return self.blame_pane(design, cx);
        }

        let rows = if self.fake.is_some() {
            self.selected
                .and_then(|index| self.fake_diffs.get(index % self.fake_diffs.len().max(1)))
                .map_or_else(|| Rc::new(Vec::new()), Rc::clone)
        } else {
            Rc::clone(&self.diff_rows)
        };

        // The column keeps its share of the area as the window changes; the
        // splitter only rewrites the share.
        let mut left = div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .min_h_0()
            .min_w_0()
            .border_r_1()
            .border_color(design.border)
            .bg(design.panel);
        left = match self.details_column_width(cx) {
            Some(width) => left.w(px(width)),
            // Nothing has been laid out yet: the basis is the same share, so
            // the first frame uses the same proportions as the measured ones.
            None => left.flex_basis(relative(super::split::files_fraction(cx))),
        };
        left = if self.mode == MainMode::Reflog {
            // The reflog row is the commit on screen: the history selection is
            // left where it was, so its details must not stand in.
            left.child(self.reflog_details(design))
                .child(self.files_summary(design))
                .child(self.file_list(design))
        } else if self.selection.is_multi() {
            let mut summary = left.child(self.commit_selection_summary(design));
            // Until the range's file list arrives there is nothing to show;
            // the diff side says what it is waiting for.
            if self.files.is_some() {
                summary = summary
                    .child(self.files_summary(design))
                    .child(self.file_list(design));
            }
            summary
        } else {
            let source = self.source();
            let commit = self
                .selected
                .and_then(|index| source.as_ref().and_then(|source| source.commit(index)));
            left.child(self.commit_details(commit, design))
                .child(self.files_summary(design))
                .child(self.file_list(design))
        };

        let drag_entity = self.entity();
        let reset_entity = drag_entity.clone();
        let extent = self.splits.details.clone();
        left = left.child(split_handle(
            "details-split-handle",
            Axis::Horizontal,
            move |event, _window, cx| {
                let Some(area) = extent.get() else {
                    return;
                };
                let fraction = area.fraction_at(event.event.position.x);
                drag_entity.update(cx, |this, cx| {
                    this.set_files_fraction(fraction, area.size(), cx);
                });
            },
            move |_event, _window, cx| {
                reset_entity.update(cx, |this, cx| this.reset_files_fraction(cx));
            },
        ));

        let right =
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .child(if self.tree.is_open() {
                    self.tree_pane(design, cx)
                } else {
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(self.diff_header(design, rows.len(), cx))
                        .child(if self.fake.is_some() {
                            self.diff_rows_list(rows, None, design, cx)
                        } else {
                            self.real_diff_body(design, cx)
                        })
                        .into_any_element()
                });

        let measured_cell = self.splits.details.clone();
        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .min_h_0()
            .min_w_0()
            .bg(design.background)
            .when(!self.details_maximized(cx), |pane| {
                // The strip above and the list's own border already separate
                // the details when they fill the view.
                pane.border_t_1().border_color(design.border)
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .on_children_prepainted(move |bounds: Vec<gpui::Bounds<gpui::Pixels>>, _, _| {
                        if let Some(area) = super::split::measure(&bounds, Axis::Horizontal) {
                            measured_cell.set(Some(area));
                        }
                    })
                    .child(left)
                    .child(right),
            )
            .into_any_element()
    }

    fn diff_header(&self, design: &Design, row_count: usize, cx: &App) -> AnyElement {
        let (path, kind) = match (&self.fake, &self.files, self.selected_file) {
            _ if self.fake.is_some() => (t!("history.synthetic_diff").to_string(), None),
            (_, Some(files), Some(index)) => match files.get(index) {
                Some(change) => (change.path.clone(), Some(change.kind)),
                None => (t!("compare.no_file_selected").to_string(), None),
            },
            _ => (t!("compare.no_file_selected").to_string(), None),
        };
        let (name, dir) = style::path_parts(&path);
        let right = match self.fake {
            Some(_) => t!("diff.header.rows", count = row_count).to_string(),
            None => match self.files.as_ref().map_or(0, Vec::len) {
                0 => String::new(),
                files => t!("diff.header.changed", count = files).to_string(),
            },
        };

        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(40.0))
            .flex_none()
            .px_3p5()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .text_size(px(12.0))
            .child(match kind {
                Some(kind) => crate::ui::change_badge(kind, design),
                None => div().into_any_element(),
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(name.to_owned()),
                    )
                    .when_some(dir, |row, dir| {
                        row.child(
                            div()
                                .ml_2()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.5))
                                .text_color(design.muted)
                                .child(dir.to_owned()),
                        )
                    }),
            )
            .when_some(
                self.diff
                    .view()
                    .and_then(|view| style::encoding_label(view)),
                |header, label| {
                    header.child(ui::status_badge(label, ui::BadgeTone::Neutral, design))
                },
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(design.muted)
                    .child(right),
            )
            .child(
                if self.fake.is_none() && self.diff.view().is_some_and(|view| !view.diff.binary) {
                    let active = self.diff_context.is_whole_file();
                    let entity = self.entity();
                    ui::button("toggle-whole-file", t!("history.whole_file").to_string())
                        .size(ButtonSize::Tiny)
                        .ghost()
                        .selected(active)
                        .tooltip(if active {
                            t!("worktree.show_only_changes").to_string()
                        } else {
                            t!("worktree.show_every_line").to_string()
                        })
                        .on_click(move |_event, _window, cx| {
                            entity.update(cx, |this, cx| this.toggle_whole_file(cx));
                        })
                        .into_any_element()
                } else {
                    div().into_any_element()
                },
            )
            .child(
                self.diff
                    .view()
                    .and_then(|view| {
                        crate::image_diff::svg_toggle(&self.image_diff, view, self.entity())
                    })
                    .unwrap_or_else(|| div().into_any_element()),
            )
            .child({
                let maximized = self.details_maximized(cx);
                let entity = self.entity();
                ui::icon_button(
                    "toggle-details-maximized",
                    if maximized {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    },
                )
                .size(ButtonSize::Tiny)
                .ghost()
                .tooltip(t!("action.toggle_details_maximized").to_string())
                .on_click(move |_event, _window, cx| {
                    entity.update(cx, |this, cx| this.toggle_details_maximized(cx));
                })
            })
            .into_any_element()
    }

    fn real_diff_body(&self, design: &Design, cx: &mut App) -> AnyElement {
        if let Some(error) = &self.files_error {
            return message(error, design.remove_text, design);
        }
        if self.selection.is_multi() {
            if let Some(error) = self.selection_diff.error() {
                return message(error, design.remove_text, design);
            }
            if self.selection_diff.plan().is_none() {
                return message(t!("details.reading_range").as_ref(), design.muted, design);
            }
        }
        let Some(files) = &self.files else {
            let text = if self.selection.is_multi() {
                t!("details.comparing_range").to_string()
            } else {
                t!("details.reading_files").to_string()
            };
            return message(&text, design.muted, design);
        };
        if files.is_empty() {
            let text = if self.selection.is_multi() {
                t!("details.no_range_changes").to_string()
            } else {
                t!("details.no_commit_changes").to_string()
            };
            return message(&text, design.muted, design);
        }
        self.diff_content(design, cx)
    }

    fn diff_content(&self, design: &Design, cx: &mut App) -> AnyElement {
        if let Some(error) = &self.diff_error {
            return message(error, design.remove_text, design);
        }
        let Some(view) = self.diff.view().cloned() else {
            return message(t!("worktree.computing_diff").as_ref(), design.muted, design);
        };
        if let Some(image) =
            crate::image_diff::body(&self.image_diff, &view, design, self.entity(), cx)
        {
            return image;
        }
        if let Some(summary) = view.lfs.clone() {
            return self.lfs_diff(summary, view, design, cx);
        }
        if view.diff.binary {
            return message(t!("worktree.binary_file").as_ref(), design.muted, design);
        }
        if self.diff_rows.is_empty() {
            return message(
                t!("worktree.no_text_changes").as_ref(),
                design.muted,
                design,
            );
        }
        let list = self.diff_rows_list(Rc::clone(&self.diff_rows), Some(view), design, cx);
        match self.diff_context.notice() {
            Some(notice) => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(crate::ui::banner(
                    crate::ui::BannerKind::Warning,
                    notice,
                    design,
                ))
                .child(list)
                .into_any_element(),
            None => list,
        }
    }

    /// The LFS card above the real-content diff, or above a notice that the
    /// object is not local.
    fn lfs_diff(
        &self,
        summary: strata_core::lfs::LfsDiffSummary,
        view: std::sync::Arc<DiffView>,
        design: &Design,
        cx: &mut App,
    ) -> AnyElement {
        let missing = summary.old.as_ref().is_some_and(|side| !side.present)
            || summary.new.as_ref().is_some_and(|side| !side.present);
        let mut card = ui::lfs_card(summary, design);
        if missing && let Some(path) = self.selected_file_path() {
            let entity = self.entity();
            card = card.action(
                ui::button("lfs-download", t!("worktree.download").to_string())
                    .size(ButtonSize::Tiny)
                    .on_click(move |_, window, cx| {
                        let path = path.clone();
                        entity.update(cx, |this, cx| {
                            this.run_operation(
                                Operation::LfsPull { path },
                                t!("worktree.download_lfs").to_string(),
                                window,
                                cx,
                            );
                        });
                    }),
            );
        }
        let body = if !self.diff_rows.is_empty() {
            self.diff_rows_list(Rc::clone(&self.diff_rows), Some(view), design, cx)
        } else if view.diff.binary {
            message(t!("worktree.binary_file").as_ref(), design.muted, design)
        } else if missing {
            message(t!("worktree.not_downloaded").as_ref(), design.muted, design)
        } else {
            message(
                t!("worktree.no_text_changes").as_ref(),
                design.muted,
                design,
            )
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(card)
            .child(body)
            .into_any_element()
    }

    fn selected_file_path(&self) -> Option<String> {
        let files = self.files.as_ref()?;
        let index = self.selected_file?;
        files.get(index).map(|change| change.path.clone())
    }

    fn diff_rows_list(
        &self,
        rows: Rc<Vec<DiffRow>>,
        view: Option<std::sync::Arc<DiffView>>,
        design: &Design,
        cx: &mut App,
    ) -> AnyElement {
        diff_view::diff_list(
            diff_view::DiffList {
                id: "diff",
                rows,
                view,
                colors: DiffColors::from_design(design),
                mono: design.mono.clone(),
                scroll: &self.diff_scroll,
                on_gap: self.gap_handler(),
            },
            cx,
        )
    }
}

fn message(text: &str, color: Hsla, design: &Design) -> AnyElement {
    crate::ui::message(text, color, design).into_any_element()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use strata_core::diff::FileDiff;
    use strata_core::encoding::TextEncoding;
    use strata_core::session::{DiffRequest, DiffView};

    use crate::style::encoding_label;

    fn view(old: TextEncoding, new: TextEncoding) -> DiffView {
        DiffView {
            request: DiffRequest::Staged {
                path: "設定.txt".into(),
            },
            diff: FileDiff::default(),
            old_highlights: Arc::new(Vec::new()),
            new_highlights: Arc::new(Vec::new()),
            lfs: None,
            old_text: Arc::from(""),
            new_text: Arc::from(""),
            old_encoding: old,
            new_encoding: new,
            gaps: Arc::from(Vec::new()),
            image: None,
        }
    }

    #[test]
    fn the_encoding_label_appears_only_for_non_utf8_text() {
        assert_eq!(
            encoding_label(&view(TextEncoding::UTF_8, TextEncoding::UTF_8)),
            None
        );
        let shift_jis = TextEncoding::from_label("Shift_JIS").unwrap();
        assert_eq!(
            encoding_label(&view(TextEncoding::UTF_8, shift_jis)),
            Some("Shift_JIS")
        );
        assert_eq!(
            encoding_label(&view(shift_jis, TextEncoding::UTF_8)),
            Some("Shift_JIS"),
            "a file converted from Shift_JIS still names the old encoding"
        );
    }
}
