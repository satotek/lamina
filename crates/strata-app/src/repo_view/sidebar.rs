//! The sidebar: the Working tree and History entries, and the container that
//! hosts the branch, remote, tag and stash sections.

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Styled, div, px,
};
use gpui_kit_assets::IconName;

use super::RepoView;
use super::common::{glyph, icon};
use crate::data::Source;
use crate::style::{self, Design};
use crate::worktree_panel::MainMode;

const SIDEBAR_WIDTH: f32 = 248.0;

impl RepoView {
    pub(super) fn sidebar(&self, cx: &mut Context<Self>, design: &Design) -> AnyElement {
        let mut sidebar = div()
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
            .border_color(design.border);
        // The scope is a property of the history pane, but the sidebar is where
        // the refs and the way out of the filter both are.
        if let Some(name) = self.branches.scope_name() {
            sidebar = sidebar.child(self.scope_banner(&name, cx, design));
        }
        sidebar
            .child(self.nav_section(cx, design))
            .child(self.ref_sections(design))
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
        self.branches.render(
            &data,
            &upstreams,
            remotes,
            self.session.is_some(),
            self.is_busy(),
            &colors,
            &self.entity,
        )
    }
}
