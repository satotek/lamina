//! The commit list: the column headings, the rows with their graph column and
//! badges, and the search filter bar above them.
//!
//! The list is built from [`Source`](crate::data::Source), so the synthetic
//! benchmark and a real repository share the same row renderer.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    Anchor, AnyElement, ClickEvent, Div, ElementId, Entity, FontWeight, InteractiveElement,
    IntoElement, MouseButton, ParentElement, SharedString, Stateful, Styled, Window, div, px,
    uniform_list,
};
use gpui_component::input::Input;
use gpui_component::popover::Popover;
use gpui_component::tooltip::Tooltip;
use gpui_component::{Sizable as _, v_flex};
use gpui_kit_assets::IconName;
use strata_core::session::Operation;
use strata_core::{HeadInfo, Oid, RefKind};

use super::history_ops::{self, CommitTarget};
use super::{RepoView, one_line};
use crate::data::{Badge, RowData};
use crate::graph_canvas;
use crate::scrollbar::WithScrollbar as _;
use crate::search::{Field, SEARCH_BAR, SearchMode};
use crate::style::{self, Design};
use crate::ui::RowTipExt as _;

const AUTHOR_WIDTH: f32 = 150.0;
const DATE_WIDTH: f32 = 96.0;
const COMMIT_WIDTH: f32 = 64.0;

/// Maximum inline ref chips rendered per commit row before collapsing the rest
/// into a `+N` overflow chip.
const MAX_VISIBLE_REF_CHIPS: usize = 2;
/// Minimum space reserved inside the description cell so inline ref chips do
/// not squeeze the commit subject off-screen.
const DESCRIPTION_SUBJECT_RESERVE: f32 = 105.0;
const MIN_SINGLE_CHIP_LABEL_WIDTH: f32 = 48.0;
/// Approximate space taken by the right-aligned changed-file count pill and its gap.
const CHANGED_COUNT_RESERVE: f32 = 32.0;

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
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CommitColumns {
    author: bool,
    date: bool,
    zoom: f32,
}

impl CommitColumns {
    /// The widest column set that leaves the summary its minimum, given the
    /// widest graph column in the list.
    pub(super) fn for_width(width: f32, graph_width: f32, zoom: f32) -> Self {
        [
            Self {
                author: true,
                date: true,
                zoom,
            },
            Self {
                author: true,
                date: false,
                zoom,
            },
            Self {
                author: false,
                date: false,
                zoom,
            },
        ]
        .into_iter()
        .find(|columns| columns.fits(width, graph_width))
        .unwrap_or(Self {
            author: false,
            date: false,
            zoom,
        })
    }

    fn author_width(self) -> f32 {
        AUTHOR_WIDTH * self.zoom
    }

    fn date_width(self) -> f32 {
        DATE_WIDTH * self.zoom
    }

    fn commit_width(self) -> f32 {
        COMMIT_WIDTH * self.zoom
    }

    /// The cells a row has: the graph, the summary, the optional columns and
    /// the short commit id.
    fn cells(self) -> f32 {
        3.0 + f32::from(self.author) + f32::from(self.date)
    }

    /// What the row reserves before the summary gets a share.
    fn fixed_width(self, graph_width: f32) -> f32 {
        ROW_PADDING * self.zoom
            + graph_width
            + self.commit_width()
            + if self.author {
                self.author_width()
            } else {
                0.0
            }
            + if self.date { self.date_width() } else { 0.0 }
            + (CELL_GAP * self.zoom) * (self.cells() - 1.0)
    }

    fn description_width(self, width: f32, graph_width: f32) -> f32 {
        (width - self.fixed_width(graph_width)).max(0.0)
    }

    fn fits(self, width: f32, graph_width: f32) -> bool {
        self.fixed_width(graph_width) + DESCRIPTION_MIN_WIDTH * self.zoom <= width
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
        cx: &gpui::App,
    ) -> AnyElement {
        let Some(source) = self.source() else {
            return empty_pane(&t!("history.empty.no_repository"), design);
        };
        let len = source.len();
        let has_uncommitted = self.has_uncommitted_row(cx);
        if len == 0 && !has_uncommitted && !source.complete() {
            // The walk has not published its first page yet. Saying "no
            // commits" here would be a lie in a large repository.
            return crate::ui::loading_state(t!("common.loading").to_string(), design)
                .into_any_element();
        }
        if len == 0 && !has_uncommitted {
            return empty_pane(&t!("history.empty.no_commits"), design);
        }

        let uncommitted_selected =
            has_uncommitted && (self.uncommitted_selected || self.selection.includes_uncommitted());
        let uncommitted_summary = has_uncommitted
            .then(|| self.uncommitted_summary())
            .flatten();
        let uncommitted_counts = if has_uncommitted {
            self.uncommitted_counts()
        } else {
            Default::default()
        };
        let uncommitted_graph = has_uncommitted.then(|| self.uncommitted_graph()).flatten();
        let uncommitted_state = self
            .session
            .as_ref()
            .map(|session| session.snapshot().state)
            .unwrap_or(strata_core::RepoState::Normal);
        let widest = self
            .lanes
            .widest()
            .max(uncommitted_graph.map_or(1, |graph| graph.lanes));

        // The decision uses the widest graph in the list, not the row's own:
        // columns that came and went between rows would read as noise.
        let columns = CommitColumns::for_width(
            width,
            graph_canvas::indent_at(widest, design.zoom),
            design.zoom,
        );
        let now = style::now_epoch();
        let selection = self.selection.clone();
        let revealed = self.revealed_commit();
        let row_design = design.clone();
        let focus = self.focus.clone();
        // The menu itself is built in `history_ops`; its rules only need the
        // snapshot, taken once per frame rather than per row.
        let menu_state = self.history_menu_state();
        let filter = self.search.filter().cloned();
        let count = filter
            .as_ref()
            .map_or(len + usize::from(has_uncommitted), |rows| rows.len());
        // The rows as displayed, which is what a shift range and arrow moves
        // walk; a filtered list shows a subset of the history indices.
        let order: Rc<Vec<usize>> = Rc::new(match &filter {
            Some(rows) => rows.as_ref().clone(),
            None => (0..len).collect(),
        });
        let date_format = design.date_format;
        let show_remote = design.show_remote_branches;
        let show_tags = design.show_tags;
        let show_signatures = design.show_commit_signatures;
        let show_changed_counts = design.show_changed_file_count;
        let request_signatures = show_signatures && self.session.is_some() && !self.smoke;
        let request_changed_counts = show_changed_counts && self.session.is_some() && !self.smoke;
        let list_entity = entity.clone();
        let defer_entity = entity.clone();
        let source_for_list = source.clone();
        let list = uniform_list("commits", count, move |range, window, cx| {
            let mut uncached = Vec::new();
            let mut visible_for_counts = Vec::new();
            let items = {
                let view = (show_signatures || show_changed_counts).then(|| list_entity.read(cx));
                range
                    .map(|position| {
                        let entity = list_entity.clone();
                        let focus = focus.clone();
                        if has_uncommitted && position == 0 {
                            let row_ui_state = row_state(uncommitted_selected, false);
                            let graph =
                                uncommitted_graph.unwrap_or(graph_canvas::UncommittedGraph {
                                    head_lane: 0,
                                    head_color: 0,
                                    has_head_edge: false,
                                    merge_target: None,
                                    merge_target_index: None,
                                    lanes: 1,
                                });
                            let summary = uncommitted_summary
                                .clone()
                                .unwrap_or_else(|| t!("history.uncommitted.summary").to_string());
                            let tooltip_text =
                                SharedString::from(t!("history.uncommitted.tooltip").to_string());
                            let click_entity = entity.clone();
                            return uncommitted_row(
                                &summary,
                                uncommitted_counts,
                                uncommitted_state,
                                graph,
                                show_changed_counts,
                                row_ui_state,
                                &row_design,
                                columns,
                            )
                            .cursor_pointer()
                            .row_tooltip(move |window, cx| {
                                Tooltip::new(tooltip_text.clone()).build(window, cx)
                            })
                            .on_click(move |event, window, cx| {
                                let open = event.click_count() >= 2;
                                let secondary = event.modifiers().secondary(cx);
                                let focus = focus.clone();
                                click_entity.update(cx, |this, cx| {
                                    if open {
                                        this.open_worktree(window, cx);
                                    } else if secondary {
                                        this.toggle_uncommitted_selection(cx);
                                        window.focus(&focus, cx);
                                    } else {
                                        this.select_uncommitted(cx);
                                        window.focus(&focus, cx);
                                    }
                                });
                            })
                            .into_any_element();
                        }

                        // While a search filters the list, `position` is a row of
                        // the result set and `index` the commit it points at.
                        let commit_pos = position - usize::from(has_uncommitted);
                        let index = filter
                            .as_ref()
                            .and_then(|rows| rows.get(commit_pos).copied())
                            .unwrap_or(commit_pos);
                        let Some(mut row) = source_for_list.row(index) else {
                            return div().into_any_element();
                        };
                        let commit_id = source_for_list.commit_id(index);
                        let sig_state = if show_signatures {
                            view.as_ref().and_then(|view| {
                                let oid = commit_id?;
                                let state = view.commit_signature_state(oid);
                                if request_signatures && view.should_schedule_signature(oid) {
                                    uncached.push(oid);
                                }
                                state
                            })
                        } else {
                            None
                        };
                        let changed_count = if show_changed_counts {
                            view.as_ref().and_then(|view| {
                                let oid = commit_id?;
                                if request_changed_counts {
                                    visible_for_counts.push(oid);
                                }
                                view.commit_changed_file_count(oid)
                            })
                        } else {
                            None
                        };
                        // The menu offers a delete for each tag on the commit, so
                        // collect them before the badge filters hide any.
                        let target = CommitTarget {
                            id: commit_id,
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
                        // A ref revealed from the sidebar keeps its row tinted
                        // while it is not the selection.
                        let revealed = revealed.is_some_and(|commit| Some(commit) == target.id);
                        let selected = selection.contains(index);
                        let row_ui_state = row_state(selected, revealed);
                        let row_graph_width = if filtered {
                            graph_canvas::search_indent_at(row_design.zoom)
                        } else {
                            graph_canvas::indent_at(row.graph.width, row_design.zoom)
                        };
                        let desc_width = columns.description_width(width, row_graph_width);
                        let chip_desc_width = if show_changed_counts {
                            (desc_width - row_design.scale(CHANGED_COUNT_RESERVE)).max(0.0)
                        } else {
                            desc_width
                        };
                        let element = if filtered {
                            // The graph is hidden while filtered: spaced-out rows
                            // would draw lanes that never existed between them.
                            filtered_commit_row(
                                index,
                                &row,
                                sig_state.as_ref(),
                                changed_count,
                                row_ui_state,
                                &date,
                                &row_design,
                                columns,
                                chip_desc_width,
                                &entity,
                            )
                        } else {
                            commit_row(
                                index,
                                &row,
                                sig_state.as_ref(),
                                changed_count,
                                uncommitted_graph.and_then(|graph| graph.merge_edge_at(index)),
                                row_ui_state,
                                &date,
                                &row_design,
                                columns,
                                chip_desc_width,
                                &entity,
                            )
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
                        .on_mouse_down(
                            MouseButton::Right,
                            move |_, _, cx| {
                                right_entity
                                    .update(cx, |this, cx| this.ensure_commit_selection(index, cx));
                            },
                        );
                        let menu_entity = entity;
                        let menu_state = menu_state.clone();
                        crate::ui::context_menu(
                            ("commit-row-menu", index),
                            &row_design,
                            element,
                            move |menu, window, cx| {
                                let multi = menu_entity.read(cx).multi_pick_context();
                                let variables = menu_entity.read(cx).custom_variables();
                                let menu = history_ops::commit_menu(
                                    menu,
                                    target.clone(),
                                    menu_state.clone(),
                                    multi,
                                    menu_entity.clone(),
                                    window,
                                    cx,
                                );
                                crate::commands::submenu(
                                    menu,
                                    crate::commands::Context::Commit,
                                    &variables,
                                    window,
                                    cx,
                                )
                            },
                        )
                        .w_full()
                        .into_any_element()
                    })
                    .collect::<Vec<_>>()
            };
            if !uncached.is_empty() || request_changed_counts {
                let defer_entity = defer_entity.clone();
                window.defer(cx, move |_, cx| {
                    defer_entity.update(cx, |this, cx| {
                        if !uncached.is_empty() {
                            this.schedule_visible_signatures(uncached, cx);
                        }
                        if request_changed_counts {
                            this.observe_visible_changed_file_counts(visible_for_counts, cx);
                        }
                    });
                });
            }
            items
        })
        .track_scroll(&self.commit_scroll)
        .with_scrollbar(&self.commit_scroll)
        .flex_1();

        let body: AnyElement = if count == 0 {
            if self.search.is_scanning() {
                empty_pane(&t!("history.searching"), design)
            } else {
                empty_pane(&t!("history.no_match"), design)
            }
        } else {
            list.into_any_element()
        };

        let mut column = div().flex().flex_col().flex_1().min_h_0().min_w_0();
        if self.hidden_ref_count() > 0 {
            column = column.child(self.hidden_refs_banner(entity.clone(), design));
        }
        column = column.child(commit_header(design, columns));
        if self.search.is_active() || self.search.mode().is_content() {
            column = column.child(self.search_bar(entity, design));
        }
        column = column.child(body);
        if !self.source().is_some_and(|source| source.complete()) {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .h(design.px(20.0))
                    .flex_none()
                    .px_3()
                    .text_size(design.px(10.0))
                    .text_color(design.muted)
                    .child(
                        t!(&crate::i18n::count_key("history.loading", len), count = len)
                            .to_string(),
                    ),
            );
        }
        column.into_any_element()
    }

    /// The strip above the commit header while the graph leaves refs out.
    ///
    /// It names the count rather than the branches: the sidebar's closed-eye
    /// icons say which ones, and the history pane should not grow a list that
    /// pushes the commits down.
    fn hidden_refs_banner(&self, entity: gpui::Entity<Self>, design: &Design) -> AnyElement {
        let count = self.hidden_ref_count();
        let show_all =
            crate::ui::button("show-all-hidden-refs", t!("sidebar.show_all").to_string())
                .size(crate::ui::ButtonSize::Tiny)
                .kind(crate::ui::ButtonKind::Ghost)
                .on_click(move |_, _, cx| {
                    entity.update(cx, |view, cx| view.show_hidden_refs(cx));
                });
        crate::ui::banner(
            crate::ui::BannerKind::Info,
            t!(
                &crate::i18n::count_key("history.hidden_refs", count),
                count = count
            )
            .to_string(),
            design,
        )
        .action(show_all)
        .into_any_element()
    }

    /// Match toggles and the match count, shown only while a search filters
    /// the list or a content search mode is selected. The text field itself is
    /// in the title bar.
    fn search_bar(&self, entity: gpui::Entity<Self>, design: &Design) -> AnyElement {
        let mode = self.search.mode();
        let mut bar = div()
            .flex()
            .items_center()
            .gap_2()
            .h(design.px(30.0))
            .flex_none()
            .px_3()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .key_context(SEARCH_BAR)
            .child(
                div()
                    .flex_none()
                    .text_size(design.px(10.5))
                    .text_color(design.muted)
                    .child(t!("history.filters").to_string()),
            )
            .child(search_chip(
                "summary",
                t!("history.filter.summary"),
                Field::Summary,
                mode,
                self.search.fields(),
                entity.clone(),
            ))
            .child(search_chip(
                "author",
                t!("history.filter.author"),
                Field::Author,
                mode,
                self.search.fields(),
                entity.clone(),
            ))
            .child(search_chip(
                "hash",
                t!("history.filter.hash"),
                Field::Hash,
                mode,
                self.search.fields(),
                entity.clone(),
            ))
            .child(
                div()
                    .w(px(1.0))
                    .h(design.px(14.0))
                    .mx_0p5()
                    .bg(design.border),
            )
            .child(search_mode_chip(
                "content",
                IconName::FileSearch,
                SearchMode::Content,
                mode,
                t!("history.search.content_tooltip"),
                entity.clone(),
            ))
            .child(search_mode_chip(
                "content-regex",
                IconName::Regex,
                SearchMode::ContentRegex,
                mode,
                t!("history.search.regex_tooltip"),
                entity,
            ));

        if mode.is_content()
            && let Some(path_input) = self.search.path_input()
        {
            bar = bar.child(
                div()
                    .w(design.px(180.0))
                    .h(design.px(22.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded(design.px(5.0))
                    .bg(design.field)
                    .child(
                        Input::new(path_input)
                            .appearance(false)
                            .bordered(false)
                            .small(),
                    ),
            );
        }

        bar = bar.child(div().flex_1());

        let label = match self.search.error() {
            Some(error) => one_line(error),
            None => self.search.count_label(self.commit_count()),
        };
        bar.child(
            div()
                .flex_none()
                .max_w(design.px(340.0))
                .truncate()
                .text_size(design.px(10.5))
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
        .h(design.px(COMMIT_HEADER_HEIGHT))
        .flex_none()
        .pl(design.px(20.0))
        .pr(design.px(13.0))
        .border_b_1()
        .border_color(design.border)
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(design.muted)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(t!("history.column.description").to_string()),
        )
        .when(columns.author, |header| {
            header.child(
                div()
                    .w(px(columns.author_width()))
                    .flex_none()
                    .child(t!("history.column.author").to_string()),
            )
        })
        .when(columns.date, |header| {
            header.child(
                div()
                    .w(px(columns.date_width()))
                    .flex_none()
                    .child(t!("history.column.date").to_string()),
            )
        })
        .child(
            div()
                .w(px(columns.commit_width()))
                .flex_none()
                .child(t!("history.column.commit").to_string()),
        )
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

#[allow(clippy::too_many_arguments)]
fn commit_description_cell(
    row_index: usize,
    sig_prefix: &'static str,
    data: &RowData<'_>,
    signature: Option<&crate::ui::CommitSignatureState>,
    changed_count: Option<usize>,
    design: &Design,
    description_width: f32,
    entity: &Entity<RepoView>,
) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .gap(design.px(6.0))
        .overflow_hidden()
        .when_some(
            ref_chips_strip(row_index, &data.badges, description_width, design, entity),
            |cell, strip| cell.child(strip),
        )
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
        .when_some(
            signature
                .and_then(|sig| crate::ui::signature_badge((sig_prefix, row_index), sig, design)),
            |cell, badge| cell.child(badge),
        )
        .when(data.shallow_boundary, |cell| {
            cell.child(
                div()
                    .flex_none()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(t!("history.shallow_boundary").to_string()),
            )
        })
        .when_some(changed_count, |cell, count| {
            cell.child(div().flex_1().min_w_0())
                .child(crate::ui::changed_file_count_badge(
                    ("commit-files", row_index),
                    count,
                    design,
                ))
        })
}

#[allow(clippy::too_many_arguments)]
fn commit_row(
    row_index: usize,
    data: &RowData<'_>,
    signature: Option<&crate::ui::CommitSignatureState>,
    changed_count: Option<usize>,
    merge_edge: Option<graph_canvas::MergeEdge>,
    state: crate::ui::RowState,
    date: &str,
    design: &Design,
    columns: CommitColumns,
    description_width: f32,
    entity: &Entity<RepoView>,
) -> gpui::Stateful<Div> {
    let avatar_color = design.avatar_color(&data.email);
    let initials = style::initials(&data.author);
    // The graph fades into the row's own fill, so it has to be the same colour
    // `ui::list_row` gives the row.
    let fade = crate::ui::row_surface(state, design);
    crate::ui::list_row(("commit-row", row_index), state, design)
        .gap_3()
        .h(design.px(graph_canvas::ROW_HEIGHT))
        .w_full()
        .mx(px(0.0))
        .px_3()
        .child(graph_canvas::graph_column(
            data.graph,
            data.shallow_boundary,
            merge_edge,
            design.lane,
            fade,
            design.muted,
            design.zoom,
        ))
        .child(commit_description_cell(
            row_index,
            "commit-sig",
            data,
            signature,
            changed_count,
            design,
            description_width,
            entity,
        ))
        .when(columns.author, |row| {
            row.child(
                div()
                    .w(px(columns.author_width()))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(design.px(7.0))
                    .overflow_hidden()
                    .child(crate::ui::avatar(
                        initials.clone(),
                        avatar_color,
                        design.scale(18.0),
                    ))
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
                    .w(px(columns.date_width()))
                    .flex_none()
                    .truncate()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(date.to_owned()),
            )
        })
        .child(
            div()
                .w(px(columns.commit_width()))
                .flex_none()
                .font_family(design.mono.clone())
                .text_size(px(design.code))
                .text_color(design.muted)
                .child(data.id.to_string()),
        )
}

#[allow(clippy::too_many_arguments)]
fn uncommitted_row(
    summary: &str,
    counts: super::uncommitted::UncommittedCounts,
    repo_state: strata_core::RepoState,
    graph: graph_canvas::UncommittedGraph,
    show_changed_counts: bool,
    state: crate::ui::RowState,
    design: &Design,
    columns: CommitColumns,
) -> gpui::Stateful<Div> {
    let fade = crate::ui::row_surface(state, design);
    let badge_tone = if counts.conflicts > 0 {
        crate::ui::BadgeTone::Danger
    } else if repo_state != strata_core::RepoState::Normal {
        crate::ui::BadgeTone::Warning
    } else {
        crate::ui::BadgeTone::Accent
    };
    crate::ui::list_row("uncommitted-row", state, design)
        .gap_3()
        .h(design.px(graph_canvas::ROW_HEIGHT))
        .w_full()
        .mx(px(0.0))
        .px_3()
        .child(graph_canvas::uncommitted_graph_column(
            graph,
            design.lane,
            fade,
            design.zoom,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(design.px(6.0))
                .overflow_hidden()
                .child(crate::ui::status_badge(
                    t!("history.uncommitted.badge_worktree").to_string(),
                    badge_tone,
                    design,
                ))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(design.text)
                        .font_weight(FontWeight::MEDIUM)
                        .child(summary.to_string()),
                )
                .when(show_changed_counts && counts.total() > 0, |cell| {
                    cell.child(div().flex_1().min_w_0())
                        .child(crate::ui::changed_file_count_badge(
                            "uncommitted-files",
                            counts.total(),
                            design,
                        ))
                }),
        )
        .when(columns.author, |row| {
            row.child(div().w(px(columns.author_width())).flex_none())
        })
        .when(columns.date, |row| {
            row.child(div().w(px(columns.date_width())).flex_none())
        })
        .child(
            div()
                .w(px(columns.commit_width()))
                .flex_none()
                .font_family(design.mono.clone())
                .text_size(px(design.code))
                .text_color(design.muted)
                .child("-------"),
        )
}

/// A commit row in the filtered result set. A small fixed indent stands in for
/// the graph, which is hidden: the matching commits are not adjacent in
/// history, so lanes between them would be fiction.
#[allow(clippy::too_many_arguments)]
fn filtered_commit_row(
    row_index: usize,
    data: &RowData<'_>,
    signature: Option<&crate::ui::CommitSignatureState>,
    changed_count: Option<usize>,
    state: crate::ui::RowState,
    date: &str,
    design: &Design,
    columns: CommitColumns,
    description_width: f32,
    entity: &Entity<RepoView>,
) -> gpui::Stateful<Div> {
    let avatar_color = design.avatar_color(&data.email);
    let initials = style::initials(&data.author);
    crate::ui::list_row(("commit-row", row_index), state, design)
        .gap_3()
        .h(design.px(graph_canvas::ROW_HEIGHT))
        .w_full()
        .mx(px(0.0))
        .px_3()
        .child(
            div()
                .w(px(graph_canvas::search_indent_at(design.zoom)))
                .flex_none(),
        )
        .child(commit_description_cell(
            row_index,
            "filtered-commit-sig",
            data,
            signature,
            changed_count,
            design,
            description_width,
            entity,
        ))
        .when(columns.author, |row| {
            row.child(
                div()
                    .w(px(columns.author_width()))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(design.px(7.0))
                    .overflow_hidden()
                    .child(crate::ui::avatar(
                        initials.clone(),
                        avatar_color,
                        design.scale(18.0),
                    ))
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
                    .w(px(columns.date_width()))
                    .flex_none()
                    .truncate()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(date.to_owned()),
            )
        })
        .child(
            div()
                .w(px(columns.commit_width()))
                .flex_none()
                .font_family(design.mono.clone())
                .text_size(px(design.code))
                .text_color(design.muted)
                .child(data.id.to_string()),
        )
}

/// One of the three match toggles above the commit list.
fn search_chip(
    id: &'static str,
    label: impl Into<gpui::SharedString>,
    field: Field,
    mode: SearchMode,
    fields: crate::search::Fields,
    entity: gpui::Entity<RepoView>,
) -> AnyElement {
    let on = !mode.is_content() && fields.enabled(field);
    let label = label.into();
    crate::ui::button(format!("search-chip-{id}"), label.clone())
        .size(crate::ui::ButtonSize::Tiny)
        .kind(if on {
            crate::ui::ButtonKind::Primary
        } else {
            crate::ui::ButtonKind::Ghost
        })
        .tooltip(t!("history.filter.match", field = label))
        .on_click(move |_event, _window, cx| {
            entity.update(cx, |this, cx| this.toggle_search_field(field, cx));
        })
        .into_any_element()
}

/// Content / regex-content search mode toggle above the commit list.
fn search_mode_chip(
    id: &'static str,
    icon: IconName,
    target: SearchMode,
    current: SearchMode,
    tooltip: impl Into<gpui::SharedString>,
    entity: gpui::Entity<RepoView>,
) -> AnyElement {
    let on = current == target;
    crate::ui::icon_toggle(format!("search-mode-{id}"), icon, on)
        .size(crate::ui::ButtonSize::Tiny)
        .tooltip(tooltip)
        .on_click(move |_event, _window, cx| {
            entity.update(cx, |this, cx| this.toggle_search_mode(target, cx));
        })
        .into_any_element()
}

#[derive(Clone, Debug, PartialEq)]
struct RefChipPlan<'a> {
    visible: Vec<Badge<'a>>,
    hidden: Vec<Badge<'a>>,
    all: Vec<Badge<'a>>,
    max_label_width: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnedBadge {
    label: String,
    kind: RefKind,
    head: bool,
}

fn ref_display_priority<'a>(badge: &'a Badge<'_>) -> (u8, u8, &'a str) {
    let head_rank = u8::from(!badge.head);
    let kind_rank = match badge.kind {
        RefKind::LocalBranch => 0,
        RefKind::Tag => 1,
        RefKind::RemoteBranch => 2,
    };
    (head_rank, kind_rank, badge.label.as_ref())
}

fn ref_chip_width_budget(description_width: f32, zoom: f32) -> f32 {
    (description_width - DESCRIPTION_SUBJECT_RESERVE * zoom).max(0.0)
}

/// Orders commit refs by relevance (HEAD/current branch, local branches, tags,
/// remote branches) and fits at most [`MAX_VISIBLE_REF_CHIPS`] within the
/// description column's width budget, leaving the rest for a `+N` chip.
fn plan_ref_chips<'a>(badges: &[Badge<'a>], description_width: f32, zoom: f32) -> RefChipPlan<'a> {
    let mut all = badges.to_vec();
    all.sort_by(|a, b| ref_display_priority(a).cmp(&ref_display_priority(b)));

    let max_label_width = (crate::ui::REF_BADGE_MAX_LABEL_WIDTH * zoom)
        .min((description_width - 80.0 * zoom).max(MIN_SINGLE_CHIP_LABEL_WIDTH * zoom));

    if all.is_empty() {
        return RefChipPlan {
            visible: Vec::new(),
            hidden: Vec::new(),
            all,
            max_label_width,
        };
    }

    // Keep a single ref rendered as a chip so long single names truncate with
    // ellipsis and expose their full name in the tooltip instead of turning
    // into a `+1` chip.
    if all.len() == 1 {
        return RefChipPlan {
            visible: all.clone(),
            hidden: Vec::new(),
            all,
            max_label_width,
        };
    }

    let budget = ref_chip_width_budget(description_width, zoom);
    let widths: Vec<f32> = all
        .iter()
        .map(|badge| {
            crate::ui::estimate_ref_badge_width(badge.label.as_ref(), max_label_width, zoom)
        })
        .collect();

    let gap = crate::ui::REF_BADGE_GAP * zoom;
    if all.len() <= MAX_VISIBLE_REF_CHIPS {
        let total_width = widths.iter().sum::<f32>() + gap * (all.len() - 1) as f32;
        if total_width <= budget {
            return RefChipPlan {
                visible: all.clone(),
                hidden: Vec::new(),
                all,
                max_label_width,
            };
        }
    }

    let mut shown = 0usize;
    let max_with_overflow = MAX_VISIBLE_REF_CHIPS.min(all.len() - 1);
    for candidate in (1..=max_with_overflow).rev() {
        let hidden_count = all.len() - candidate;
        let chips_width = widths[..candidate].iter().sum::<f32>();
        let gaps_width = gap * candidate as f32;
        let overflow_width = crate::ui::estimate_ref_overflow_width(hidden_count, zoom);
        if chips_width + gaps_width + overflow_width <= budget {
            shown = candidate;
            break;
        }
    }

    RefChipPlan {
        visible: all[..shown].to_vec(),
        hidden: all[shown..].to_vec(),
        all,
        max_label_width,
    }
}

fn overflow_tooltip(hidden: &[Badge<'_>]) -> String {
    hidden
        .iter()
        .map(|badge| badge.label.as_ref())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders one inline ref chip.
///
/// Keeping chip rendering in this single helper lets row layout and future
/// drop targets share one element builder across normal and filtered rows.
fn badge_chip(
    id: impl Into<ElementId>,
    badge: &Badge<'_>,
    max_label_width: f32,
    design: &Design,
) -> Stateful<Div> {
    let full_name = SharedString::from(badge.label.to_string());
    crate::ui::ref_chip(
        id,
        badge.kind,
        badge.head,
        full_name.clone(),
        max_label_width,
        design,
    )
    .row_tooltip(move |window, cx| Tooltip::new(full_name.clone()).build(window, cx))
}

/// A local branch chip is also the drop target of a branch dragged from the
/// sidebar, which opens the same merge menu the sidebar rows do. The wrapper's
/// padding is room for the drop highlight, since the chip paints its own fill.
fn branch_drop_chip(
    chip: Stateful<Div>,
    badge: &Badge<'_>,
    slot: usize,
    design: &Design,
    entity: &Entity<RepoView>,
) -> AnyElement {
    if badge.kind != RefKind::LocalBranch {
        return chip.into_any_element();
    }
    let wrapper = div()
        .id(("commit-ref-drop", slot))
        .flex_none()
        .flex()
        .items_center()
        .rounded_full()
        .p(design.px(2.0))
        .child(chip);
    crate::branches::branch_drop_target_with(
        wrapper,
        &badge.label,
        design.accent_soft,
        entity.clone(),
    )
    .into_any_element()
}

fn ref_chips_strip(
    row_index: usize,
    badges: &[Badge<'_>],
    description_width: f32,
    design: &Design,
    entity: &Entity<RepoView>,
) -> Option<AnyElement> {
    if badges.is_empty() {
        return None;
    }

    let plan = plan_ref_chips(badges, description_width, design.zoom);
    let mut strip = div()
        .flex()
        .items_center()
        .gap(design.px(crate::ui::REF_BADGE_GAP))
        .flex_none()
        .overflow_hidden();

    for (chip_index, badge) in plan.visible.iter().enumerate() {
        let chip = badge_chip(
            ("commit-ref", (row_index << 8) | chip_index),
            badge,
            plan.max_label_width,
            design,
        );
        strip = strip.child(branch_drop_chip(
            chip,
            badge,
            (row_index << 8) | chip_index,
            design,
            entity,
        ));
    }

    if !plan.hidden.is_empty() {
        let tooltip = overflow_tooltip(&plan.hidden);
        let trigger = crate::ui::ref_overflow_chip(
            ("commit-ref-overflow-chip", row_index),
            plan.hidden.len(),
            design,
        )
        .tooltip(tooltip);
        let all_owned: Vec<OwnedBadge> = plan
            .all
            .iter()
            .map(|badge| OwnedBadge {
                label: badge.label.to_string(),
                kind: badge.kind,
                head: badge.head,
            })
            .collect();
        strip = strip.child(ref_overflow_popover(
            row_index,
            trigger,
            all_owned,
            design.clone(),
            entity.clone(),
        ));
    }

    Some(strip.into_any_element())
}

fn ref_overflow_popover(
    row_index: usize,
    trigger: crate::ui::RefOverflowChip,
    refs: Vec<OwnedBadge>,
    design: Design,
    entity: Entity<RepoView>,
) -> AnyElement {
    Popover::new(("commit-ref-overflow", row_index))
        .anchor(Anchor::TopLeft)
        .offset(design.px(4.0))
        .trigger(trigger)
        .content(move |_, _window, cx| {
            let popover = cx.entity();
            let total = refs.len();
            let mut list = v_flex()
                .id(("commit-ref-overflow-list", row_index))
                .gap(design.px(2.0))
                .max_h(design.px(260.0))
                .overflow_y_scroll();

            for (item_index, item) in refs.iter().enumerate() {
                let item = item.clone();
                let click_item = item.clone();
                let click_entity = entity.clone();
                let dismiss_popover = popover.clone();
                let glyph_data = match item.kind {
                    RefKind::LocalBranch => style::glyph::BRANCH,
                    RefKind::RemoteBranch => style::glyph::CLOUD,
                    RefKind::Tag => style::glyph::TAG,
                };
                let icon_color = match item.kind {
                    RefKind::LocalBranch if item.head => design.accent_text,
                    RefKind::LocalBranch => design.accent_text,
                    RefKind::RemoteBranch => design.remote_text,
                    RefKind::Tag => design.tag_text,
                };
                let full_label = SharedString::from(item.label.clone());
                let tooltip_label = full_label.clone();

                list = list.child(
                    crate::ui::list_row(
                        ("commit-ref-overflow-item", (row_index << 8) | item_index),
                        crate::ui::RowState::NORMAL.head(item.head),
                        &design,
                    )
                    .h(design.px(26.0))
                    .mx(px(0.0))
                    .w_full()
                    .cursor_pointer()
                    .child(
                        style::glyph_icon(glyph_data, design.scale(12.0), icon_color).flex_none(),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(full_label))
                    .when(item.head, |row| {
                        row.child(crate::ui::status_badge(
                            t!("history.refs.current").to_string(),
                            crate::ui::BadgeTone::Accent,
                            &design,
                        ))
                    })
                    .row_tooltip(move |window, cx| {
                        Tooltip::new(tooltip_label.clone()).build(window, cx)
                    })
                    .on_click(move |_: &ClickEvent, window, cx| {
                        dismiss_popover.update(cx, |state, cx| state.dismiss(window, cx));
                        click_entity.update(cx, |view, cx| {
                            activate_ref_badge(&click_item, view, window, cx);
                        });
                    }),
                );
            }

            v_flex()
                .min_w(design.px(210.0))
                .max_w(design.px(340.0))
                .gap(design.px(4.0))
                .child(
                    div()
                        .px_2()
                        .text_size(px(design.tiny()))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(design.muted)
                        .child(t!("history.refs.title", count = total).to_string()),
                )
                .child(list)
        })
        .into_any_element()
}

fn activate_ref_badge(
    item: &OwnedBadge,
    view: &mut RepoView,
    window: &mut Window,
    cx: &mut Context<RepoView>,
) {
    let is_detached_head = item.head
        && item.label == "HEAD"
        && view
            .source()
            .is_some_and(|source| matches!(source.head(), Some(HeadInfo::Detached { .. })));
    if is_detached_head {
        return;
    }

    let busy = view.is_busy();
    let has_session = view.session.is_some();
    match item.kind {
        RefKind::LocalBranch => {
            if item.head || !has_session || busy {
                view.reveal_ref(RefKind::LocalBranch, &item.label, window, cx);
            } else {
                let branch = item.label.clone();
                let label = t!("branch.op.checkout", branch = branch).to_string();
                view.run_operation(Operation::Checkout { branch }, label, window, cx);
            }
        }
        RefKind::RemoteBranch => {
            if let Some(local) =
                crate::branches::local_name_for_remote(&item.label).map(str::to_owned)
                && has_session
                && !busy
            {
                let existing = view
                    .source()
                    .is_some_and(|source| source.sidebar().local.iter().any(|b| b == &local));
                crate::branches::checkout_remote(view, &item.label, &local, existing, window, cx);
            } else {
                view.reveal_ref(RefKind::RemoteBranch, &item.label, window, cx);
            }
        }
        RefKind::Tag => {
            view.reveal_ref(RefKind::Tag, &item.label, window, cx);
        }
    }
}

fn empty_pane(text: &str, design: &Design) -> AnyElement {
    crate::ui::empty_state(text, design).into_any_element()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    fn test_badge(label: &'static str, kind: RefKind, head: bool) -> Badge<'static> {
        Badge {
            label: Cow::Borrowed(label),
            kind,
            head,
        }
    }

    #[test]
    fn the_date_goes_before_the_author() {
        let graph = graph_canvas::indent(1);

        let wide = CommitColumns::for_width(900.0, graph, 1.0);
        assert!(wide.author && wide.date, "a wide list keeps both columns");

        let narrow = CommitColumns::for_width(500.0, graph, 1.0);
        assert!(
            narrow.author && !narrow.date,
            "a narrow list drops the date first"
        );

        let narrowest = CommitColumns::for_width(300.0, graph, 1.0);
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
                let columns = CommitColumns::for_width(width, graph, 1.0);
                assert!(
                    columns.fits(width, graph),
                    "{} px with a {lanes} lane graph leaves the summary {} px",
                    width,
                    width - columns.fixed_width(graph)
                );
                let full = CommitColumns {
                    author: true,
                    date: true,
                    zoom: 1.0,
                };
                let no_date = CommitColumns {
                    author: true,
                    date: false,
                    zoom: 1.0,
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
        let columns = CommitColumns::for_width(width, graph, 1.0);
        assert!(columns.author, "a 720 px window still names the author");
        assert!(!columns.date, "the date is the first column to give way");
        assert!(columns.fits(width, graph));
    }

    #[test]
    fn ref_chips_order_head_local_tag_then_remote_and_cap_at_two() {
        let badges = vec![
            test_badge("origin/main", RefKind::RemoteBranch, false),
            test_badge("v1.0.0", RefKind::Tag, false),
            test_badge("alpha", RefKind::LocalBranch, false),
            test_badge("t3/topic-1", RefKind::LocalBranch, false),
            test_badge("zeta", RefKind::LocalBranch, true),
        ];

        let plan = plan_ref_chips(&badges, 600.0, 1.0);
        let all_labels: Vec<&str> = plan.all.iter().map(|b| b.label.as_ref()).collect();
        assert_eq!(
            all_labels,
            ["zeta", "alpha", "t3/topic-1", "v1.0.0", "origin/main"]
        );

        let visible_labels: Vec<&str> = plan.visible.iter().map(|b| b.label.as_ref()).collect();
        let hidden_labels: Vec<&str> = plan.hidden.iter().map(|b| b.label.as_ref()).collect();
        assert_eq!(visible_labels, ["zeta", "alpha"]);
        assert_eq!(hidden_labels, ["t3/topic-1", "v1.0.0", "origin/main"]);
        assert_eq!(
            overflow_tooltip(&plan.hidden),
            "t3/topic-1\nv1.0.0\norigin/main"
        );
    }

    #[test]
    fn ref_chips_shrink_count_on_narrow_columns() {
        let badges = vec![
            test_badge(
                "feature/very-long-branch-name-alpha",
                RefKind::LocalBranch,
                true,
            ),
            test_badge(
                "feature/very-long-branch-name-beta",
                RefKind::LocalBranch,
                false,
            ),
            test_badge("origin/main", RefKind::RemoteBranch, false),
        ];

        // Wide description shows 2 chips + `+1`.
        let wide = plan_ref_chips(&badges, 560.0, 1.0);
        assert_eq!(wide.visible.len(), 2);
        assert_eq!(wide.hidden.len(), 1);

        // Medium description only has room for 1 max-width chip + `+2`.
        let medium = plan_ref_chips(&badges, 305.0, 1.0);
        assert_eq!(medium.visible.len(), 1);
        assert_eq!(medium.hidden.len(), 2);

        // Very narrow description leaves only the `+3` chip so the summary stays visible.
        let narrow = plan_ref_chips(&badges, 140.0, 1.0);
        assert_eq!(narrow.visible.len(), 0);
        assert_eq!(narrow.hidden.len(), 3);
    }

    #[test]
    fn single_long_branch_chip_stays_visible_without_overflow_chip() {
        let badges = vec![test_badge(
            "t3/an-unusually-long-branch-name-that-truncates-with-ellipsis",
            RefKind::LocalBranch,
            false,
        )];

        let plan = plan_ref_chips(&badges, 160.0, 1.0);
        assert_eq!(plan.visible.len(), 1);
        assert!(plan.hidden.is_empty());
        assert!(plan.max_label_width <= crate::ui::REF_BADGE_MAX_LABEL_WIDTH);
    }

    #[test]
    fn commit_columns_and_ref_chips_scale_proportionally_with_zoom() {
        let graph_1x = graph_canvas::indent_at(3, 1.0);
        let graph_15x = graph_canvas::indent_at(3, 1.5);
        assert!((graph_15x - graph_1x * 1.5).abs() < 0.01);

        let cols_1x = CommitColumns::for_width(500.0, graph_1x, 1.0);
        let cols_15x = CommitColumns::for_width(500.0 * 1.5, graph_15x, 1.5);
        assert_eq!(cols_1x.author, cols_15x.author);
        assert_eq!(cols_1x.date, cols_15x.date);
        assert!(
            (cols_15x.fixed_width(graph_15x) - cols_1x.fixed_width(graph_1x) * 1.5).abs() < 0.01
        );

        let badges = vec![
            test_badge("feature/alpha-branch", RefKind::LocalBranch, true),
            test_badge("feature/beta-branch", RefKind::LocalBranch, false),
            test_badge("origin/main", RefKind::RemoteBranch, false),
        ];
        let plan_1x = plan_ref_chips(&badges, 305.0, 1.0);
        let plan_15x = plan_ref_chips(&badges, 305.0 * 1.5, 1.5);
        assert_eq!(plan_1x.visible.len(), plan_15x.visible.len());
        assert_eq!(plan_1x.hidden.len(), plan_15x.hidden.len());
        assert!((plan_15x.max_label_width - plan_1x.max_label_width * 1.5).abs() < 0.01);
    }
}
