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
use lamina_core::session::{Operation, RepoLocation};

use super::RepoView;
use crate::settings::{self, Settings};
use crate::style::Design;

/// The key a repository's "Don't ask" answer is stored under: its working-tree
/// root, canonical when the path can be resolved.
pub(super) fn repository_key(location: &RepoLocation) -> Option<PathBuf> {
    location
        .local_path()
        .map(lamina_core::fs_path::canonicalize)
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
        if !self.commit_graph.is_offered() || !self.commit_graph_allowed(cx) {
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
        self.commit_graph.begin_write();
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
        self.commit_graph.dismiss();
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
        self.commit_graph.dismiss();
        cx.notify();
    }
}

/// The one-time offer to write a commit-graph.
///
/// The history loader raises it, the banner's buttons answer it, and the
/// operation the answer starts settles it. A write that fails returns to
/// `Offered` so the user can retry; every other answer is final until the
/// repository is opened again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum CommitGraphOffer {
    /// Nothing suggested a write yet.
    #[default]
    Hidden,
    /// The banner asks the user.
    Offered,
    /// The offered write is running.
    Writing,
    /// The banner was answered, or shown and dismissed.
    Dismissed,
}

impl CommitGraphOffer {
    /// The history loader found a missing or stale graph.
    ///
    /// Only the initial state changes: an offer already on screen stays, a
    /// running write is not interrupted, and an answered offer stays answered.
    pub(super) fn suggest(&mut self) -> bool {
        if *self != Self::Hidden {
            return false;
        }
        *self = Self::Offered;
        true
    }

    pub(super) fn is_offered(&self) -> bool {
        *self == Self::Offered
    }

    /// The user clicked Write; the operation queue has the job from here.
    pub(super) fn begin_write(&mut self) {
        *self = Self::Writing;
    }

    /// The write reported back. A failure is offered again so the user can
    /// retry; a success is as final as dismissing the banner.
    pub(super) fn write_finished(&mut self, succeeded: bool) {
        if *self != Self::Writing {
            return;
        }
        *self = if succeeded {
            Self::Dismissed
        } else {
            Self::Offered
        };
    }

    /// "Not now" and "Don't ask" both hide the offer for this view.
    pub(super) fn dismiss(&mut self) {
        *self = Self::Dismissed;
    }
}

#[cfg(test)]
mod tests {
    use super::CommitGraphOffer;

    #[test]
    fn a_write_returns_to_the_offer_when_it_fails() {
        let mut offer = CommitGraphOffer::Hidden;
        assert!(offer.suggest());
        offer.begin_write();
        offer.write_finished(false);
        assert!(offer.is_offered());
    }

    #[test]
    fn a_successful_write_and_a_dismissal_are_final() {
        let mut written = CommitGraphOffer::Hidden;
        written.suggest();
        written.begin_write();
        written.write_finished(true);
        assert_eq!(written, CommitGraphOffer::Dismissed);

        let mut dismissed = CommitGraphOffer::Offered;
        dismissed.dismiss();
        assert_eq!(dismissed, CommitGraphOffer::Dismissed);
    }

    #[test]
    fn a_running_write_is_not_offered_again_or_interrupted() {
        let mut offer = CommitGraphOffer::Offered;
        offer.begin_write();
        assert!(!offer.suggest());
        assert_eq!(offer, CommitGraphOffer::Writing);
    }

    #[test]
    fn a_finished_write_does_not_undo_an_answer() {
        let mut offer = CommitGraphOffer::Dismissed;
        offer.write_finished(false);
        assert_eq!(offer, CommitGraphOffer::Dismissed);
    }
}
