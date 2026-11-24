//! The status bar: what runs now, the last git command and the last error.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, div, px,
};
use gpui_component::tooltip::Tooltip;

use std::time::{Duration, Instant};

use super::RepoView;
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

    /// The status line when nothing else is reporting. An unwatched remote
    /// helper cannot see edits made outside the app, so the line says that a
    /// refresh is required instead of claiming the tab is watching.
    pub(super) fn idle_status(&self) -> String {
        if self
            .session
            .as_ref()
            .is_some_and(|session| !session.watches_filesystem())
        {
            t!("status.remote_unwatched").to_string()
        } else {
            t!("status.watching").to_string()
        }
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
        let command = lamina_core::text::truncate_width(&command, 160);
        let error = self.status_error.clone();
        let note = self.status_note.clone();
        // The activity segment names what is running; the command log keeps the
        // actual argv as secondary text, because that is what users ask for
        // when a command is slow.
        let activity = self.activity.status(Instant::now());
        let operation = self.remote_status(design);

        let mut leading = div()
            .flex()
            .items_center()
            .gap(design.px(10.0))
            .flex_1()
            .min_w_0();
        // Shown immediately, not after the activity delay. A fetch never gets
        // an activity label, so this button is the way to stop it.
        if self.can_cancel_running_operation() {
            let entity = self.entity();
            leading = leading.child(
                crate::ui::button("status-cancel", t!("common.cancel").to_string())
                    .ghost()
                    .size(crate::ui::ButtonSize::Tiny)
                    .tooltip(t!("action.cancel_running_operation").to_string())
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |view, cx| {
                            view.cancel_running_operation(cx);
                        });
                    }),
            );
        }
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
        if let Some(quality) = self.connection_quality() {
            let slow = quality.slow;
            let mut chip = div()
                .id("status-rtt")
                .flex_none()
                .text_color(if slow {
                    design.remove_text
                } else {
                    design.remote_text
                })
                .child(quality.label);
            if slow {
                let tip = t!("status.ssh_slow_tooltip").to_string();
                chip = chip.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx));
            }
            bar = bar.child(chip);
        }
        let is_error = error.is_some();
        let message = error.or(note);
        // A git error rarely fits the bar; the tooltip carries the whole text
        // so the advice at its end ("commit or stash …") stays readable.
        let mut slot = div()
            .id("status-message")
            .max_w(design.px(720.0))
            .truncate()
            .text_color(if is_error {
                design.remove_text
            } else {
                design.muted
            });
        if let Some(full) = message.clone() {
            // The bar sits at the window's bottom right, so an unbounded
            // tooltip would run off the window; a fixed width wraps instead.
            let width = design.px(520.0);
            slot = slot.tooltip(move |window, cx| {
                let full = full.clone();
                Tooltip::element(move |_, _| div().max_w(width).child(full.clone()))
                    .build(window, cx)
            });
        }
        bar.child(slot.child(message.unwrap_or_else(|| self.idle_status())))
            .into_any_element()
    }
}

#[cfg(test)]
mod status_clock_tests {
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

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use gpui::{QuitMode, VisualTestContext};
    use lamina_core::session::{Operation, SessionEvent};
    use lamina_core::test_support::TestRepo;

    use super::RepoView;
    use crate::repo_view::Launch;

    fn prepare(cx: &mut gpui::App) {
        cx.set_quit_mode(QuitMode::Explicit);
        gpui_component::init(cx);
        cx.set_global(crate::settings::Settings::default());
        crate::actions::bind(cx);
    }

    fn sh_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    /// A pre-commit hook that sleeps, so cancel has a live git child to kill.
    fn install_slow_hook(repo: &TestRepo) -> std::path::PathBuf {
        let hooks = repo.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).expect("hooks dir");
        let marker = repo.path().join(".git/hook-started");
        let script = format!(
            "#!/bin/sh\ntouch {}\nsleep 30\n",
            sh_quote(&marker.display().to_string())
        );
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, script).expect("write hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("hook mode");
        repo.git(&["config", "core.hooksPath", &hooks.display().to_string()]);
        marker
    }

    #[gpui::test]
    fn cancelling_a_commit_reaches_the_session_and_clears_the_status(
        cx: &mut gpui::TestAppContext,
    ) {
        // The session's worker threads wake the test scheduler from outside it.
        cx.dispatcher.allow_parking();
        cx.update(prepare);

        let repo = TestRepo::new();
        repo.git(&["config", "user.name", "Lamina Test"]);
        repo.git(&["config", "user.email", "lamina@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.write("README.md", "one\n");
        let first = repo.commit("initial");
        let marker = install_slow_hook(&repo);
        repo.write("README.md", "two\n");
        repo.git(&["add", "README.md"]);

        let window = cx.add_window(|window, cx| {
            RepoView::new(Launch::open_with(repo.path(), None), window, cx)
        });
        let view = window.root(cx).expect("the window's root view");
        let mut vcx = VisualTestContext::from_window(window.into(), cx);

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "the history did not load");
            std::thread::sleep(Duration::from_millis(20));
            vcx.run_until_parked();
            if view.read_with(&vcx, |view, _| view.commit_count()) >= 1 {
                break;
            }
        }

        vcx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let started = view.run_operation(
                    Operation::Commit {
                        message: "should not land".into(),
                        amend: false,
                    },
                    "Commit",
                    window,
                    cx,
                );
                assert!(started.is_some(), "the commit did not start");
                assert!(
                    view.can_cancel_running_operation(),
                    "cancel must be offered before the activity delay"
                );
            });
        });

        let deadline = Instant::now() + Duration::from_secs(15);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "the pre-commit hook did not start"
            );
            std::thread::sleep(Duration::from_millis(20));
            vcx.run_until_parked();
        }

        vcx.update(|_window, cx| {
            view.update(cx, |view, cx| {
                let running = view
                    .session
                    .as_ref()
                    .and_then(|session| session.snapshot().busy)
                    .expect("the commit is running");
                assert!(view.cancel_running_operation(cx));
                // The session's start event can reach the view after the
                // cancel on a busy machine. It must not wipe the note.
                view.on_event(SessionEvent::OperationStarted { id: running }, cx);
                assert!(
                    view.activity.is_empty(),
                    "cancel left the activity segment in place"
                );
                assert!(!view.remote.is_running());
                assert_eq!(view.status_error, None);
                assert_eq!(
                    view.status_note.as_deref(),
                    Some(t!("status.cancelled").as_ref())
                );
            });
        });

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            std::thread::sleep(Duration::from_millis(20));
            vcx.run_until_parked();
            // `is_busy` stays true until OperationFinished, which is the event
            // that would paint a red error if cancel were treated as a failure.
            let settled = view.read_with(&vcx, |view, _| {
                !view.is_busy() && view.status_error.is_none()
            });
            if settled {
                assert_eq!(repo.head(), first, "the cancelled commit must not land");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cancel did not finish; head {} first {first}",
                repo.head()
            );
        }

        view.read_with(&vcx, |view, _| {
            let snapshot = view.session.as_ref().expect("session").snapshot();
            assert!(snapshot.busy.is_none());
            assert!(
                snapshot.head.is_some(),
                "the tab can still read the repository"
            );
            assert_eq!(view.status_error, None, "cancel must not paint a red error");
            assert_eq!(
                view.status_note.as_deref(),
                Some(t!("status.cancelled").as_ref())
            );
            assert!(!view.can_cancel_running_operation());
        });
        assert_eq!(repo.head(), first, "the cancelled commit must not land");
    }
}
