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

use gpui::{AnyWindowHandle, Context, Task};

use crate::remote::{Command, Trigger};
use crate::repo_view::RepoView;
use crate::settings::Settings;

/// How often the task checks whether a fetch is due. Fine enough that turning
/// auto-fetch on or off feels immediate, coarse enough to cost nothing.
const TICK: Duration = Duration::from_secs(5);

/// Starts the timer for an active repository view. Dropping the returned
/// [`Task`] cancels polling immediately when the tab becomes inactive or closes.
pub(crate) fn start(
    window: AnyWindowHandle,
    mut last: Instant,
    cx: &mut Context<RepoView>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(TICK).await;

            let Ok((active, has_session, minutes)) = this.read_with(cx, |view, cx| {
                (
                    view.is_active(),
                    view.session.is_some(),
                    Settings::get(cx).behaviour.auto_fetch_minutes,
                )
            }) else {
                // The window or view is gone, and with it the repository.
                return;
            };
            // Stop polling when inactive, `--fake`, or disabled.
            if !active || !has_session || minutes == 0 {
                return;
            }
            if !due(minutes, last.elapsed()) {
                continue;
            }

            // The command refuses while an operation runs; leaving `last` alone
            // then makes the next tick try again.
            let started = window.update(cx, |_, window, cx| {
                this.update(cx, |view, cx| {
                    if !view.is_active() {
                        return false;
                    }
                    let busy = view
                        .session
                        .as_ref()
                        .is_none_or(|session| session.snapshot().busy.is_some());
                    if busy {
                        return false;
                    }
                    view.remote_command(Command::Fetch, Trigger::Automatic, window, cx);
                    true
                })
                .unwrap_or(false)
            });
            if !matches!(started, Ok(true)) {
                continue;
            }
            last = Instant::now();
            let _ = this.update(cx, |view, _| view.record_autofetch(last));
        }
    })
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
