//! Window chrome and the commit/diff lists.
//!
//! Everything here renders from [`RepoView`]'s state; nothing in this module
//! calls git or the session. The commit list is built from [`Source`], so the
//! synthetic benchmark and a real repository share the same row renderer.
//!
//! Colours and sizes come from [`Design`], one palette resolved per frame from
//! the theme and the user's settings; the layout follows the design mockups.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Div, FontWeight, Hsla, InteractiveElement,
    IntoElement, ParentElement, Render, Styled, Window, div, px, uniform_list,
};
use gpui_component::input::Input;
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_component::{ActiveTheme, Icon, Sizable as _, TitleBar};
use gpui_kit_assets::IconName;
use strata_core::diff::LineKind;
use strata_core::session::DiffView;
use strata_core::{ChangeKind, FileChange, HeadInfo, RefKind};

use super::{DIFF_HEIGHT, PAGE, RepoView, one_line};
use crate::actions::{
    self, SelectFirst, SelectLast, SelectNext, SelectPageDown, SelectPageUp, SelectPrevious,
    StashChanges, ToggleFrameStats,
};
use crate::branches::CreateBranchFromHead;
use crate::data::{self, Badge, RowData, Source};
use crate::diff_view::{self, DiffColors, Row as DiffRow};
use crate::graph_canvas;
use crate::remote;
use crate::remote::{Command, FetchRemotes, PullBranch, PushBranch};
use crate::scrollbar::WithScrollbar as _;
use crate::search::{
    Field, FocusSearch, SEARCH_BAR, SearchDismiss, SearchNext, SearchPrevious, SearchSubmit,
};
use crate::settings::Settings;
use crate::style::{self, Design};
use crate::worktree_panel::MainMode;

const SIDEBAR_WIDTH: f32 = 248.0;
/// Width of the commit metadata column, matching the design's details pane.
const DETAILS_WIDTH: f32 = 320.0;
const AUTHOR_WIDTH: f32 = 150.0;
const DATE_WIDTH: f32 = 96.0;
const COMMIT_WIDTH: f32 = 64.0;

impl Render for RepoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let started = self.metrics.begin_frame();
        self.target_frame_ms = RepoView::target_frame_ms(window, cx);
        self.sync_title(window);

        // Snapshot publishes arrive in chunks; only the rows after the last
        // observed one are scanned for the graph column's width.
        let observed = self.source();
        if let Some(Source::Repo(snapshot)) = &observed {
            self.lanes.observe(&snapshot.commits);
        }

        if self.overlay && self.metrics.log_due() {
            let stats = self.metrics.stats();
            eprintln!(
                "frame {:.2} ms ({:.1} fps, p95 {:.2} ms) · build {:.2} ms",
                stats.frame_ms, stats.fps, stats.p95_ms, stats.render_ms
            );
        }

        if self.smoke {
            self.advance_smoke(cx);
        }

        let entity = cx.entity();
        let design = Design::new(cx.theme(), Settings::get(cx));
        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(design.background)
            .text_color(design.text)
            .text_size(px(design.ui))
            .track_focus(&self.focus)
            .key_context(actions::REPO_VIEW)
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| this.move_selection(-PAGE, cx)))
            .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| this.move_selection(PAGE, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.select_last(cx)))
            .on_action(cx.listener(|this, _: &ToggleFrameStats, _, cx| this.toggle_frame_stats(cx)))
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SearchDismiss, window, cx| this.dismiss(window, cx)))
            .on_action(cx.listener(|this, _: &SearchNext, _, cx| this.step_results(1, cx)))
            .on_action(cx.listener(|this, _: &SearchPrevious, _, cx| this.step_results(-1, cx)))
            .on_action(cx.listener(|this, _: &SearchSubmit, _, cx| this.submit_search(cx)))
            .on_action(
                cx.listener(|this, _: &StashChanges, window, cx| this.stash_changes(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CreateBranchFromHead, window, cx| {
                this.create_branch_from_head(window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchRemotes, window, cx| {
                this.remote_command(Command::Fetch, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PullBranch, window, cx| {
                this.remote_command(Command::Pull, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PushBranch, window, cx| {
                this.remote_command(Command::Push, window, cx)
            }));
        if let Some(font) = &design.ui_font {
            root = root.font_family(font.clone());
        }

        root = root.child(self.header(&design));
        if let Some((path, error)) = &self.error {
            root = root.child(self.error_screen(path, error, &design));
        } else {
            let sidebar = self.sidebar(cx, &design);
            // The worktree view owns the main area while it is open; a file
            // history belongs to the history view, so it is dropped instead of
            // coming back stale.
            if self.mode != MainMode::History {
                self.discard_file_history();
            }
            let main = if self.mode == MainMode::History && self.file_history.is_some() {
                self.file_history_view(entity.clone(), cx)
            } else {
                match self.mode {
                    MainMode::History => div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(self.commit_pane(entity.clone(), &design))
                        .child(self.diff_pane(&design, cx))
                        .into_any_element(),
                    MainMode::Worktree => self.worktree_view(entity.clone(), window, cx),
                }
            };
            root = root
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(sidebar)
                        .child(main),
                )
                .child(self.status_bar(&design));
        }

        if self.overlay {
            root = root.child(self.overlay_panel(&design));
        }

        self.metrics.end_frame(started);

        if self.overlay && !self.finished {
            window.request_animation_frame();
        }

        root
    }
}

impl RepoView {
    /// The title bar: the app mark, the repository and branch, the commit
    /// search, and the remote and window actions on the right.
    fn header(&self, design: &Design) -> AnyElement {
        let source = self.source();
        let head = source.as_ref().and_then(Source::head);
        let repo = match &self.fake {
            Some(_) => "synthetic".to_owned(),
            None => self.name.clone().unwrap_or_else(|| "strata".to_owned()),
        };
        let branch = match head {
            Some(HeadInfo::Branch { short_name, .. }) => short_name.clone(),
            Some(HeadInfo::Detached { .. }) => "detached".to_owned(),
            // Synthetic history has no `HeadInfo`, but its sidebar names the
            // branch it pretends to be on.
            None => source
                .as_ref()
                .and_then(|source| source.sidebar().head_branch)
                .unwrap_or_else(|| "loading".to_owned()),
        };
        let upstream = match &source {
            Some(Source::Repo(snapshot)) => head
                .and_then(|head| match head {
                    HeadInfo::Branch { short_name, .. } => snapshot.upstreams.get(short_name),
                    HeadInfo::Detached { .. } => None,
                })
                .cloned(),
            _ => None,
        };
        let track = upstream
            .as_ref()
            .and_then(crate::branches::upstream_label)
            .unwrap_or_default();

        let repository = div()
            .id("repository-button")
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(30.0))
            .px_2p5()
            .rounded(px(7.0))
            .cursor_pointer()
            .hover(|style| style.bg(design.field))
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(actions::OpenRepository), cx);
            })
            .child(icon(IconName::Folder, 15.0, design.text))
            .child(
                div()
                    .max_w(px(220.0))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(repo),
            )
            .child(icon(IconName::ChevronDown, 12.0, design.muted));

        let branch_chip = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(28.0))
            .px_2p5()
            .rounded(px(7.0))
            .border_1()
            .border_color(design.border)
            .bg(design.background)
            .child(glyph(style::glyph::BRANCH, 14.0, design.text))
            .child(div().max_w(px(240.0)).truncate().child(branch))
            .when(!track.is_empty(), |chip| {
                chip.child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(design.muted)
                        .child(track),
                )
            })
            .child(icon(IconName::ChevronDown, 12.0, design.muted));

        // The real field lives in the title bar when a repository can search;
        // `--fake` gets the same look without an editable field.
        let search_field: AnyElement = match self.search.input() {
            Some(input) => div()
                .flex_1()
                .min_w_0()
                .child(Input::new(input).appearance(false).bordered(false).small())
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child("Search commits, authors, hashes")
                .into_any_element(),
        };
        let search = div()
            .id("commit-search-field")
            .flex()
            .items_center()
            .gap(px(8.0))
            .w(px(420.0))
            .max_w_full()
            .h(px(28.0))
            .px_2p5()
            .rounded(px(7.0))
            .bg(design.field)
            .text_color(design.muted)
            .cursor_text()
            .child(icon(IconName::Search, 14.0, design.muted))
            .child(search_field)
            .child(kbd(search_key_label(), design));

        let separator = div()
            .flex_none()
            .w(px(1.0))
            .h(px(22.0))
            .mx_1p5()
            .bg(design.border);

        let nav = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .flex_none()
            .child(self.remote_buttons(
                source.as_ref(),
                &remote::Colors {
                    foreground: design.text,
                    muted_foreground: design.muted,
                    border: design.border,
                    accent: design.accent,
                    accent_foreground: design.on_accent,
                },
            ))
            .child(stash_button(design))
            .child(separator)
            .child(
                crate::ui::icon_button("terminal-placeholder", IconName::SquareTerminal)
                    .ghost()
                    .w(px(32.0))
                    .h(px(32.0))
                    .text_color(design.muted)
                    .tooltip("Terminal — coming soon")
                    .disabled(true),
            )
            .child(
                crate::ui::icon_button("open-settings", IconName::Settings)
                    .ghost()
                    .w(px(32.0))
                    .h(px(32.0))
                    .text_color(design.muted)
                    .tooltip("Settings")
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(actions::OpenSettings), cx);
                    }),
            );

        TitleBar::new()
            .h(px(44.0))
            .bg(design.chrome)
            .border_color(design.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    .child(style::app_mark(20.0))
                    .child(repository)
                    .child(branch_chip),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .justify_center()
                    .px_4()
                    .child(search),
            )
            .child(nav)
            .into_any_element()
    }

    fn sidebar(&self, cx: &mut Context<Self>, design: &Design) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .gap(px(14.0))
            .py_3()
            .px_2()
            .overflow_hidden()
            .bg(design.sidebar)
            .border_r_1()
            .border_color(design.border)
            .child(self.nav_section(cx, design))
            .child(self.ref_sections(design))
            .into_any_element()
    }

    /// The two view entries at the top of the sidebar.
    fn nav_section(&self, cx: &mut Context<Self>, design: &Design) -> AnyElement {
        let (staged, unstaged) = self
            .source()
            .map_or((0, 0), |source| source.sidebar().working_tree);
        let changes = staged + unstaged;
        let worktree = self.mode == MainMode::Worktree;

        let working_tree = div()
            .id("working-tree-entry")
            .flex()
            .items_center()
            .gap_2()
            .h(px(30.0))
            .px_2()
            .rounded(px(7.0))
            .text_size(px(design.ui))
            .cursor_pointer()
            .when(worktree, |row| row.bg(design.selected))
            .hover(|style| style.bg(design.selected))
            .on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.open_worktree(window, cx)),
            )
            .child(icon(IconName::FileText, 15.0, design.text))
            .child(div().flex_1().min_w_0().truncate().child("Working tree"))
            .when(changes > 0, |row| {
                row.child(
                    div()
                        .flex_none()
                        .px(px(7.0))
                        .rounded_full()
                        .bg(design.accent_soft)
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(design.accent_text)
                        .child(changes.to_string()),
                )
            });

        let history = div()
            .id("history-entry")
            .flex()
            .items_center()
            .gap_2()
            .h(px(30.0))
            .px_2()
            .rounded(px(7.0))
            .text_size(px(design.ui))
            .cursor_pointer()
            .when(!worktree, |row| row.bg(design.selected))
            .hover(|style| style.bg(design.selected))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                if this.mode != MainMode::History {
                    this.close_worktree(window, cx);
                }
            }))
            .child(glyph(
                style::glyph::COMMIT,
                15.0,
                if worktree {
                    design.text
                } else {
                    design.accent_text
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(!worktree, |row| row.font_weight(FontWeight::MEDIUM))
                    .child("History"),
            );

        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(working_tree)
            .child(history)
            .into_any_element()
    }

    /// Branches, remotes, tags and stashes below the view entries.
    fn ref_sections(&self, design: &Design) -> AnyElement {
        let Some(source) = self.source() else {
            return div().into_any_element();
        };
        let data = source.sidebar();
        let upstreams = match &source {
            Source::Repo(snapshot) => snapshot.upstreams.as_ref().clone(),
            Source::Fake(_) => Default::default(),
        };
        let colors = crate::branches::Colors {
            design: design.clone(),
            sidebar_foreground: design.text,
            muted_foreground: design.muted,
            border: design.border,
            foreground: design.text,
            danger: design.remove_text,
            overlay: design.panel,
        };
        self.branches.render(
            &data,
            &upstreams,
            self.session.is_some(),
            self.is_busy(),
            &colors,
            &self.entity,
        )
    }

    fn commit_pane(&self, entity: gpui::Entity<Self>, design: &Design) -> AnyElement {
        let Some(source) = self.source() else {
            return empty_pane("No repository", design);
        };
        let len = source.len();
        if len == 0 {
            return empty_pane("No commits yet", design);
        }

        let now = style::now_epoch();
        let selected = self.selected;
        let lanes = self.lanes.max();
        let graph_width = graph_canvas::column_width(lanes);
        let row_design = design.clone();
        let focus = self.focus.clone();
        // Branch creation from a commit row is only meaningful for a real
        // repository, and only when no operation is in flight.
        let branch_menu = self.session.is_some() && !self.is_busy();
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
                    let element = if filtered {
                        // The graph is hidden while filtered: spaced-out rows
                        // would draw lanes that never existed between them.
                        filtered_commit_row(
                            id,
                            &row,
                            selected == Some(index),
                            graph_width,
                            &date,
                            &row_design,
                        )
                    } else {
                        commit_row(id, &row, selected == Some(index), lanes, &date, &row_design)
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
                    let Some(commit) = source_for_list.commit_id(index).filter(|_| branch_menu)
                    else {
                        return element.into_any_element();
                    };
                    let entity = entity.clone();
                    element
                        .context_menu(move |menu, _window, _cx| {
                            let point = commit.to_string();
                            let from = format!("from commit {}", data::short_oid(commit));
                            let open = {
                                let entity = entity.clone();
                                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                    let point = point.clone();
                                    let from = from.clone();
                                    entity.update(cx, |this, cx| {
                                        this.open_create_dialog(Some(point), from, window, cx);
                                    });
                                }
                            };
                            menu.item(PopupMenuItem::new("Create branch here…").on_click(open))
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
        column = column.child(commit_header(design, graph_width));
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

    fn diff_pane(&self, design: &Design, cx: &mut App) -> AnyElement {
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

        let right = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(self.diff_header(design, rows.len()))
            .child(if self.fake.is_some() {
                self.diff_rows_list(rows, None, design, cx)
            } else {
                self.real_diff_body(design, cx)
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

    /// The commit's summary, author, id and parents at the top of the details
    /// column. Empty while nothing is selected.
    fn commit_details(
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
    fn files_summary(&self, design: &Design) -> AnyElement {
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

    fn file_list(&self, design: &Design) -> AnyElement {
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

    fn status_bar(&self, design: &Design) -> AnyElement {
        let command = self
            .last_command
            .clone()
            .unwrap_or_else(|| "no git commands yet".to_owned());
        let error = self.status_error.clone();
        let operation = self.remote_status(&remote::Colors {
            foreground: design.text,
            muted_foreground: design.muted,
            border: design.border,
            accent: design.accent,
            accent_foreground: design.on_accent,
        });

        let mut bar = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(26.0))
            .flex_none()
            .px_3()
            .gap(px(10.0))
            .bg(design.chrome)
            .border_t_1()
            .border_color(design.border)
            .text_size(px(design.small()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex_none()
                            .w(px(7.0))
                            .h(px(7.0))
                            .rounded_full()
                            .bg(design.add_text),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(design.mono.clone())
                            .text_color(design.muted)
                            .child(command),
                    ),
            );
        // Only while a fetch, pull or push runs: the bar otherwise keeps its
        // command log and last-error slots.
        if let Some(operation) = operation {
            bar = bar.child(operation);
        }
        bar.child(
            div()
                .max_w(px(720.0))
                .truncate()
                .text_color(if error.is_some() {
                    design.remove_text
                } else {
                    design.muted
                })
                .child(error.unwrap_or_else(|| "Watching for changes".to_owned())),
        )
        .into_any_element()
    }

    fn overlay_panel(&self, design: &Design) -> AnyElement {
        let stats = self.metrics.stats();
        let target = self.target_frame_ms;
        let target_hz = if target > 0.0 { 1000.0 / target } else { 60.0 };
        let commits = self.commit_count();

        let mut panel = div()
            .absolute()
            .bottom(px(10.0))
            .right(px(12.0))
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_md()
            .bg(design.panel)
            .border_1()
            .border_color(design.border)
            .text_size(px(11.0))
            .text_color(design.text)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                "{:.2} ms/frame mean · p95 {:.2} ms · {:.1} fps",
                stats.frame_ms, stats.p95_ms, stats.fps
            )))
            .child(format!(
                "render {:.2} ms · {} samples · {} frames",
                stats.render_ms,
                stats.samples,
                self.metrics.frames()
            ))
            .child(format!(
                "display target {:.2} ms ({:.0} Hz)",
                target, target_hz
            ))
            .child(format!(
                "{commits} commits · selected {}",
                self.selected
                    .map_or_else(|| "none".to_owned(), |index| index.to_string())
            ));

        if stats.samples > 0 {
            panel = panel.child(div().text_color(design.muted).child(
                if stats.frame_ms <= target * 1.05 {
                    format!("smooth ({target_hz:.0} fps target)")
                } else {
                    format!("below {target_hz:.0} fps target")
                },
            ));
        }

        panel.into_any_element()
    }

    fn error_screen(&self, path: &str, error: &str, design: &Design) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .flex_1()
            .p_8()
            .child(
                div()
                    .text_size(px(18.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(design.remove_text)
                    .child("Could not open repository"),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(design.muted)
                    .child(path.to_owned()),
            )
            .child(
                div()
                    .max_w(px(760.0))
                    .text_size(px(12.0))
                    .child(error.to_owned()),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(design.muted)
                    .child("usage: strata [PATH] · strata --fake [N]"),
            )
            .into_any_element()
    }
}

/// The column headings above the commit list.
fn commit_header(design: &Design, graph_width: f32) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .h(px(28.0))
        .flex_none()
        .pl(px(17.0))
        .pr(px(13.0))
        .border_b_1()
        .border_color(design.border)
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(design.muted)
        .child(div().w(px(graph_width)).flex_none().child("Graph"))
        .child(div().flex_1().min_w_0().child("Description"))
        .child(div().w(px(AUTHOR_WIDTH)).flex_none().child("Author"))
        .child(div().w(px(DATE_WIDTH)).flex_none().child("Date"))
        .child(div().w(px(COMMIT_WIDTH)).flex_none().child("Commit"))
        .into_any_element()
}

fn commit_row(
    id: impl Into<gpui::ElementId>,
    data: &RowData<'_>,
    selected: bool,
    lanes: u16,
    date: &str,
    design: &Design,
) -> gpui::Stateful<Div> {
    let avatar_color = design.avatar_color(&data.email);
    let initials = style::initials(&data.author);
    crate::ui::list_row(id, crate::ui::RowState::NORMAL.selected(selected), design)
        .gap_3()
        .h(px(graph_canvas::ROW_HEIGHT))
        .w_full()
        .mx(px(0.0))
        .px_3()
        .child(graph_canvas::graph_column(data.graph, design.lane, lanes))
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

/// A commit row in the filtered result set. The graph column keeps its width so
/// the columns stay aligned, but nothing is painted in it: the matching commits
/// are not adjacent in history, so lanes between them would be fiction.
fn filtered_commit_row(
    id: impl Into<gpui::ElementId>,
    data: &RowData<'_>,
    selected: bool,
    graph_width: f32,
    date: &str,
    design: &Design,
) -> gpui::Stateful<Div> {
    let avatar_color = design.avatar_color(&data.email);
    let initials = style::initials(&data.author);
    crate::ui::list_row(id, crate::ui::RowState::NORMAL.selected(selected), design)
        .gap_3()
        .h(px(graph_canvas::ROW_HEIGHT))
        .w_full()
        .mx(px(0.0))
        .px_3()
        .child(div().w(px(graph_width)).flex_none())
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

fn message(text: &str, color: Hsla, design: &Design) -> AnyElement {
    crate::ui::message(text, color, design).into_any_element()
}

fn icon(name: IconName, size: f32, color: Hsla) -> Icon {
    Icon::new(name).with_size(px(size)).text_color(color)
}

fn glyph(data: &'static [u8], size: f32, color: Hsla) -> Icon {
    style::glyph_icon(data, size, color)
}

/// The title bar's Stash action: the same icon-over-label shape as the remote
/// buttons, but it dispatches an action instead of running a git command here.
fn stash_button(design: &Design) -> AnyElement {
    crate::ui::content_button("stash-button")
        .ghost()
        .w(px(52.0))
        .h(px(40.0))
        .text_size(px(10.0))
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(actions::StashChanges), cx);
        })
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(2.0))
                .child(glyph(style::glyph::ARCHIVE, 16.0, design.text))
                .child("Stash"),
        )
        .into_any_element()
}

/// A keyboard hint chip, matching the design's `Ctrl F`.
fn kbd(label: &'static str, design: &Design) -> AnyElement {
    crate::ui::kbd(label, design)
}

#[cfg(not(target_os = "macos"))]
fn search_key_label() -> &'static str {
    "Ctrl F"
}

#[cfg(target_os = "macos")]
fn search_key_label() -> &'static str {
    "⌘F"
}
