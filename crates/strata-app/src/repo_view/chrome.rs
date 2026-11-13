//! Window chrome and the commit/diff lists.
//!
//! Everything here renders from [`RepoView`]'s state; nothing in this module
//! calls git or the session. The commit list is built from [`Source`], so the
//! synthetic benchmark and a real repository share the same row renderer.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Div, FontWeight, Hsla, InteractiveElement, ParentElement, Render,
    SharedString, Styled, Window, div, px, uniform_list,
};
use gpui_component::{ActiveTheme, Theme, TitleBar, window_border};
use strata_core::diff::LineKind;
use strata_core::highlight::LineHighlights;
use strata_core::session::DiffView;
use strata_core::{ChangeKind, HeadInfo, RefKind};

use super::{DIFF_HEIGHT, PAGE, RepoView};
use crate::actions::{
    self, SelectFirst, SelectLast, SelectNext, SelectPageDown, SelectPageUp, SelectPrevious,
    ToggleFrameStats,
};
use crate::data::{self, Badge, RowData, Source};
use crate::diff_view::{self, DiffColors, Row as DiffRow};
use crate::graph_canvas;

const SIDEBAR_WIDTH: f32 = 240.0;
const FILE_LIST_WIDTH: f32 = 260.0;
const SIDEBAR_ENTRIES: usize = 6;

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
        let palette = Palette::from_theme(cx.theme());
        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(palette.background)
            .text_color(palette.foreground)
            .track_focus(&self.focus)
            .key_context(actions::REPO_VIEW)
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| this.move_selection(-PAGE, cx)))
            .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| this.move_selection(PAGE, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.select_last(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleFrameStats, _, cx| this.toggle_frame_stats(cx)),
            );

        root = root.child(self.header(&palette));
        if let Some((path, error)) = &self.error {
            root = root.child(self.error_screen(path, error, &palette));
        } else {
            root = root
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(self.sidebar(&palette))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .min_h_0()
                                .child(self.commit_pane(entity.clone(), &palette))
                                .child(self.diff_pane(entity, cx.theme(), &palette)),
                        ),
                )
                .child(self.status_bar(&palette));
        }

        if self.overlay {
            root = root.child(self.overlay_panel(&palette));
        }

        self.metrics.end_frame(started);

        if self.overlay && !self.finished {
            window.request_animation_frame();
        }

        // Client-side decorations need a drawn border, shadow and resize
        // edges on Linux; elsewhere this is a plain full-size wrapper.
        window_border().child(root)
    }
}

impl RepoView {
    fn header(&self, palette: &Palette) -> AnyElement {
        let source = self.source();
        let count = source.as_ref().map_or(0, Source::len);
        let subtitle = if self.fake.is_some() {
            format!("synthetic — {count} commits")
        } else if let Some(name) = &self.name {
            let branch = match source.as_ref().and_then(Source::head) {
                Some(HeadInfo::Branch { short_name, .. }) => format!("on {short_name}"),
                Some(HeadInfo::Detached { .. }) => "detached".to_owned(),
                None => "loading".to_owned(),
            };
            format!("{name} — {branch} — {count} commits")
        } else {
            "no repository".to_owned()
        };

        // `TitleBar` owns dragging, double-click and (on Windows and Linux)
        // the window controls; the app only supplies its content. On macOS
        // it leaves room for the traffic lights itself.
        TitleBar::new()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Strata"),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(palette.muted_foreground)
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .pr_3()
                    .text_size(px(11.0))
                    .text_color(palette.muted_foreground)
                    .child("↑/↓ select · F12 frame stats"),
            )
            .into_any_element()
    }

    fn sidebar(&self, palette: &Palette) -> AnyElement {
        let data = self.source().map(|source| source.sidebar());
        let (staged, unstaged) = data.as_ref().map_or((0, 0), |data| data.working_tree);

        let mut column = div()
            .flex()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .py_2()
            .overflow_hidden()
            .bg(palette.sidebar)
            .border_r_1()
            .border_color(palette.border);

        column = column
            .child(section_title("WORKING TREE", None, palette))
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .text_size(px(11.0))
                    .text_color(if staged + unstaged == 0 {
                        palette.muted_foreground
                    } else {
                        palette.foreground
                    })
                    .child(format!(
                        "Working tree: {staged} staged, {unstaged} unstaged"
                    )),
            );

        if let Some(data) = &data {
            column = column
                .child(sidebar_section(
                    "LOCAL BRANCHES",
                    &data.local,
                    data.head_branch.as_deref(),
                    palette,
                ))
                .child(sidebar_section(
                    "REMOTE BRANCHES",
                    &data.remote,
                    None,
                    palette,
                ))
                .child(sidebar_section("TAGS", &data.tags, None, palette))
                .child(sidebar_section("STASHES", &data.stashes, None, palette));
        }

        column.into_any_element()
    }

    fn commit_pane(&self, entity: gpui::Entity<Self>, palette: &Palette) -> AnyElement {
        let Some(source) = self.source() else {
            return empty_pane("No repository", palette);
        };
        let len = source.len();
        if len == 0 {
            return empty_pane("No commits yet", palette);
        }

        let selected = self.selected;
        let lanes = self.lanes.max();
        let row_palette = palette.clone();
        let focus = self.focus.clone();
        let list = uniform_list("commits", len, move |range, _window, _cx| {
            range
                .map(|index| {
                    let entity = entity.clone();
                    let focus = focus.clone();
                    let Some(row) = source.row(index) else {
                        return div().into_any_element();
                    };
                    commit_row(&row, selected == Some(index), lanes, &row_palette)
                        .id(("commit-row", index))
                        .cursor_pointer()
                        .on_click(move |_event, window, cx| {
                            entity.update(cx, |this, cx| {
                                this.jump(index, cx);
                                window.focus(&focus, cx);
                            });
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.commit_scroll)
        .flex_1();

        let mut column = div().flex().flex_col().flex_1().min_h_0().min_w_0();
        column = column.child(list);
        if !self.source().is_some_and(|source| source.complete()) {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .h(px(20.0))
                    .flex_none()
                    .px_3()
                    .text_size(px(10.0))
                    .text_color(palette.muted_foreground)
                    .child(format!("loading {len} commits")),
            );
        }
        column.into_any_element()
    }

    fn diff_pane(
        &self,
        entity: gpui::Entity<Self>,
        theme: &Theme,
        palette: &Palette,
    ) -> AnyElement {
        let rows = if self.fake.is_some() {
            self.selected
                .and_then(|index| self.fake_diffs.get(index % self.fake_diffs.len().max(1)))
                .map_or_else(|| Rc::new(Vec::new()), Rc::clone)
        } else {
            Rc::clone(&self.diff_rows)
        };

        let header = self.diff_header(palette, rows.len());
        let body = if self.fake.is_some() {
            self.diff_rows_list(rows, None, theme, palette)
        } else {
            self.real_diff_body(entity, theme, palette)
        };

        div()
            .flex()
            .flex_col()
            .w_full()
            .h(px(DIFF_HEIGHT))
            .flex_none()
            .min_h_0()
            .bg(palette.background)
            .border_t_1()
            .border_color(palette.border)
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn diff_header(&self, palette: &Palette, row_count: usize) -> AnyElement {
        let source = self.source();
        let commit = self
            .selected
            .and_then(|index| source.as_ref().and_then(|source| source.row(index)))
            .map(|row| row.id.to_string());

        let (path, kind) = match (&self.fake, &self.files, self.selected_file) {
            _ if self.fake.is_some() => ("synthetic diff".to_owned(), None),
            (_, Some(files), Some(index)) => match files.get(index) {
                Some(change) => (change.path.clone(), Some(change.kind)),
                None => ("no file selected".to_owned(), None),
            },
            _ => ("no file selected".to_owned(), None),
        };

        let right = if self.fake.is_some() {
            format!("synthetic — {row_count} rows")
        } else {
            let files = self.files.as_ref().map_or(0, Vec::len);
            match commit {
                Some(commit) => format!("commit {commit} · {files} changed files"),
                None => format!("{files} changed files"),
            }
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(30.0))
            .flex_none()
            .px_3()
            .gap_3()
            .bg(palette.title_bar)
            .border_b_1()
            .border_color(palette.border)
            .text_size(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(match kind {
                        Some(kind) => change_badge(kind, palette),
                        None => div().into_any_element(),
                    })
                    .child(div().truncate().child(path)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(palette.muted_foreground)
                    .child(right),
            )
            .into_any_element()
    }

    fn real_diff_body(
        &self,
        entity: gpui::Entity<Self>,
        theme: &Theme,
        palette: &Palette,
    ) -> AnyElement {
        if let Some(error) = &self.files_error {
            return message(error, palette.danger, palette);
        }
        let Some(files) = &self.files else {
            return message("reading changed files…", palette.muted_foreground, palette);
        };
        if files.is_empty() {
            return message(
                "No changes in this commit",
                palette.muted_foreground,
                palette,
            );
        }

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(self.file_list(entity, files, palette))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(self.diff_content(theme, palette)),
            )
            .into_any_element()
    }

    fn file_list(
        &self,
        entity: gpui::Entity<Self>,
        files: &[strata_core::FileChange],
        palette: &Palette,
    ) -> AnyElement {
        let selected = self.selected_file;
        let list_palette = palette.clone();
        let count = files.len();
        let paths: Vec<(String, ChangeKind)> = files
            .iter()
            .map(|change| (change.path.clone(), change.kind))
            .collect();

        uniform_list("changed-files", count, move |range, _window, _cx| {
            range
                .map(|index| {
                    let entity = entity.clone();
                    let (path, kind) = &paths[index];
                    let mut row = div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(20.0))
                        .w_full()
                        .px_2()
                        .text_size(px(11.0))
                        .child(change_badge(*kind, &list_palette))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(if selected == Some(index) {
                                    list_palette.accent
                                } else {
                                    list_palette.foreground
                                })
                                .child(path.clone()),
                        );
                    if selected == Some(index) {
                        row = row.bg(list_palette.selection);
                    }
                    row.cursor_pointer()
                        .id(("changed-file", index))
                        .on_click(move |_event, _window, cx| {
                            entity.update(cx, |this, cx| this.select_file(index, cx));
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.file_scroll)
        .w(px(FILE_LIST_WIDTH))
        .flex_none()
        .border_r_1()
        .border_color(palette.border)
        .into_any_element()
    }

    fn diff_content(&self, theme: &Theme, palette: &Palette) -> AnyElement {
        if let Some(error) = &self.diff_error {
            return message(error, palette.danger, palette);
        }
        let Some(view) = self.diff.view().cloned() else {
            return message("computing diff…", palette.muted_foreground, palette);
        };
        if view.diff.binary {
            return message("Binary file", palette.muted_foreground, palette);
        }
        if self.diff_rows.is_empty() {
            return message("No textual changes", palette.muted_foreground, palette);
        }
        self.diff_rows_list(Rc::clone(&self.diff_rows), Some(view), theme, palette)
    }

    fn diff_rows_list(
        &self,
        rows: Rc<Vec<DiffRow>>,
        view: Option<std::sync::Arc<DiffView>>,
        theme: &Theme,
        palette: &Palette,
    ) -> AnyElement {
        let colors = DiffColors::from_theme(theme);
        let mono = palette.mono.clone();
        uniform_list("diff", rows.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    let row = &rows[index];
                    let highlights = view.as_ref().and_then(|view| line_highlights(row, view));
                    diff_view::render_row(row, &colors, &mono, highlights)
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.diff_scroll)
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn status_bar(&self, palette: &Palette) -> AnyElement {
        let command = self
            .last_command
            .clone()
            .unwrap_or_else(|| "no git commands yet".to_owned());
        let error = self.status_error.clone();

        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(24.0))
            .flex_none()
            .px_3()
            .gap_4()
            .bg(palette.title_bar)
            .border_t_1()
            .border_color(palette.border)
            .font_family(palette.mono.clone())
            .text_size(px(11.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(palette.muted_foreground)
                    .child(command),
            )
            .child(
                div()
                    .max_w(px(720.0))
                    .truncate()
                    .text_color(if error.is_some() {
                        palette.danger
                    } else {
                        palette.muted_foreground
                    })
                    .child(error.unwrap_or_else(|| "ready".to_owned())),
            )
            .into_any_element()
    }

    fn overlay_panel(&self, palette: &Palette) -> AnyElement {
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
            .bg(palette.overlay)
            .border_1()
            .border_color(palette.border)
            .text_size(px(11.0))
            .text_color(palette.foreground)
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
            panel = panel.child(div().text_color(palette.muted_foreground).child(
                if stats.frame_ms <= target * 1.05 {
                    format!("smooth ({target_hz:.0} fps target)")
                } else {
                    format!("below {target_hz:.0} fps target")
                },
            ));
        }

        panel.into_any_element()
    }

    fn error_screen(&self, path: &str, error: &str, palette: &Palette) -> AnyElement {
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
                    .text_color(palette.danger)
                    .child("Could not open repository"),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(palette.muted_foreground)
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
                    .text_color(palette.muted_foreground)
                    .child("usage: strata [PATH] · strata --fake [N]"),
            )
            .into_any_element()
    }
}

fn commit_row(data: &RowData<'_>, selected: bool, lanes: u16, palette: &Palette) -> Div {
    let mut row = div()
        .flex()
        .items_center()
        .h(px(graph_canvas::ROW_HEIGHT))
        .w_full()
        .text_size(px(12.0))
        .child(graph_canvas::graph_column(data.graph, palette.lane, lanes));

    if !data.badges.is_empty() {
        let mut badges = div().flex().items_center().gap_1().flex_none().pl_1();
        for badge in &data.badges {
            badges = badges.child(badge_chip(badge, palette));
        }
        row = row.child(badges);
    }

    row = row
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .pl_2()
                .text_color(palette.foreground)
                .child(data.summary.to_string()),
        )
        .child(
            div()
                .w(px(150.0))
                .flex_none()
                .truncate()
                .text_size(px(11.0))
                .text_color(palette.muted_foreground)
                .child(data.author.to_string()),
        )
        .child(
            div()
                .w(px(120.0))
                .flex_none()
                .text_size(px(11.0))
                .text_color(palette.muted_foreground)
                .child(data::format_time(data.time)),
        )
        .child(
            div()
                .w(px(70.0))
                .flex_none()
                .font_family(palette.mono.clone())
                .text_size(px(11.0))
                .text_color(palette.muted_foreground)
                .child(data.id.to_string()),
        );

    if selected {
        row = row.bg(palette.selection);
    }
    row
}

fn badge_chip(badge: &Badge<'_>, palette: &Palette) -> Div {
    let (background, foreground) = match badge.kind {
        RefKind::LocalBranch if badge.head => (palette.accent, palette.accent_foreground),
        RefKind::LocalBranch => (palette.muted, palette.foreground),
        RefKind::RemoteBranch => (palette.muted, palette.muted_foreground),
        RefKind::Tag => (palette.warning.opacity(0.24), palette.warning),
    };

    div()
        .flex()
        .items_center()
        .flex_none()
        .max_w(px(150.0))
        .truncate()
        .px_1()
        .rounded_sm()
        .bg(background)
        .text_size(px(9.5))
        .font_weight(if badge.head {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        })
        .text_color(foreground)
        .child(badge.label.to_string())
}

fn change_badge(kind: ChangeKind, palette: &Palette) -> AnyElement {
    let (marker, color) = match kind {
        ChangeKind::Added | ChangeKind::Untracked => ("A", palette.success),
        ChangeKind::Modified => ("M", palette.warning),
        ChangeKind::Deleted => ("D", palette.danger),
        ChangeKind::Renamed => ("R", palette.accent),
        ChangeKind::Copied => ("C", palette.info),
        ChangeKind::TypeChanged => ("T", palette.warning),
        ChangeKind::Conflicted => ("!", palette.danger),
    };
    div()
        .w(px(14.0))
        .flex_none()
        .text_size(px(10.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(marker)
        .into_any_element()
}

fn section_title(title: &str, count: Option<usize>, palette: &Palette) -> Div {
    let label = match count {
        Some(count) => format!("{title} ({count})"),
        None => title.to_owned(),
    };
    div()
        .px_3()
        .pt_2()
        .pb_1()
        .text_size(px(10.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(palette.muted_foreground)
        .child(label)
}

fn sidebar_section(title: &str, entries: &[String], head: Option<&str>, palette: &Palette) -> Div {
    let mut section =
        div()
            .flex()
            .flex_col()
            .pb_1()
            .child(section_title(title, Some(entries.len()), palette));

    if entries.is_empty() {
        section = section.child(
            div()
                .px_3()
                .h(px(22.0))
                .text_size(px(11.0))
                .text_color(palette.muted_foreground)
                .child("none"),
        );
    }

    for name in entries.iter().take(SIDEBAR_ENTRIES) {
        let active = head == Some(name.as_str());
        let mut row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .h(px(22.0))
            .px_3()
            .text_size(px(11.0))
            .text_color(if active {
                palette.sidebar_accent_foreground
            } else {
                palette.sidebar_foreground
            });
        if active {
            row = row.bg(palette.sidebar_accent);
        }
        row = row.child(div().flex_1().min_w_0().truncate().child(name.clone()));
        if active {
            row = row.child(
                div()
                    .flex_none()
                    .text_size(px(9.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("HEAD"),
            );
        }
        section = section.child(row);
    }

    if entries.len() > SIDEBAR_ENTRIES {
        section = section.child(
            div()
                .px_3()
                .h(px(20.0))
                .text_size(px(10.0))
                .text_color(palette.muted_foreground)
                .child(format!("+{} more", entries.len() - SIDEBAR_ENTRIES)),
        );
    }

    section
}

fn empty_pane(message: &str, palette: &Palette) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(palette.muted_foreground)
        .child(message.to_owned())
        .into_any_element()
}

fn message(text: &str, color: Hsla, palette: &Palette) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .p_4()
        .text_size(px(12.0))
        .text_color(color)
        .bg(palette.background)
        .child(text.to_owned())
        .into_any_element()
}

/// The highlights for the side of the line that is drawn. Context lines exist
/// on both sides, so the new side is used and the old one is only a fallback.
fn line_highlights<'a>(row: &DiffRow, view: &'a DiffView) -> Option<&'a LineHighlights> {
    let DiffRow::Line(line) = row else {
        return None;
    };
    let (highlights, number) = match line.kind {
        LineKind::Removed => (&view.old_highlights, line.old_lineno),
        LineKind::Added => (&view.new_highlights, line.new_lineno),
        LineKind::Context => (&view.new_highlights, line.new_lineno.or(line.old_lineno)),
    };
    highlights.get(number?.checked_sub(1)? as usize)
}

#[derive(Clone)]
struct Palette {
    background: Hsla,
    foreground: Hsla,
    muted: Hsla,
    muted_foreground: Hsla,
    border: Hsla,
    sidebar: Hsla,
    sidebar_foreground: Hsla,
    sidebar_accent: Hsla,
    sidebar_accent_foreground: Hsla,
    selection: Hsla,
    title_bar: Hsla,
    overlay: Hsla,
    accent: Hsla,
    accent_foreground: Hsla,
    danger: Hsla,
    warning: Hsla,
    success: Hsla,
    info: Hsla,
    mono: SharedString,
    lane: [Hsla; 8],
}

impl Palette {
    fn from_theme(theme: &Theme) -> Self {
        let dark = theme.is_dark();
        let lane = if dark {
            [
                theme.red,
                theme.green,
                theme.blue,
                theme.yellow,
                theme.magenta,
                theme.cyan,
                theme.primary,
                theme.warning,
            ]
        } else {
            [
                theme.red_light,
                theme.green_light,
                theme.blue_light,
                theme.yellow_light,
                theme.magenta_light,
                theme.cyan_light,
                theme.primary,
                theme.warning,
            ]
        };

        Self {
            background: theme.background,
            foreground: theme.foreground,
            muted: theme.muted,
            muted_foreground: theme.muted_foreground,
            border: theme.border,
            sidebar: theme.sidebar,
            sidebar_foreground: theme.sidebar_foreground,
            sidebar_accent: theme.sidebar_accent,
            sidebar_accent_foreground: theme.sidebar_accent_foreground,
            selection: theme.list_active,
            title_bar: theme.title_bar,
            overlay: theme.popover,
            accent: theme.primary,
            accent_foreground: theme.primary_foreground,
            danger: theme.danger,
            warning: theme.warning,
            success: theme.success,
            info: theme.info,
            mono: theme.mono_font_family.clone(),
            lane,
        }
    }
}
