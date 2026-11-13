//! The commit list: the column headings, the rows with their graph column and
//! badges, and the search filter bar above them.
//!
//! The list is built from [`Source`](crate::data::Source), so the synthetic
//! benchmark and a real repository share the same row renderer.

use gpui::prelude::*;
use gpui::{
    AnyElement, Div, FontWeight, InteractiveElement, IntoElement, ParentElement, Styled, div, px,
    uniform_list,
};
use gpui_component::menu::ContextMenuExt as _;
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

impl RepoView {
    /// Commit of the ref the sidebar last revealed. The field lives on
    /// [`RepoView`] because `reveal_ref` and `reveal_stash` set it there.
    pub(super) fn revealed_commit(&self) -> Option<Oid> {
        self.revealed_commit
    }

    pub(super) fn commit_pane(&self, entity: gpui::Entity<Self>, design: &Design) -> AnyElement {
        let Some(source) = self.source() else {
            return empty_pane("No repository", design);
        };
        let len = source.len();
        if len == 0 {
            return empty_pane("No commits yet", design);
        }

        let now = style::now_epoch();
        let selected = self.selected;
        let revealed = self.revealed_commit();
        let row_design = design.clone();
        let focus = self.focus.clone();
        // The menu itself is built in `history_ops`; its rules only need the
        // snapshot, taken once per frame rather than per row.
        let menu_state = self.history_menu_state();
        let filter = self.search.filter().cloned();
        let count = filter.as_ref().map_or(len, |rows| rows.len());
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
                    let id = ("commit-row", index);
                    // A ref revealed from the sidebar keeps its row tinted
                    // while it is not the selection.
                    let revealed = revealed.is_some_and(|commit| Some(commit) == target.id);
                    let element = if filtered {
                        // The graph is hidden while filtered: spaced-out rows
                        // would draw lanes that never existed between them.
                        filtered_commit_row(
                            id,
                            &row,
                            selected == Some(index),
                            revealed,
                            &date,
                            &row_design,
                        )
                    } else {
                        commit_row(
                            id,
                            &row,
                            selected == Some(index),
                            revealed,
                            &date,
                            &row_design,
                        )
                    }
                    .cursor_pointer()
                    .on_click(move |_event, window, cx| {
                        click_entity.update(cx, |this, cx| {
                            if filtered {
                                this.select_result(index, position, cx);
                            } else {
                                this.jump(index, cx);
                            }
                            window.focus(&focus, cx);
                        });
                    });
                    let menu_entity = entity;
                    let menu_state = menu_state.clone();
                    element
                        .context_menu(move |menu, window, cx| {
                            history_ops::commit_menu(
                                menu,
                                target.clone(),
                                menu_state.clone(),
                                menu_entity.clone(),
                                window,
                                cx,
                            )
                        })
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
        column = column.child(commit_header(design));
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
fn commit_header(design: &Design) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .h(px(28.0))
        .flex_none()
        .pl(px(20.0))
        .pr(px(13.0))
        .border_b_1()
        .border_color(design.border)
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(design.muted)
        .child(div().flex_1().min_w_0().child("Description"))
        .child(div().w(px(AUTHOR_WIDTH)).flex_none().child("Author"))
        .child(div().w(px(DATE_WIDTH)).flex_none().child("Date"))
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
) -> gpui::Stateful<Div> {
    let avatar_color = design.avatar_color(&data.email);
    let initials = style::initials(&data.author);
    let state = row_state(selected, revealed);
    // The graph fades into the row's own fill, following the same precedence
    // as `ui::list_row`: selection, then reveal, then the list background.
    let fade = if selected {
        design.row_selected
    } else if revealed {
        design.field
    } else {
        design.background
    };
    crate::ui::list_row(id, state, design)
        .gap_3()
        .h(px(graph_canvas::ROW_HEIGHT))
        .w_full()
        .mx(px(0.0))
        .px_3()
        .child(graph_canvas::graph_column(data.graph, design.lane, fade))
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
                ),
        )
        .child(
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
        .child(
            div()
                .w(px(DATE_WIDTH))
                .flex_none()
                .truncate()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(date.to_owned()),
        )
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
                ),
        )
        .child(
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
        .child(
            div()
                .w(px(DATE_WIDTH))
                .flex_none()
                .truncate()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(date.to_owned()),
        )
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
