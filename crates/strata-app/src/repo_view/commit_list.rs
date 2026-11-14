//! The commit list: the column headings, the rows with their graph column and
//! badges, and the search filter bar above them.
//!
//! The list is built from [`Source`](crate::data::Source), so the synthetic
//! benchmark and a real repository share the same row renderer.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Div, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Styled, div, px, uniform_list,
};
use strata_core::{Oid, RefKind};

use super::history_ops::{self, CommitTarget};
use super::{RepoView, one_line};
use crate::data::{Badge, RowData};
use crate::graph_canvas;
use crate::scrollbar::WithScrollbar as _;
use crate::search::{Field, SEARCH_BAR};
use crate::style::{self, Design};

const AUTHOR_WIDTH: f32 = 150.0;
const DATE_WIDTH: f32 = 96.0;
const COMMIT_WIDTH: f32 = 64.0;

/// A row's own left and right padding.
const ROW_PADDING: f32 = 24.0;
/// The gap between two of a row's cells.
const CELL_GAP: f32 = 12.0;
/// The narrowest the summary column gets before a fixed column drops instead.
/// A summary narrower than this cannot show the start of a commit message.
const DESCRIPTION_MIN_WIDTH: f32 = 110.0;

/// Height of the column headings above the commit list. The splitter uses it
/// to keep three rows visible under them.
pub(super) const COMMIT_HEADER_HEIGHT: f32 = 28.0;

/// Which of the commit list's fixed columns fit in the width it was given.
///
/// The summary column is the flexible one, so when a narrow window cannot hold
/// every column the list gives up the date first and then the author, rather
/// than squeezing the summary to nothing or letting the row run past the edge.
/// The decision is made once per frame for the whole list, so rows do not
/// change shape from one row to the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CommitColumns {
    author: bool,
    date: bool,
}

impl CommitColumns {
    /// The widest column set that leaves the summary its minimum, given the
    /// widest graph column in the list.
    pub(super) fn for_width(width: f32, graph_width: f32) -> Self {
        [
            Self {
                author: true,
                date: true,
            },
            Self {
                author: true,
                date: false,
            },
            Self {
                author: false,
                date: false,
            },
        ]
        .into_iter()
        .find(|columns| columns.fits(width, graph_width))
        .unwrap_or(Self {
            author: false,
            date: false,
        })
    }

    /// The cells a row has: the graph, the summary, the optional columns and
    /// the short commit id.
    fn cells(self) -> f32 {
        3.0 + f32::from(self.author) + f32::from(self.date)
    }

    /// What the row reserves before the summary gets a share.
    fn fixed_width(self, graph_width: f32) -> f32 {
        ROW_PADDING
            + graph_width
            + COMMIT_WIDTH
            + if self.author { AUTHOR_WIDTH } else { 0.0 }
            + if self.date { DATE_WIDTH } else { 0.0 }
            + CELL_GAP * (self.cells() - 1.0)
    }

    fn fits(self, width: f32, graph_width: f32) -> bool {
        self.fixed_width(graph_width) + DESCRIPTION_MIN_WIDTH <= width
    }
}

impl RepoView {
    /// Commit of the ref the sidebar last revealed. The field lives on
    /// [`RepoView`] because `reveal_ref` and `reveal_stash` set it there.
    pub(super) fn revealed_commit(&self) -> Option<Oid> {
        self.revealed_commit
    }

    pub(super) fn commit_pane(
        &self,
        entity: gpui::Entity<Self>,
        design: &Design,
        width: f32,
    ) -> AnyElement {
        let Some(source) = self.source() else {
            return empty_pane("No repository", design);
        };
        let len = source.len();
        if len == 0 {
            return empty_pane("No commits yet", design);
        }

        // The decision uses the widest graph in the list, not the row's own:
        // columns that came and went between rows would read as noise.
        let columns = CommitColumns::for_width(width, graph_canvas::indent(self.lanes.widest()));
        let now = style::now_epoch();
        let selection = self.selection.clone();
        let revealed = self.revealed_commit();
        let row_design = design.clone();
        let focus = self.focus.clone();
        // The menu itself is built in `history_ops`; its rules only need the
        // snapshot, taken once per frame rather than per row.
        let menu_state = self.history_menu_state();
        let filter = self.search.filter().cloned();
        let count = filter.as_ref().map_or(len, |rows| rows.len());
        // The rows as displayed, which is what a shift range and arrow moves
        // walk; a filtered list shows a subset of the history indices.
        let order: Rc<Vec<usize>> = Rc::new(match &filter {
            Some(rows) => rows.as_ref().clone(),
            None => (0..len).collect(),
        });
        let date_format = design.date_format;
        let show_remote = design.show_remote_branches;
        let show_tags = design.show_tags;
        let list_entity = entity.clone();
        let source_for_list = source.clone();
        let list = uniform_list("commits", count, move |range, _window, _cx| {
            range
                .map(|position| {
                    let entity = list_entity.clone();
                    let focus = focus.clone();
                    // While a search filters the list, `position` is a row of
                    // the result set and `index` the commit it points at.
                    let index = filter
                        .as_ref()
                        .and_then(|rows| rows.get(position).copied())
                        .unwrap_or(position);
                    let Some(mut row) = source_for_list.row(index) else {
                        return div().into_any_element();
                    };
                    // The menu offers a delete for each tag on the commit, so
                    // collect them before the badge filters hide any.
                    let target = CommitTarget {
                        id: source_for_list.commit_id(index),
                        short: row.id.to_string(),
                        summary: row.summary.to_string(),
                        tags: row
                            .badges
                            .iter()
                            .filter(|badge| badge.kind == RefKind::Tag)
                            .map(|badge| badge.label.to_string())
                            .collect(),
                    };
                    if !show_remote {
                        row.badges
                            .retain(|badge| badge.kind != RefKind::RemoteBranch);
                    }
                    if !show_tags {
                        row.badges.retain(|badge| badge.kind != RefKind::Tag);
                    }
                    let date = style::date_label(row.time, now, date_format);
                    let filtered = filter.is_some();
                    let click_entity = entity.clone();
                    let click_order = order.clone();
                    let right_entity = entity.clone();
                    let id = ("commit-row", index);
                    // A ref revealed from the sidebar keeps its row tinted
                    // while it is not the selection.
                    let revealed = revealed.is_some_and(|commit| Some(commit) == target.id);
                    let selected = selection.contains(index);
                    let element = if filtered {
                        // The graph is hidden while filtered: spaced-out rows
                        // would draw lanes that never existed between them.
                        filtered_commit_row(
                            id,
                            &row,
                            selected,
                            revealed,
                            &date,
                            &row_design,
                            columns,
                        )
                    } else {
                        commit_row(id, &row, selected, revealed, &date, &row_design, columns)
                    }
                    .cursor_pointer()
                    .on_click(move |event, window, cx| {
                        let modifiers = event.modifiers();
                        let shift = modifiers.shift;
                        let secondary = modifiers.secondary(cx);
                        let order = click_order.clone();
                        click_entity.update(cx, |this, cx| {
                            this.click_commit(index, position, shift, secondary, &order, cx);
                            window.focus(&focus, cx);
                        });
                    })
                    // Right-clicking outside the selection selects that row,
                    // so the menu never acts on rows the user did not point at.
                    .on_mouse_down(MouseButton::Right, move |_, _, cx| {
                        right_entity.update(cx, |this, cx| this.ensure_commit_selection(index, cx));
                    });
                    let menu_entity = entity;
                    let menu_state = menu_state.clone();
                    crate::ui::context_menu(
                        ("commit-row-menu", index),
                        &row_design,
                        element,
                        move |menu, window, cx| {
                            let multi = menu_entity.read(cx).multi_pick_context();
                            history_ops::commit_menu(
                                menu,
                                target.clone(),
                                menu_state.clone(),
                                multi,
                                menu_entity.clone(),
                                window,
                                cx,
                            )
                        },
                    )
                    .w_full()
                    .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.commit_scroll)
        .with_scrollbar(&self.commit_scroll)
        .flex_1();

        let body: AnyElement = if count == 0 {
            empty_pane("No commits match the search", design)
        } else {
            list.into_any_element()
        };

        let mut column = div().flex().flex_col().flex_1().min_h_0().min_w_0();
        column = column.child(commit_header(design, columns));
        if self.search.is_active() {
            column = column.child(self.search_bar(entity, design));
        }
        column = column.child(body);
        if !self.source().is_some_and(|source| source.complete()) {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .h(px(20.0))
                    .flex_none()
                    .px_3()
                    .text_size(px(10.0))
                    .text_color(design.muted)
                    .child(format!("loading {len} commits")),
            );
        }
        column.into_any_element()
    }

    /// Match toggles and the match count, shown only while a search filters
    /// the list. The text field itself is in the title bar.
    fn search_bar(&self, entity: gpui::Entity<Self>, design: &Design) -> AnyElement {
        let bar = div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(30.0))
            .flex_none()
            .px_3()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .key_context(SEARCH_BAR)
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(design.muted)
                    .child("Filters"),
            )
            .child(search_chip(
                "summary",
                Field::Summary,
                self.search.fields(),
                entity.clone(),
            ))
            .child(search_chip(
                "author",
                Field::Author,
                self.search.fields(),
                entity.clone(),
            ))
            .child(search_chip(
                "hash",
                Field::Hash,
                self.search.fields(),
                entity,
            ))
            .child(div().flex_1());

        let label = match self.search.error() {
            Some(error) => one_line(error),
            None => self.search.count_label(self.commit_count()),
        };
        bar.child(
            div()
                .flex_none()
                .max_w(px(220.0))
                .truncate()
                .text_size(px(10.5))
                .text_color(if self.search.error().is_some() {
                    design.remove_text
                } else {
                    design.muted
                })
                .child(label),
        )
        .into_any_element()
    }
}

/// The column headings above the commit list.
///
/// The graph has no heading of its own: every row indents its text by its own
/// graph width, so there is no single left edge a "Graph" label could mark.
fn commit_header(design: &Design, columns: CommitColumns) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .h(px(COMMIT_HEADER_HEIGHT))
        .flex_none()
        .pl(px(20.0))
        .pr(px(13.0))
        .border_b_1()
        .border_color(design.border)
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(design.muted)
        .child(div().flex_1().min_w_0().child("Description"))
        .when(columns.author, |header| {
            header.child(div().w(px(AUTHOR_WIDTH)).flex_none().child("Author"))
        })
        .when(columns.date, |header| {
            header.child(div().w(px(DATE_WIDTH)).flex_none().child("Date"))
        })
        .child(div().w(px(COMMIT_WIDTH)).flex_none().child("Commit"))
        .into_any_element()
}

/// The row state both commit-row shapes paint with. `ui::list_row` gives the
/// selection precedence over the revealed tint, so a revealed row reads as
/// selected while it is also the selection.
fn row_state(selected: bool, revealed: bool) -> crate::ui::RowState {
    crate::ui::RowState::NORMAL
        .selected(selected)
        .revealed(revealed)
}

fn commit_row(
    id: impl Into<gpui::ElementId>,
    data: &RowData<'_>,
    selected: bool,
    revealed: bool,
    date: &str,
    design: &Design,
    columns: CommitColumns,
) -> gpui::Stateful<Div> {
    let avatar_color = design.avatar_color(&data.email);
    let initials = style::initials(&data.author);
    let state = row_state(selected, revealed);
    // The graph fades into the row's own fill, so it has to be the same colour
    // `ui::list_row` gives the row.
    let fade = crate::ui::row_surface(state, design);
    crate::ui::list_row(id, state, design)
        .gap_3()
        .h(px(graph_canvas::ROW_HEIGHT))
        .w_full()
        .mx(px(0.0))
        .px_3()
        .child(graph_canvas::graph_column(
            data.graph,
            data.shallow_boundary,
            design.lane,
            fade,
            design.muted,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(6.0))
                .overflow_hidden()
                .when(!data.badges.is_empty(), |cell| {
                    let mut badges = div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .flex_none()
                        .overflow_hidden();
                    for badge in &data.badges {
                        badges = badges.child(badge_chip(badge, design));
                    }
                    cell.child(badges)
                })
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(design.text)
                        .when(data.badges.iter().any(|badge| badge.head), |text| {
                            text.font_weight(FontWeight::SEMIBOLD)
                        })
                        .child(data.summary.to_string()),
                )
                .when(data.shallow_boundary, |cell| {
                    cell.child(
                        div()
                            .flex_none()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child("⋯ history not fetched"),
                    )
                }),
        )
        .when(columns.author, |row| {
            row.child(
                div()
                    .w(px(AUTHOR_WIDTH))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .overflow_hidden()
                    .child(crate::ui::avatar(initials.clone(), avatar_color, 18.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(design.small()))
                            .text_color(design.muted)
                            .child(data.author.to_string()),
                    ),
            )
        })
        .when(columns.date, |row| {
            row.child(
                div()
                    .w(px(DATE_WIDTH))
                    .flex_none()
                    .truncate()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(date.to_owned()),
            )
        })
        .child(
            div()
                .w(px(COMMIT_WIDTH))
                .flex_none()
                .font_family(design.mono.clone())
                .text_size(px(design.code))
                .text_color(design.muted)
                .child(data.id.to_string()),
        )
}

/// A commit row in the filtered result set. A small fixed indent stands in for
/// the graph, which is hidden: the matching commits are not adjacent in
/// history, so lanes between them would be fiction.
fn filtered_commit_row(
    id: impl Into<gpui::ElementId>,
    data: &RowData<'_>,
    selected: bool,
    revealed: bool,
    date: &str,
    design: &Design,
    columns: CommitColumns,
) -> gpui::Stateful<Div> {
    let avatar_color = design.avatar_color(&data.email);
    let initials = style::initials(&data.author);
    crate::ui::list_row(id, row_state(selected, revealed), design)
        .gap_3()
        .h(px(graph_canvas::ROW_HEIGHT))
        .w_full()
        .mx(px(0.0))
        .px_3()
        .child(div().w(px(graph_canvas::search_indent())).flex_none())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(6.0))
                .overflow_hidden()
                .when(!data.badges.is_empty(), |cell| {
                    let mut badges = div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .flex_none()
                        .overflow_hidden();
                    for badge in &data.badges {
                        badges = badges.child(badge_chip(badge, design));
                    }
                    cell.child(badges)
                })
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(design.text)
                        .child(data.summary.to_string()),
                )
                .when(data.shallow_boundary, |cell| {
                    cell.child(
                        div()
                            .flex_none()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child("⋯ history not fetched"),
                    )
                }),
        )
        .when(columns.author, |row| {
            row.child(
                div()
                    .w(px(AUTHOR_WIDTH))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .overflow_hidden()
                    .child(crate::ui::avatar(initials.clone(), avatar_color, 18.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(design.small()))
                            .text_color(design.muted)
                            .child(data.author.to_string()),
                    ),
            )
        })
        .when(columns.date, |row| {
            row.child(
                div()
                    .w(px(DATE_WIDTH))
                    .flex_none()
                    .truncate()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(date.to_owned()),
            )
        })
        .child(
            div()
                .w(px(COMMIT_WIDTH))
                .flex_none()
                .font_family(design.mono.clone())
                .text_size(px(design.code))
                .text_color(design.muted)
                .child(data.id.to_string()),
        )
}

/// One of the three match toggles above the commit list.
fn search_chip(
    label: &'static str,
    field: Field,
    fields: crate::search::Fields,
    entity: gpui::Entity<RepoView>,
) -> AnyElement {
    let on = fields.enabled(field);
    crate::ui::button(format!("search-chip-{label}"), label)
        .size(crate::ui::ButtonSize::Tiny)
        .kind(if on {
            crate::ui::ButtonKind::Primary
        } else {
            crate::ui::ButtonKind::Ghost
        })
        .tooltip(format!("Match {label}"))
        .on_click(move |_event, _window, cx| {
            entity.update(cx, |this, cx| this.toggle_search_field(field, cx));
        })
        .into_any_element()
}

fn badge_chip(badge: &Badge<'_>, design: &Design) -> AnyElement {
    crate::ui::ref_badge(badge.kind, badge.head, badge.label.to_string(), design)
}

fn empty_pane(text: &str, design: &Design) -> AnyElement {
    crate::ui::empty_state(text, design).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_date_goes_before_the_author() {
        let graph = graph_canvas::indent(1);

        let wide = CommitColumns::for_width(900.0, graph);
        assert!(wide.author && wide.date, "a wide list keeps both columns");

        let narrow = CommitColumns::for_width(500.0, graph);
        assert!(
            narrow.author && !narrow.date,
            "a narrow list drops the date first"
        );

        let narrowest = CommitColumns::for_width(300.0, graph);
        assert!(
            !narrowest.author && !narrowest.date,
            "the narrowest list keeps only the summary and the id"
        );
    }

    #[test]
    fn the_widest_column_set_that_leaves_the_summary_readable_wins() {
        // A width no real window gives the list (the main area is at least
        // 440 px at the app minimum) through to a maximised one, with a graph
        // as narrow as one lane and as wide as the cap.
        for width in (400..=1600).map(|width| width as f32) {
            for lanes in [1, 4, graph_canvas::MAX_LANES] {
                let graph = graph_canvas::indent(lanes);
                let columns = CommitColumns::for_width(width, graph);
                assert!(
                    columns.fits(width, graph),
                    "{} px with a {lanes} lane graph leaves the summary {} px",
                    width,
                    width - columns.fixed_width(graph)
                );
                let full = CommitColumns {
                    author: true,
                    date: true,
                };
                let no_date = CommitColumns {
                    author: true,
                    date: false,
                };
                if !columns.author {
                    assert!(
                        !no_date.fits(width, graph),
                        "the author still fits at {width} px with {lanes} lanes"
                    );
                } else if !columns.date {
                    assert!(
                        !full.fits(width, graph),
                        "the date still fits at {width} px with {lanes} lanes"
                    );
                }
            }
        }
    }

    #[test]
    fn the_app_minimum_window_keeps_the_author_but_drops_the_date() {
        // The sidebar's share at the minimum leaves the list this width.
        let width = crate::style::MIN_WINDOW_SIZE.0
            - crate::repo_view::sidebar::width_for(crate::style::MIN_WINDOW_SIZE.0);
        let graph = graph_canvas::indent(6);
        let columns = CommitColumns::for_width(width, graph);
        assert!(columns.author, "a 720 px window still names the author");
        assert!(!columns.date, "the date is the first column to give way");
        assert!(columns.fits(width, graph));
    }
}
