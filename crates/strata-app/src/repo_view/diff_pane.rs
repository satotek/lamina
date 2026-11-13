//! The diff half of the history view: the details column beside it, the diff
//! header and the rows or message below it.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, FontWeight, Hsla, IntoElement, ParentElement, Styled, div, px};
use gpui_component::ActiveTheme as _;
use strata_core::session::DiffView;

use super::{DIFF_HEIGHT, RepoView};
use crate::diff_view::{self, DiffColors, Row as DiffRow};
use crate::style::{self, Design};

/// Width of the commit metadata column, matching the design's details pane.
const DETAILS_WIDTH: f32 = 320.0;

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

        let source = self.source();
        let commit = self
            .selected
            .and_then(|index| source.as_ref().and_then(|source| source.commit(index)));

        let left = div()
            .flex()
            .flex_col()
            .w(px(DETAILS_WIDTH))
            .flex_none()
            .min_h_0()
            .border_r_1()
            .border_color(design.border)
            .bg(design.panel)
            .child(self.commit_details(commit, design))
            .child(self.files_summary(design))
            .child(self.file_list(design));

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
                        .child(self.diff_header(design, rows.len()))
                        .child(if self.fake.is_some() {
                            self.diff_rows_list(rows, None, design, cx)
                        } else {
                            self.real_diff_body(design, cx)
                        })
                        .into_any_element()
                });

        div()
            .flex()
            .flex_col()
            .w_full()
            .h(px(DIFF_HEIGHT))
            .flex_none()
            .min_h_0()
            .bg(design.background)
            .border_t_1()
            .border_color(design.border)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(left)
                    .child(right),
            )
            .into_any_element()
    }

    fn diff_header(&self, design: &Design, row_count: usize) -> AnyElement {
        let (path, kind) = match (&self.fake, &self.files, self.selected_file) {
            _ if self.fake.is_some() => ("synthetic diff".to_owned(), None),
            (_, Some(files), Some(index)) => match files.get(index) {
                Some(change) => (change.path.clone(), Some(change.kind)),
                None => ("no file selected".to_owned(), None),
            },
            _ => ("no file selected".to_owned(), None),
        };
        let (name, dir) = style::path_parts(&path);
        let right = match self.fake {
            Some(_) => format!("{row_count} rows"),
            None => match self.files.as_ref().map_or(0, Vec::len) {
                0 => String::new(),
                files => format!("{files} changed"),
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
                    .child(div().font_weight(FontWeight::MEDIUM).child(name.to_owned()))
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
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(design.muted)
                    .child(right),
            )
            .into_any_element()
    }

    fn real_diff_body(&self, design: &Design, cx: &mut App) -> AnyElement {
        if let Some(error) = &self.files_error {
            return message(error, design.remove_text, design);
        }
        let Some(files) = &self.files else {
            return message("reading changed files…", design.muted, design);
        };
        if files.is_empty() {
            return message("No changes in this commit", design.muted, design);
        }
        self.diff_content(design, cx)
    }

    fn diff_content(&self, design: &Design, cx: &mut App) -> AnyElement {
        if let Some(error) = &self.diff_error {
            return message(error, design.remove_text, design);
        }
        let Some(view) = self.diff.view().cloned() else {
            return message("computing diff…", design.muted, design);
        };
        if view.diff.binary {
            return message("Binary file", design.muted, design);
        }
        if self.diff_rows.is_empty() {
            return message("No textual changes", design.muted, design);
        }
        self.diff_rows_list(Rc::clone(&self.diff_rows), Some(view), design, cx)
    }

    fn diff_rows_list(
        &self,
        rows: Rc<Vec<DiffRow>>,
        view: Option<std::sync::Arc<DiffView>>,
        design: &Design,
        cx: &mut App,
    ) -> AnyElement {
        diff_view::diff_list(
            "diff",
            rows,
            view,
            DiffColors::from_theme(cx.theme()),
            design.mono.clone(),
            &self.diff_scroll,
            cx,
        )
    }
}

fn message(text: &str, color: Hsla, design: &Design) -> AnyElement {
    crate::ui::message(text, color, design).into_any_element()
}
