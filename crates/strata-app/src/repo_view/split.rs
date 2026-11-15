//! The resizable layout of the History view.
//!
//! The history area stacks the commit list above the details area, and the
//! details area puts the file list beside the diff. Both boundaries are
//! draggable, both remember their share as a fraction rather than a pixel size,
//! and both go back to the default split on a double click.
//!
//! The fractions live in [`LayoutSettings`](crate::settings::LayoutSettings),
//! one value for the whole app, so a new tab or a restart opens the same way.
//! Panes are laid out from the fraction each frame, which is what makes a
//! window resize keep the proportion; the splitters only need the measured
//! extent of the area to turn a pointer position back into a fraction, and
//! [`SplitExtent`](crate::ui::SplitExtent) carries it.
//!
//! The details can take the whole view ("diff focus"). The commit list is then
//! not rendered, but nothing about it is rebuilt: `commit_scroll` and the
//! selection stay as they are, so toggling back lands on the same row at the
//! same offset.

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, Axis, Bounds, Context, Entity, Pixels, div, px, relative};
use gpui_kit_assets::IconName;

use super::RepoView;
use super::commit_list::COMMIT_HEADER_HEIGHT;
use crate::graph_canvas;
use crate::settings::{
    DEFAULT_HISTORY_FILES_FRACTION, DEFAULT_HISTORY_LIST_FRACTION, Settings, update,
};
use crate::style::Design;
use crate::ui::{self, ButtonSize, SplitExtent, split_handle};

/// Three commit rows plus the list's column headings, so the smallest list
/// still reads as a list rather than a single row.
const LIST_MIN_HEIGHT: f32 = COMMIT_HEADER_HEIGHT + graph_canvas::ROW_HEIGHT * 3.0;
/// The details area never gets shorter than its header and a few diff lines.
const DETAILS_MIN_HEIGHT: f32 = 160.0;
/// The file column needs room for a full path; the diff is useless when it is
/// squeezed narrower than a hunk header.
const FILES_MIN_WIDTH: f32 = 200.0;
const DIFF_MIN_WIDTH: f32 = 240.0;

/// Height of the one-row commit strip shown while the details are maximized.
const STRIP_HEIGHT: f32 = 32.0;

/// The measured extents of the two splitters, filled after each layout.
///
/// The cells are shared with the element callbacks rather than copied into
/// them: a drag outlives the frame that created the handle, and the extent it
/// reads must be the newest one, not the one from the frame the drag started.
#[derive(Default)]
pub(crate) struct SplitExtents {
    /// The commit list and details area, measured vertically.
    history: Rc<Cell<Option<SplitExtent>>>,
    /// The file column and diff, measured horizontally.
    pub(super) details: Rc<Cell<Option<SplitExtent>>>,
}

/// The span the first and last child of a split cover on `axis`.
pub(super) fn measure(bounds: &[Bounds<Pixels>], axis: Axis) -> Option<SplitExtent> {
    let leading = bounds.first()?;
    let trailing = bounds.last()?;
    Some(SplitExtent::between(*leading, *trailing, axis))
}

/// The commit list's share of the history area, from the settings.
pub(super) fn commit_list_fraction(cx: &App) -> f32 {
    Settings::get(cx).layout.history_list_fraction
}

/// The file column's share of the details area, from the settings.
pub(super) fn files_fraction(cx: &App) -> f32 {
    Settings::get(cx).layout.history_files_fraction
}

impl RepoView {
    pub(crate) fn details_maximized(&self, cx: &App) -> bool {
        Settings::get(cx).layout.history_details_maximized
    }

    /// The History area: the commit list above the details, unless the details
    /// are maximized.
    pub(super) fn history_split(
        &self,
        entity: Entity<Self>,
        design: &Design,
        width: f32,
        cx: &mut App,
    ) -> AnyElement {
        if self.details_maximized(cx) {
            return self.maximized_history(entity, design, cx);
        }

        let fraction = commit_list_fraction(cx);
        let measured = self.splits.history.get();
        let mut list_pane = div()
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .min_h_0()
            .flex_none()
            .child(self.commit_pane(entity.clone(), design, width));
        list_pane = match measured {
            Some(area) => list_pane.h(px(ui::split_pixels(
                fraction,
                area.size(),
                LIST_MIN_HEIGHT,
                DETAILS_MIN_HEIGHT,
            ))),
            // The first frame has no measurement yet; the basis is the same
            // fraction of the same area, so the pane does not jump.
            None => list_pane.flex_basis(relative(fraction)),
        };

        let drag_entity = entity.clone();
        let extent = self.splits.history.clone();
        let reset_entity = entity.clone();
        list_pane = list_pane.child(split_handle(
            "history-split-handle",
            Axis::Vertical,
            move |event, _window, cx| {
                let Some(area) = extent.get() else {
                    return;
                };
                let fraction = area.fraction_at(event.event.position.y);
                drag_entity.update(cx, |this, cx| {
                    this.set_commit_list_fraction(fraction, area.size(), cx);
                });
            },
            move |_event, _window, cx| {
                reset_entity.update(cx, |this, cx| this.reset_commit_list_fraction(cx));
            },
        ));

        let measured_cell = self.splits.history.clone();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .on_children_prepainted(move |bounds: Vec<Bounds<Pixels>>, _, _| {
                if let Some(area) = measure(&bounds, Axis::Vertical) {
                    measured_cell.set(Some(area));
                }
            })
            .child(list_pane)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.diff_pane(design, cx)),
            )
            .into_any_element()
    }

    /// The details alone, with the selection reduced to a strip above them.
    fn maximized_history(&self, entity: Entity<Self>, design: &Design, cx: &mut App) -> AnyElement {
        let strip = self.commit_strip(entity, design);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(strip)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.diff_pane(design, cx)),
            )
            .into_any_element()
    }

    /// The row that stands in for the commit list while the details fill the
    /// view: the focused commit and the two ways to step off it.
    fn commit_strip(&self, entity: Entity<Self>, design: &Design) -> AnyElement {
        let source = self.source();
        let row = self
            .selected
            .and_then(|index| source.as_ref().and_then(|source| source.row(index)));
        let previous_entity = entity.clone();
        let next_entity = entity;
        let previous = ui::icon_button("history-strip-previous", IconName::ChevronUp)
            .size(ButtonSize::Tiny)
            .ghost()
            .tooltip(t!("split.previous_commit"))
            .on_click(move |_event, _window, cx| {
                previous_entity.update(cx, |this, cx| this.move_selection(-1, cx));
            });
        let next = ui::icon_button("history-strip-next", IconName::ChevronDown)
            .size(ButtonSize::Tiny)
            .ghost()
            .tooltip(t!("split.next_commit"))
            .on_click(move |_event, _window, cx| {
                next_entity.update(cx, |this, cx| this.move_selection(1, cx));
            });

        let label = match &row {
            Some(row) => div()
                .flex()
                .items_center()
                .gap_2()
                .flex_1()
                .min_w_0()
                .child(div().min_w_0().truncate().child(row.summary.to_string()))
                .child(
                    div()
                        .flex_none()
                        .font_family(design.mono.clone())
                        .text_size(px(design.code))
                        .text_color(design.muted)
                        .child(row.id.to_string()),
                )
                .into_any_element(),
            None => div()
                .flex_1()
                .text_size(px(design.small()))
                .text_color(design.muted)
                .child(t!("split.no_commit_selected"))
                .into_any_element(),
        };

        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(STRIP_HEIGHT))
            .flex_none()
            .px_3()
            .bg(design.panel)
            .border_b_1()
            .border_color(design.border)
            .child(previous)
            .child(label)
            .child(next)
            .into_any_element()
    }

    /// Gives the details area the whole view, or restores the saved split.
    pub(crate) fn toggle_details_maximized(&mut self, cx: &mut Context<Self>) {
        let maximized = !self.details_maximized(cx);
        update(cx, |settings| {
            settings.layout.history_details_maximized = maximized;
        });
        cx.notify();
    }

    /// Stores a new commit list share, clamped so neither side collapses.
    fn set_commit_list_fraction(&mut self, fraction: f32, total: f32, cx: &mut Context<Self>) {
        let fraction = ui::clamp_fraction(fraction, total, LIST_MIN_HEIGHT, DETAILS_MIN_HEIGHT);
        if commit_list_fraction(cx) == fraction {
            return;
        }
        update(cx, |settings| {
            settings.layout.history_list_fraction = fraction;
        });
        cx.notify();
    }

    fn reset_commit_list_fraction(&mut self, cx: &mut Context<Self>) {
        if commit_list_fraction(cx) == DEFAULT_HISTORY_LIST_FRACTION {
            return;
        }
        update(cx, |settings| {
            settings.layout.history_list_fraction = DEFAULT_HISTORY_LIST_FRACTION;
        });
        cx.notify();
    }

    /// Stores a new file column share, clamped so the diff keeps its width.
    pub(super) fn set_files_fraction(&mut self, fraction: f32, total: f32, cx: &mut Context<Self>) {
        let fraction = ui::clamp_fraction(fraction, total, FILES_MIN_WIDTH, DIFF_MIN_WIDTH);
        if files_fraction(cx) == fraction {
            return;
        }
        update(cx, |settings| {
            settings.layout.history_files_fraction = fraction;
        });
        cx.notify();
    }

    pub(super) fn reset_files_fraction(&mut self, cx: &mut Context<Self>) {
        if files_fraction(cx) == DEFAULT_HISTORY_FILES_FRACTION {
            return;
        }
        update(cx, |settings| {
            settings.layout.history_files_fraction = DEFAULT_HISTORY_FILES_FRACTION;
        });
        cx.notify();
    }

    /// The file column's width in pixels, once the details area is measured.
    pub(super) fn details_column_width(&self, cx: &App) -> Option<f32> {
        let area = self.splits.details.get()?;
        Some(ui::split_pixels(
            files_fraction(cx),
            area.size(),
            FILES_MIN_WIDTH,
            DIFF_MIN_WIDTH,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{ToggleDetailsMaximized, bind};
    use crate::repo_view::Launch;
    use crate::settings::Settings;
    use gpui::{QuitMode, TestAppContext, VisualTestContext};

    fn prepare(cx: &mut App) {
        cx.set_quit_mode(QuitMode::Explicit);
        gpui_component::init(cx);
        cx.set_global(Settings::default());
        bind(cx);
    }

    #[test]
    fn the_details_split_keeps_both_minimums_at_the_minimum_window() {
        let window = crate::style::MIN_WINDOW_SIZE.0;
        let content = window - crate::repo_view::sidebar::width_for(window);
        assert!(
            content >= FILES_MIN_WIDTH + DIFF_MIN_WIDTH,
            "a {window} px window leaves the details {content} px, less than its two minimum columns"
        );

        let files = ui::split_pixels(
            DEFAULT_HISTORY_FILES_FRACTION,
            content,
            FILES_MIN_WIDTH,
            DIFF_MIN_WIDTH,
        );
        assert!(
            files >= FILES_MIN_WIDTH,
            "the saved fraction must clamp up to the file column's minimum"
        );
        assert!(
            content - files >= DIFF_MIN_WIDTH,
            "the diff keeps its minimum beside the file column"
        );
    }

    #[test]
    fn a_details_area_below_both_minimums_still_fills_only_its_own_width() {
        let area = 320.0;
        let files = ui::split_pixels(0.9, area, FILES_MIN_WIDTH, DIFF_MIN_WIDTH);
        let expected = area * FILES_MIN_WIDTH / (FILES_MIN_WIDTH + DIFF_MIN_WIDTH);
        assert!(
            (files - expected).abs() < 1e-3,
            "the panes divide in the minimums' own proportion rather than overflowing"
        );
        assert!(files > 0.0 && files < area);
    }

    /// Maximizing must not rebuild the list: the same scroll handle and the
    /// same selected commit are what makes the toggle restore the exact view.
    #[gpui::test]
    fn toggling_maximized_keeps_the_selection_and_the_scroll_handle(cx: &mut TestAppContext) {
        cx.update(prepare);
        let window = cx.add_window(|window, cx| RepoView::new(Launch::fake(40), window, cx));
        let view = window.root(cx).expect("the window's root view");
        let scroll = view.read_with(cx, |view, _| view.commit_scroll.clone());
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|_, cx| {
            view.update(cx, |view, cx| view.select(Some(12), cx));
        });
        let selected = view.read_with(&cx, |view, _| (view.selected, view.selection_commit));

        cx.dispatch_action(ToggleDetailsMaximized);
        assert!(
            view.read_with(&cx, |view, cx| view.details_maximized(cx)),
            "the action must maximize the details"
        );
        assert_eq!(
            view.read_with(&cx, |view, _| (view.selected, view.selection_commit)),
            selected,
            "the selection survives the layout change"
        );
        assert!(
            Rc::ptr_eq(
                &scroll.0,
                &view.read_with(&cx, |view, _| view.commit_scroll.clone()).0
            ),
            "the uniform list keeps its scroll state while it is not drawn"
        );

        // A maximized view still draws: the strip and the details replace the
        // list without panicking on the missing rows.
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        cx.dispatch_action(ToggleDetailsMaximized);
        assert!(
            !view.read_with(&cx, |view, cx| view.details_maximized(cx)),
            "the same action restores the split"
        );
    }
}
