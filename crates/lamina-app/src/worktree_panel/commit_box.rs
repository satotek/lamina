//! The commit message box: the message textarea, the amend toggle, the commit
//! button and the merge-message prefill rules.

use gpui::prelude::*;
use gpui::{AnyElement, Context, Entity, FontWeight, IntoElement, ParentElement, Styled, div, px};
use gpui_component::input::Textarea;
use lamina_core::HeadInfo;
use lamina_core::Oid;
use lamina_core::session::{RepoSnapshot, RepositorySession};

use super::{Colors, PendingKind, WorktreePanel};
use crate::repo_view::RepoView;

impl WorktreePanel {
    pub(super) fn commit_box(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        cx: &mut Context<RepoView>,
        colors: &Colors,
    ) -> AnyElement {
        let Some(input) = self.message_input.clone() else {
            return div().into_any_element();
        };
        let message = self.message(cx);
        let staged = snapshot.status.staged.len();
        let enabled = commit_enabled(
            staged,
            self.amend,
            &message,
            snapshot.busy.is_some() || self.pending.is_some(),
        );
        let limit = colors.design.summary_hint;
        let length = summary_len(&message);
        let over = length > limit as usize;
        let hint = summary_hint(&message, limit);

        let amend_box = {
            let entity = entity.clone();
            let colors = colors.clone();
            let checked = self.amend;
            div()
                .id("amend-checkbox")
                .flex()
                .items_center()
                .gap_1()
                .flex_none()
                .cursor_pointer()
                .text_size(colors.design.px(11.0))
                .text_color(colors.foreground)
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.toggle_amend(window, cx));
                })
                .child(
                    div()
                        .w(colors.design.px(12.0))
                        .h(colors.design.px(12.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(colors.design.px(3.0))
                        .border_1()
                        .border_color(colors.border)
                        .text_size(colors.design.px(9.0))
                        .text_color(colors.accent)
                        .child(if checked { "✓" } else { "" }),
                )
                .child(t!("worktree.amend").to_string())
        };

        let commit_button = {
            let entity = entity.clone();
            let committing = matches!(self.pending, Some((_, PendingKind::Commit)));
            crate::ui::button(
                "commit-button",
                if staged == 1 {
                    t!("worktree.commit.one", count = staged).to_string()
                } else {
                    t!("worktree.commit.other", count = staged).to_string()
                },
            )
            .size(crate::ui::ButtonSize::Medium)
            .disabled(!enabled)
            .loading(committing)
            .child(
                div()
                    .px(colors.design.px(5.0))
                    .py(colors.design.px(1.0))
                    .rounded(colors.design.px(4.0))
                    .bg(colors.accent_foreground.opacity(0.18))
                    .text_size(colors.design.px(10.5))
                    .child(commit_key_label()),
            )
            .on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| this.commit_worktree(cx));
            })
        };
        let history_button = {
            let entity = entity.clone();
            crate::ui::button("show-history", t!("worktree.history_button").to_string())
                .ghost()
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.close_worktree(window, cx));
                })
        };

        let status = match (&self.error, &self.notice) {
            (Some(error), _) => div()
                .text_size(colors.design.px(10.0))
                .text_color(colors.danger)
                .child(error.clone())
                .into_any_element(),
            (None, Some(notice)) => div()
                .text_size(colors.design.px(10.0))
                .text_color(colors.success)
                .child(notice.clone())
                .into_any_element(),
            (None, None) => div()
                .text_size(colors.design.px(10.0))
                .text_color(colors.muted_foreground)
                .child(if snapshot.busy.is_some() {
                    t!("worktree.working").to_string()
                } else {
                    String::new()
                })
                .into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .gap_2()
            .p_3()
            .bg(colors.title_bar)
            .border_t_1()
            .border_color(colors.border)
            .child(
                div()
                    .text_size(colors.design.px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.muted_foreground)
                    .child(t!("worktree.commit_message").to_string()),
            )
            .child(
                Textarea::new(&input)
                    .h(colors.design.px(84.0))
                    .w_full()
                    .text_size(px(colors.design.code)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(history_button)
                    .child(amend_box)
                    .when_some(hint, |row, hint| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(colors.design.px(10.0))
                                .text_color(colors.warning)
                                .child(hint),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(colors.design.px(11.0))
                            .text_color(if over {
                                colors.warning
                            } else {
                                colors.muted_foreground
                            })
                            .child(format!("{length} / {limit}")),
                    )
                    .child(div().flex_1())
                    .child(commit_button),
            )
            .child(status)
            .into_any_element()
    }
}

#[cfg(not(target_os = "macos"))]
fn commit_key_label() -> &'static str {
    "Ctrl ↵"
}

#[cfg(target_os = "macos")]
fn commit_key_label() -> &'static str {
    "⌘↵"
}

/// Commit button rule: a message and something to commit, unless amending.
pub(super) fn commit_enabled(staged: usize, amend: bool, message: &str, busy: bool) -> bool {
    !busy && !message.trim().is_empty() && (amend || staged > 0)
}

pub(super) fn trim_message(message: &str) -> String {
    message.trim().to_owned()
}

/// First line length in characters, for the summary-length hint.
fn summary_len(message: &str) -> usize {
    message.lines().next().unwrap_or_default().chars().count()
}

fn summary_hint(message: &str, limit: u32) -> Option<String> {
    let len = summary_len(message);
    (len > limit as usize)
        .then(|| t!("worktree.summary_warning", length = len, limit = limit).to_string())
}

/// HEAD's full message, used to prefill the amend box.
///
/// A single object read through gix, so it stays on the UI thread instead of
/// adding a session round trip for every amend toggle. A remote tab reads it
/// in the background instead; see [`RepoView::toggle_amend`].
pub(super) fn previous_message(session: &dyn RepositorySession) -> Option<String> {
    let head = amend_head(session)?;
    session.reader().commit_message(head).ok()
}

/// The commit whose message an amend would start from.
pub(super) fn amend_head(session: &dyn RepositorySession) -> Option<Oid> {
    match session.snapshot().head.as_ref()? {
        HeadInfo::Branch {
            target: Some(target),
            ..
        } => Some(*target),
        HeadInfo::Detached { target } => Some(*target),
        HeadInfo::Branch { target: None, .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_needs_a_message_and_staged_work_unless_amending() {
        assert!(commit_enabled(1, false, "subject", false));
        assert!(!commit_enabled(1, false, "subject", true), "busy session");
        assert!(!commit_enabled(1, false, "  \n\t", false), "blank message");
        assert!(
            !commit_enabled(0, false, "subject", false),
            "nothing staged"
        );
        assert!(
            commit_enabled(0, true, "subject", false),
            "amend can rewrite a message without staged content"
        );
    }

    #[test]
    fn messages_are_trimmed_at_both_ends() {
        assert_eq!(trim_message("  subject\n\nbody  \n\n"), "subject\n\nbody");
        assert_eq!(trim_message("\n\n"), "");
    }

    #[test]
    fn long_summaries_get_a_hint() {
        assert_eq!(summary_hint("short summary", 72), None);
        let summary = "s".repeat(73);
        assert_eq!(
            summary_hint(&summary, 72).as_deref(),
            Some("summary is 73 characters (guideline: 72)")
        );
        let message = format!("{summary}\n\nbody that is long but irrelevant");
        assert!(
            summary_hint(&message, 72).is_some(),
            "only the first line counts"
        );
        assert_eq!(
            summary_hint(&summary, 120),
            None,
            "the limit comes from the settings"
        );
        assert_eq!(
            summary_len(&format!("{}\nbody", "é".repeat(72))),
            72,
            "counts characters, not bytes"
        );
    }
}
