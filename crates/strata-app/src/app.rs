//! The Strata phase-0 window: sidebar, virtualised commit list, diff pane.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, FocusHandle, FontWeight, Hsla, InteractiveElement, KeyDownEvent,
    ParentElement, Render, ScrollStrategy, SharedString, Subscription, UniformListScrollHandle,
    Window, div, px, uniform_list,
};
use gpui_component::{ActiveTheme, Theme};
use strata_core::graph::GraphRow;

use crate::diff_view::{self, Row as DiffRow};
use crate::fake::{self, FakeRepo};
use crate::graph_canvas;
use crate::metrics::FrameMetrics;

const BRANCHES: [(&str, &str); 10] = [
    ("main", "HEAD"),
    ("t3/gpui-graph-validation", ""),
    ("graph/lane-layout", ""),
    ("diff/word-emphasis", ""),
    ("repo/status-watch", ""),
    ("ui/theme-pass", ""),
    ("fix/windows-paths", ""),
    ("git_cli/spawn-env", ""),
    ("perf/scrolling", ""),
    ("docs/phase-0", ""),
];

const DIFF_HEIGHT: f32 = 360.0;

pub struct StrataApp {
    repo: Rc<FakeRepo>,
    /// A small pool of pre-flattened diffs so selecting a row visibly changes
    /// the diff without rebuilding one per frame.
    diffs: Rc<Vec<Rc<Vec<DiffRow>>>>,
    selected: usize,
    commit_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    focus: FocusHandle,
    overlay: bool,
    smoke: bool,
    finished: bool,
    metrics: FrameMetrics,
    _appearance: Subscription,
}

impl StrataApp {
    pub fn new(repo: Rc<FakeRepo>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let diffs = Rc::new(
            (0..8u32)
                .map(|seed| Rc::new(diff_view::flatten(&fake::sample_diff(seed))))
                .collect::<Vec<_>>(),
        );

        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let appearance = window.observe_window_appearance(|window, cx| {
            Theme::sync_system_appearance(Some(window), cx)
        });
        let smoke = std::env::var_os("STRATA_SMOKE").is_some();

        Self {
            repo,
            diffs,
            selected: 0,
            commit_scroll: UniformListScrollHandle::default(),
            diff_scroll: UniformListScrollHandle::default(),
            focus,
            overlay: smoke,
            smoke,
            finished: false,
            metrics: FrameMetrics::default(),
            _appearance: appearance,
        }
    }

    pub fn commit_count(&self) -> usize {
        self.repo.commits.len()
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        let last = self.repo.commits.len().saturating_sub(1);
        let index = index.min(last);
        if index == self.selected {
            return;
        }
        self.selected = index;
        self.diff_scroll = UniformListScrollHandle::default();
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let last = self.repo.commits.len().saturating_sub(1);
        let next = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        self.commit_scroll
            .scroll_to_item(next, ScrollStrategy::Center);
        self.select(next, cx);
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "up" | "k" => self.move_selection(-1, cx),
            "down" | "j" => self.move_selection(1, cx),
            "pageup" => self.move_selection(-15, cx),
            "pagedown" => self.move_selection(15, cx),
            "home" => self.select(0, cx),
            "end" => self.select(self.repo.commits.len().saturating_sub(1), cx),
            "f12" => {
                self.overlay = !self.overlay;
                cx.notify();
            }
            _ => {}
        }
    }

    fn header(&self, palette: &Palette) -> AnyElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(40.0))
            .flex_none()
            .px_3()
            .bg(palette.title_bar)
            .border_b_1()
            .border_color(palette.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Strata"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(palette.muted_foreground)
                            .child(format!(
                                "phase 0 — {} synthetic commits",
                                self.commit_count()
                            )),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(palette.muted_foreground)
                    .child("↑/↓ select · F12 frame stats"),
            )
            .into_any_element()
    }

    fn sidebar(&self, palette: &Palette) -> AnyElement {
        let mut column = div()
            .flex()
            .flex_col()
            .w(px(220.0))
            .h_full()
            .flex_none()
            .py_2()
            .bg(palette.sidebar)
            .border_r_1()
            .border_color(palette.border);

        column = column.child(
            div()
                .px_3()
                .py_1()
                .text_size(px(10.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(palette.muted)
                .child("BRANCHES"),
        );

        for (index, (name, badge)) in BRANCHES.iter().enumerate() {
            let active = index == 0;
            let mut row = div()
                .flex()
                .items_center()
                .justify_between()
                .h(px(26.0))
                .px_3()
                .text_size(px(12.0))
                .text_color(if active {
                    palette.sidebar_accent_foreground
                } else {
                    palette.sidebar_foreground
                });
            if active {
                row = row.bg(palette.sidebar_accent);
            }
            row = row.child(div().truncate().child(*name));
            if !badge.is_empty() {
                row = row.child(
                    div()
                        .text_size(px(9.0))
                        .text_color(palette.muted_foreground)
                        .child(*badge),
                );
            }
            column = column.child(row);
        }

        column
            .child(div().flex_1())
            .child(
                div()
                    .px_3()
                    .text_size(px(10.0))
                    .text_color(palette.muted_foreground)
                    .child("working tree clean"),
            )
            .into_any_element()
    }

    fn commit_list(&self, entity: Entity<Self>, palette: &Palette) -> AnyElement {
        let repo = self.repo.clone();
        let selected = self.selected;
        let lanes = palette.lane;
        let lane_count = repo.max_width;
        let selection = palette.selection;
        let row_even = palette.row_even;
        let muted = palette.muted_foreground;
        let foreground = palette.foreground;
        let mono = palette.mono.clone();
        let focus = self.focus.clone();

        let list = uniform_list("commits", repo.commits.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    let commit = &repo.commits[index];
                    let graph: GraphRow<'_> = repo.graph.get(index).expect("row per commit");

                    let mut row = div()
                        .flex()
                        .items_center()
                        .h(px(graph_canvas::ROW_HEIGHT))
                        .w_full()
                        .text_size(px(12.0))
                        .child(graph_canvas::graph_column(graph, lanes, lane_count))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .pl_2()
                                .text_color(foreground)
                                .child(fake::summary(commit.summary_seed)),
                        )
                        .child(
                            div()
                                .w(px(170.0))
                                .flex_none()
                                .truncate()
                                .text_size(px(11.0))
                                .text_color(muted)
                                .child(fake::author(commit.author)),
                        )
                        .child(
                            div()
                                .w(px(140.0))
                                .flex_none()
                                .text_size(px(11.0))
                                .text_color(muted)
                                .child(fake::format_time(commit.time)),
                        )
                        .child(
                            div()
                                .w(px(80.0))
                                .flex_none()
                                .font_family(mono.clone())
                                .text_size(px(11.0))
                                .text_color(muted)
                                .child(fake::short_id(commit.id)),
                        );

                    if index == selected {
                        row = row.bg(selection);
                    } else if index % 2 == 0 {
                        row = row.bg(row_even);
                    }

                    let entity = entity.clone();
                    let focus = focus.clone();
                    row.cursor_pointer()
                        .id(("commit-row", index))
                        .on_click(move |_event, window, cx| {
                            entity.update(cx, |this, cx| {
                                this.select(index, cx);
                                window.focus(&focus, cx);
                            });
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.commit_scroll)
        .flex_1();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(list)
            .into_any_element()
    }

    fn diff_pane(&self, theme: &Theme, palette: &Palette) -> AnyElement {
        let rows = self.diffs[self.selected % self.diffs.len()].clone();
        let colors = diff_view::DiffColors::from_theme(theme);
        let mono = palette.mono.clone();
        let commit = &self.repo.commits[self.selected];

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(30.0))
            .flex_none()
            .px_3()
            .bg(palette.title_bar)
            .border_b_1()
            .border_color(palette.border)
            .text_size(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child("crates/strata-app/src/app.rs")
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(palette.muted_foreground)
                            .child(format!(
                                "commit {} — synthetic diff, {} rows",
                                fake::short_id(commit.id),
                                rows.len()
                            )),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(palette.muted_foreground)
                    .child("highlighted against the selected commit"),
            );

        let list = uniform_list("diff", rows.len(), move |range, _window, _cx| {
            range
                .map(|index| diff_view::render_row(&rows[index], &colors, &mono))
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.diff_scroll)
        .flex_1();

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
            .child(list)
            .into_any_element()
    }

    fn overlay(&self, palette: &Palette) -> AnyElement {
        let stats = self.metrics.stats();
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
                "{:.2} ms/frame · {:.1} fps",
                stats.frame_ms, stats.fps
            )))
            .child(format!(
                "render {:.2} ms · p95 {:.2} ms",
                stats.render_ms, stats.p95_ms
            ))
            .child(format!(
                "{} samples · {} commits",
                stats.samples,
                self.commit_count()
            ))
            .child(
                div()
                    .text_color(palette.muted_foreground)
                    .child(format!("selected row {}", self.selected)),
            );

        if stats.samples > 0 && stats.frame_ms > 0.0 {
            panel = panel.child(div().text_color(palette.muted_foreground).child(
                if stats.fps >= 55.0 {
                    "smooth (60 fps target)"
                } else {
                    "below 60 fps target"
                },
            ));
        }

        panel.into_any_element()
    }

    fn finish_smoke_run(&mut self, cx: &mut Context<Self>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let stats = self.metrics.stats();
        eprintln!(
            "smoke: {} frames, {:.2} ms/frame mean, {:.1} fps, p95 {:.2} ms, build {:.2} ms",
            self.metrics.frames(),
            stats.frame_ms,
            stats.fps,
            stats.p95_ms,
            stats.render_ms
        );
        cx.quit();
    }
}

impl Render for StrataApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let started = self.metrics.begin_frame();
        let entity = cx.entity();

        let palette = Palette::from_theme(cx.theme());
        let key_listener = cx.listener(
            |this: &mut Self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>| {
                this.on_key(event, window, cx);
            },
        );

        if self.overlay && self.metrics.log_due() {
            let stats = self.metrics.stats();
            eprintln!(
                "frame {:.2} ms ({:.1} fps) · build {:.2} ms · p95 {:.2} ms",
                stats.frame_ms, stats.fps, stats.render_ms, stats.p95_ms
            );
        }

        if self.smoke {
            // Walk the list so the run measures scrolling, not an idle window.
            let last = self.repo.commits.len().saturating_sub(1);
            let next = (self.selected + 3) % last.max(1);
            self.commit_scroll
                .scroll_to_item(next, ScrollStrategy::Center);
            self.selected = next;
            if self.metrics.frames() >= 300 {
                self.finish_smoke_run(cx);
            }
        }

        let root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(palette.background)
            .text_color(palette.foreground)
            .track_focus(&self.focus)
            .on_key_down(key_listener)
            .child(self.header(&palette))
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
                            .child(self.commit_list(entity, &palette))
                            .child(self.diff_pane(cx.theme(), &palette)),
                    ),
            );

        let root = if self.overlay {
            root.child(self.overlay(&palette))
        } else {
            root
        };

        self.metrics.end_frame(started);

        if self.overlay && !self.finished {
            window.request_animation_frame();
        }

        root
    }
}

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
    row_even: Hsla,
    title_bar: Hsla,
    overlay: Hsla,
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
            row_even: theme.list_even,
            title_bar: theme.title_bar,
            overlay: theme.popover,
            mono: theme.mono_font_family.clone(),
            lane,
        }
    }
}
