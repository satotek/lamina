//! The status bar: what runs now, the last git command and the last error.

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, px};

use std::time::{Duration, Instant};

use super::RepoView;
use crate::remote;
use crate::style::Design;

/// How long the bar keeps a message before treating it as stale. Long enough
/// to read a path or an error, short enough that a message about an old
/// failure does not look like the outcome of the last action.
pub(super) const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(30);

/// Whether the message first shown at `shown` has outlived the bar's window.
pub(super) fn status_message_expired(shown: Option<Instant>, now: Instant) -> bool {
    shown.is_some_and(|shown| now.saturating_duration_since(shown) >= STATUS_MESSAGE_TTL)
}

/// What a new observation of the status line found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Seen {
    /// The same message is still fresh.
    Fresh,
    /// The message changed, so its time starts now.
    Changed,
    /// The message stayed too long and the caller must drop it.
    Expired,
}

/// The one status message on screen and when it appeared.
///
/// The bar has a single slot, and several actions only write an error or a
/// note without clearing the other. Tracking the text here means every path
/// that leaves a message behind is covered: a later message resets the clock,
/// and a message nothing replaces is retired on its own.
#[derive(Default)]
pub(super) struct StatusClock {
    shown: Option<(String, Instant)>,
}

impl StatusClock {
    pub(super) fn observe(&mut self, text: &str, now: Instant) -> Seen {
        match &self.shown {
            Some((shown, at)) if shown == text => {
                if status_message_expired(Some(*at), now) {
                    self.shown = None;
                    Seen::Expired
                } else {
                    Seen::Fresh
                }
            }
            _ => {
                self.shown = Some((text.to_owned(), now));
                Seen::Changed
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.shown = None;
    }
}

impl RepoView {
    /// Retires a status message once it has been visible long enough, and
    /// schedules the repaint that makes the retirement visible. Runs from
    /// every frame so a message set by a less central action is covered too.
    pub(super) fn observe_status(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self
            .status_error
            .clone()
            .or_else(|| self.status_note.clone())
        else {
            self.status_clock.clear();
            return;
        };
        match self.status_clock.observe(&text, Instant::now()) {
            Seen::Expired => {
                self.status_error = None;
                self.status_note = None;
                cx.notify();
            }
            Seen::Changed => {
                // Nothing else repaints when a message nobody replaces goes
                // stale, so one timer per new message wakes the view.
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(STATUS_MESSAGE_TTL).await;
                    this.update(cx, |this, cx| this.observe_status(cx)).ok();
                })
                .detach();
            }
            Seen::Fresh => {}
        }
    }

    /// Drops the current message. A new action replaces whatever an earlier
    /// one left, so a success never leaves an old failure on screen.
    pub(super) fn clear_status(&mut self) {
        self.status_error = None;
        self.status_note = None;
        self.status_clock.clear();
    }

    pub(super) fn status_bar(&self, design: &Design) -> AnyElement {
        let command = self
            .last_command
            .clone()
            .unwrap_or_else(|| t!("status.no_commands").to_string());
        // The status line is one row; a command with hundreds of path
        // arguments would otherwise build a string many screens wide. The
        // truncation counts display columns, so a Japanese path is not cut
        // into an invalid boundary.
        let command = strata_core::text::truncate_width(&command, 160);
        let error = self.status_error.clone();
        let note = self.status_note.clone();
        // The activity segment names what is running; the command log keeps the
        // actual argv as secondary text, because that is what users ask for
        // when a command is slow.
        let activity = self.activity.status(Instant::now());
        let operation = self.remote_status(&remote::Colors {
            foreground: design.text,
            muted_foreground: design.muted,
            border: design.border,
            accent: design.accent,
            accent_foreground: design.on_accent,
            zoom: design.zoom,
        });

        let mut leading = div()
            .flex()
            .items_center()
            .gap(design.px(10.0))
            .flex_1()
            .min_w_0();
        if let Some(activity) = activity {
            leading = leading
                .child(crate::ui::spinner("status-activity", design, 11.0))
                .child(
                    div()
                        .flex_none()
                        .text_color(design.accent)
                        .child(activity.label),
                );
            if let Some(elapsed) = activity.elapsed {
                leading = leading.child(div().flex_none().text_color(design.muted).child(elapsed));
            }
        } else {
            leading = leading.child(
                div()
                    .flex_none()
                    .w(design.px(7.0))
                    .h(design.px(7.0))
                    .rounded_full()
                    .bg(design.add_text),
            );
        }
        leading = leading.child(
            div()
                .min_w_0()
                .truncate()
                .font_family(design.mono.clone())
                .text_color(design.muted)
                .child(command),
        );

        let mut bar = div()
            .flex()
            .items_center()
            .justify_between()
            .h(design.px(26.0))
            .flex_none()
            .px_3()
            .gap(design.px(10.0))
            .bg(design.chrome)
            .border_t_1()
            .border_color(design.border)
            .text_size(px(design.small()))
            .child(leading);
        // Only while a fetch, pull or push runs: the bar otherwise keeps its
        // command log and last-error slots.
        if let Some(operation) = operation {
            bar = bar.child(operation);
        }
        bar.child(
            div()
                .max_w(design.px(720.0))
                .truncate()
                .text_color(if error.is_some() {
                    design.remove_text
                } else {
                    design.muted
                })
                .child(
                    error
                        .or(note)
                        .unwrap_or_else(|| t!("status.watching").to_string()),
                ),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{STATUS_MESSAGE_TTL, Seen, StatusClock, status_message_expired};
    use std::time::{Duration, Instant};

    #[test]
    fn a_status_message_expires_after_its_window() {
        let now = Instant::now();
        assert!(!status_message_expired(None, now));
        assert!(!status_message_expired(
            Some(now),
            now + STATUS_MESSAGE_TTL - Duration::from_secs(1)
        ));
        assert!(status_message_expired(Some(now), now + STATUS_MESSAGE_TTL));
    }

    #[test]
    fn a_replaced_message_gets_a_fresh_window() {
        let start = Instant::now();
        let mut clock = StatusClock::default();

        assert_eq!(clock.observe("first failure", start), Seen::Changed);
        assert_eq!(
            clock.observe("first failure", start + Duration::from_secs(5)),
            Seen::Fresh
        );

        // A later action leaves a new message: it must not inherit the age of
        // the message it replaced.
        assert_eq!(
            clock.observe("done", start + STATUS_MESSAGE_TTL),
            Seen::Changed
        );
        assert_eq!(
            clock.observe("done", start + STATUS_MESSAGE_TTL + Duration::from_secs(1)),
            Seen::Fresh
        );
        assert_eq!(
            clock.observe("done", start + STATUS_MESSAGE_TTL * 2),
            Seen::Expired
        );
        // Once expired, the same text seen again starts over.
        assert_eq!(
            clock.observe("done", start + STATUS_MESSAGE_TTL * 2),
            Seen::Changed
        );
    }
}
