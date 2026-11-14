//! The sidebar: the Working tree, History and Reflog entries, and the
//! container that hosts the branch, remote, tag and stash sections.

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Styled, div, px,
};
use gpui_kit_assets::IconName;

use super::RepoView;
use super::common::{glyph, icon};
use crate::data::Source;
use crate::style::{self, Design};
use crate::worktree_panel::MainMode;

/// The sidebar's width on a window with room to spare.
const SIDEBAR_WIDTH: f32 = 248.0;
/// The narrowest the sidebar goes before it starts taking room the History
/// view needs.
const SIDEBAR_MIN_WIDTH: f32 = 168.0;
/// The share of the window the sidebar aims for between those two.
const SIDEBAR_SHARE: f32 = 0.22;

/// The sidebar's width at `viewport` width.
///
/// The main area has to keep the details' two minimum columns (a 200 px file
/// list beside a 240 px diff) even at the app's minimum window size, and the
/// commit list wants room for its description. A proportional sidebar gives
/// that room back as the window narrows instead of pushing the panes below
/// their minimums. It also grows smoothly with the window, so the main area
/// never shrinks as the window gets wider.
pub(super) fn width_for(viewport: f32) -> f32 {
    (viewport * SIDEBAR_SHARE).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_WIDTH)
}

impl RepoView {
    pub(super) fn sidebar(
        &self,
        cx: &mut Context<Self>,
        design: &Design,
        width: f32,
    ) -> AnyElement {
        let mut sidebar = div()
            .flex()
            .flex_col()
            .w(px(width))
            .h_full()
            .flex_none()
            .gap(px(14.0))
            .py_3()
            .px_2()
            .overflow_hidden()
            .bg(design.sidebar)
            .border_r_1()
            .border_color(design.border);
        // The scope is a property of the history pane, but the sidebar is where
        // the refs and the way out of the filter both are.
        if let Some(name) = self.branches.scope_name() {
            sidebar = sidebar.child(self.scope_banner(&name, cx, design));
        }
        let entity = cx.entity();
        sidebar
            .child(self.nav_section(cx, design))
            .child(self.ref_sections(design, &entity))
            .into_any_element()
    }

    /// Says which ref the history is limited to and offers the way back to
    /// everything.
    fn scope_banner(&self, name: &str, cx: &mut Context<Self>, design: &Design) -> AnyElement {
        crate::ui::banner(
            crate::ui::BannerKind::Info,
            format!("Showing {name} only"),
            design,
        )
        .rounded(px(6.0))
        // The sidebar is narrow; the banner keeps the full sentence readable
        // by dropping a size and tightening its own padding.
        .px_2()
        .gap_1()
        .text_size(px(design.tiny()))
        .action(
            crate::ui::button("show-all-refs", "Show all")
                .size(crate::ui::ButtonSize::Tiny)
                .kind(crate::ui::ButtonKind::Ghost)
                .on_click(cx.listener(|this, _, _, cx| this.show_all_refs(cx))),
        )
        .into_any_element()
    }

    /// The view entries at the top of the sidebar.
    fn nav_section(&self, cx: &mut Context<Self>, design: &Design) -> AnyElement {
        let (staged, unstaged) = self
            .source()
            .map_or((0, 0), |source| source.sidebar().working_tree);
        let changes = staged + unstaged;
        let worktree = self.mode == MainMode::Worktree;
        let history = self.mode == MainMode::History;
        let reflog = self.mode == MainMode::Reflog;

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
            .when(history, |row| row.bg(design.selected))
            .hover(|style| style.bg(design.selected))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                if this.mode != MainMode::History {
                    this.close_worktree(window, cx);
                }
            }))
            .child(glyph(
                style::glyph::COMMIT,
                15.0,
                if history {
                    design.accent_text
                } else {
                    design.text
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(history, |row| row.font_weight(FontWeight::MEDIUM))
                    .child("History"),
            );

        let reflog_entry = div()
            .id("reflog-entry")
            .flex()
            .items_center()
            .gap_2()
            .h(px(30.0))
            .px_2()
            .rounded(px(7.0))
            .text_size(px(design.ui))
            .cursor_pointer()
            .when(reflog, |row| row.bg(design.selected))
            .hover(|style| style.bg(design.selected))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_reflog(window, cx)))
            .child(icon(
                IconName::ClockArrowDown,
                15.0,
                if reflog {
                    design.accent_text
                } else {
                    design.text
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(reflog, |row| row.font_weight(FontWeight::MEDIUM))
                    .child("Reflog"),
            );

        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(working_tree)
            .child(history)
            .child(reflog_entry)
            .into_any_element()
    }

    /// Branches, remotes, tags and stashes below the view entries.
    fn ref_sections(&self, design: &Design, entity: &Entity<RepoView>) -> AnyElement {
        let Some(source) = self.source() else {
            return div().into_any_element();
        };
        let data = source.sidebar();
        let upstreams = match &source {
            Source::Repo(snapshot) => snapshot.upstreams.as_ref().clone(),
            Source::Fake(_) => Default::default(),
        };
        let remotes = match &source {
            Source::Repo(snapshot) => snapshot.remotes.as_slice(),
            Source::Fake(_) => &[],
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
        // The snapshot's generation is the sidebar list's cache key: it changes
        // on every publish, so the flattened strings are reused between frames
        // but never across two different snapshots.
        let revision = match &source {
            Source::Repo(snapshot) => snapshot.generation,
            Source::Fake(_) => 0,
        };
        // The submodule and worktree sections come from the view's own answer,
        // not from the snapshot; their generation is part of the cache key.
        let workdir = self
            .session
            .as_ref()
            .and_then(|session| session.repo().workdir().map(std::path::Path::to_path_buf));
        let extras = crate::branches::SidebarExtras {
            submodules: self.extra_lists.submodules.as_slice(),
            worktrees: self.extra_lists.worktrees.as_slice(),
            current_worktree: workdir.as_deref(),
            revision: self.extra_lists.generation,
        };
        self.branches.render(
            data,
            revision,
            &upstreams,
            remotes,
            &extras,
            &self.pull_requests.branches(),
            self.session.is_some(),
            self.is_busy(),
            &colors,
            entity,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style;

    #[test]
    fn the_sidebar_stays_between_its_bounds_and_leaves_the_main_area_room() {
        // The details' file list and diff minimums; the main area holds them
        // side by side.
        const MAIN_MIN_WIDTH: f32 = 440.0;
        for width in 640..=2_000 {
            let width = width as f32;
            let sidebar = width_for(width);
            assert!(
                (SIDEBAR_MIN_WIDTH..=SIDEBAR_WIDTH).contains(&sidebar),
                "the sidebar is {sidebar} px at a {width} px window"
            );
            assert!(
                width - sidebar >= MAIN_MIN_WIDTH,
                "a {width} px window leaves the main area {} px, below its {} px minimum",
                width - sidebar,
                MAIN_MIN_WIDTH
            );
        }
        assert_eq!(width_for(3_200.0), SIDEBAR_WIDTH);
    }

    #[test]
    fn the_main_area_grows_with_the_window() {
        let mut previous = 0.0;
        for width in 640..=2_000 {
            let main = width as f32 - width_for(width as f32);
            assert!(
                main >= previous,
                "the main area shrinks from {previous} to {main} px when the window grows to {width} px"
            );
            previous = main;
        }
    }

    #[test]
    fn the_app_minimum_window_keeps_the_history_view_inside_its_minimums() {
        let width = style::MIN_WINDOW_SIZE.0;
        let sidebar = width_for(width);
        assert!(
            width - sidebar >= 440.0,
            "the History view needs a 200 px file column beside a 240 px diff"
        );
        // The commit list also keeps its fixed columns next to a readable
        // description at the minimum, which the commit list's own test covers.
        assert!(sidebar <= SIDEBAR_WIDTH);
    }
}
