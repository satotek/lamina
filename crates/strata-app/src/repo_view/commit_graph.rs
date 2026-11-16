//! The offer to write a commit-graph.
//!
//! The history loader reports when a missing or stale graph made the first walk
//! expensive. This module renders the one banner that asks about it and runs the
//! write only after the user clicks Write.

use std::path::PathBuf;

use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Styled as _, div,
};
use strata_core::session::{Operation, RepoLocation};

use super::RepoView;
use crate::settings::{self, Settings};
use crate::style::Design;

/// The key a repository's "Don't ask" answer is stored under: its working-tree
/// root, canonical when the path can be resolved.
pub(super) fn repository_key(location: &RepoLocation) -> Option<PathBuf> {
    location
        .local_path()
        .map(strata_core::fs_path::canonicalize)
}

impl RepoView {
    /// Whether the banner may be shown for this repository.
    pub(super) fn commit_graph_allowed(&self, cx: &App) -> bool {
        self.repository_key
            .as_deref()
            .is_some_and(|repository| Settings::get(cx).commit_graph.asks(repository))
    }

    /// The banner that offers a commit-graph write, until it is answered.
    pub(super) fn commit_graph_banner(
        &self,
        entity: Entity<RepoView>,
        design: &Design,
        cx: &App,
    ) -> Option<AnyElement> {
        if !self.commit_graph_offer || self.commit_graph_writing || !self.commit_graph_allowed(cx) {
            return None;
        }

        let write = entity.clone();
        let not_now = entity.clone();
        let never = entity;
        let banner = crate::ui::banner(
            crate::ui::BannerKind::Info,
            t!("history.commit_graph_offer").to_string(),
            design,
        )
        .detail(t!("history.commit_graph_detail").to_string())
        .action(
            crate::ui::button(
                "commit-graph-write",
                t!("history.commit_graph_write").to_string(),
            )
            .size(crate::ui::ButtonSize::Tiny)
            .on_click(move |_, window, cx| {
                write.update(cx, |this, cx| this.write_commit_graph(window, cx));
            }),
        )
        .action(
            crate::ui::button("commit-graph-not-now", t!("history.not_now").to_string())
                .size(crate::ui::ButtonSize::Tiny)
                .kind(crate::ui::ButtonKind::Ghost)
                .on_click(move |_, _, cx| {
                    not_now.update(cx, |this, cx| this.dismiss_commit_graph(cx));
                }),
        )
        .action(
            crate::ui::button("commit-graph-never", t!("history.dont_ask").to_string())
                .size(crate::ui::ButtonSize::Tiny)
                .kind(crate::ui::ButtonKind::Ghost)
                .on_click(move |_, _, cx| {
                    never.update(cx, |this, cx| this.ignore_commit_graph(cx));
                }),
        );

        Some(
            div()
                .id("commit-graph-banner")
                .flex_none()
                .child(banner)
                .into_any_element(),
        )
    }

    /// Writes the graph through the operation queue, which shows its progress
    /// and reloads the history when it finishes. Only a click reaches here.
    pub(super) fn write_commit_graph(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.session.is_none() || self.is_busy() {
            return;
        }
        self.commit_graph_writing = true;
        self.run_operation(
            Operation::WriteCommitGraph,
            t!("history.commit_graph_op").to_string(),
            window,
            cx,
        );
        cx.notify();
    }

    /// Hides the offer until the repository is opened again.
    pub(super) fn dismiss_commit_graph(&mut self, cx: &mut Context<Self>) {
        self.commit_graph_offer = false;
        self.commit_graph_dismissed = true;
        cx.notify();
    }

    /// Remembers the answer for this repository, so the offer never returns.
    pub(super) fn ignore_commit_graph(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.repository_key.clone() else {
            return;
        };
        settings::update(cx, |settings| {
            settings.commit_graph.ignore(repository);
        });
        self.commit_graph_offer = false;
        self.commit_graph_dismissed = true;
        cx.notify();
    }
}
