//! Background fetch on the interval from `behaviour.auto_fetch_minutes`.
//!
//! One task per repository view: it wakes on a short tick, re-reads the
//! interval (so a settings change applies without a restart) and asks the view
//! for a fetch when one is due. The task holds the view weakly and stops when
//! the window is gone, so it never outlives its repository.
//!
//! A fetch is skipped, not postponed, while the session is busy: the next tick
//! tries again. A short tick plus an interval comparison is cheaper than a
//! timer that has to be rebuilt whenever the setting changes.

use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, Context};

use crate::remote::Command;
use crate::repo_view::RepoView;
use crate::settings::Settings;

/// How often the task checks whether a fetch is due. Fine enough that turning
/// auto-fetch on or off feels immediate, coarse enough to cost nothing.
const TICK: Duration = Duration::from_secs(5);

/// Starts the timer for a repository view. `window` is the window that owns it;
/// a fetch runs through the view's title-bar command, which needs one.
pub(crate) fn start(window: AnyWindowHandle, cx: &mut Context<RepoView>) {
    cx.spawn(async move |this, cx| {
        let mut last = Instant::now();
        loop {
            cx.background_executor().timer(TICK).await;

            let Ok((has_session, minutes)) = this.read_with(cx, |view, cx| {
                (
                    view.session.is_some(),
                    Settings::get(cx).behaviour.auto_fetch_minutes,
                )
            }) else {
                // The window is gone, and with it the repository.
                return;
            };
            // `--fake` has no session to fetch into.
            if !has_session {
                return;
            }
            if !due(minutes, last.elapsed()) {
                if minutes == 0 {
                    // Do not credit time spent with auto-fetch off to the next
                    // interval the user picks.
                    last = Instant::now();
                }
                continue;
            }

            // The command refuses while an operation runs; leaving `last` alone
            // then makes the next tick try again.
            let started = window.update(cx, |_, window, cx| {
                this.update(cx, |view, cx| {
                    let busy = view
                        .session
                        .as_ref()
                        .is_none_or(|session| session.snapshot().busy.is_some());
                    if busy {
                        return false;
                    }
                    view.remote_command(Command::Fetch, window, cx);
                    true
                })
                .unwrap_or(false)
            });
            if !matches!(started, Ok(true)) {
                continue;
            }
            last = Instant::now();
        }
    })
    .detach();
}

/// Whether `elapsed` covers the configured interval. Zero disables auto-fetch.
fn due(minutes: u32, elapsed: Duration) -> bool {
    minutes > 0 && elapsed >= Duration::from_secs(u64::from(minutes) * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fetch_is_due_only_after_the_whole_interval() {
        let fifteen = 15;
        assert!(!due(0, Duration::from_secs(24 * 60 * 60)));
        assert!(!due(fifteen, Duration::from_secs(14 * 60 + 59)));
        assert!(due(fifteen, Duration::from_secs(15 * 60)));
        assert!(due(fifteen, Duration::from_secs(20 * 60)));
    }

    #[test]
    fn the_tick_is_shorter_than_the_shortest_interval() {
        assert!(TICK < Duration::from_secs(5 * 60));
        assert!(TICK > Duration::ZERO);
    }
}
