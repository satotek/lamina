//! Background fetch on the interval from `behaviour.auto_fetch_minutes`.
//!
//! One task per repository view: it wakes on a short tick, re-reads the
//! interval (so a settings change applies without a restart) and asks the view
//! for a fetch when one is due. The task holds the view weakly and stops when
//! the window is gone, so it never outlives its repository.
//!
//! A fetch is skipped, not postponed, while the session is busy or a remote
//! tab is disconnected: the next tick tries again. A short tick plus an
//! interval comparison is cheaper than a timer that has to be rebuilt
//! whenever the setting changes. A remote tab uses the same interval and the
//! same quiet fetch as a local one.

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

            let Ok((active, connected, busy, minutes)) = this.read_with(cx, |view, cx| {
                (
                    view.is_active(),
                    view.autofetch_ready(),
                    view.has_operation_in_progress(),
                    Settings::get(cx).behaviour.auto_fetch_minutes,
                )
            }) else {
                // The window or view is gone, and with it the repository.
                return;
            };
            // Stop polling when inactive, `--fake`, or disabled. A lost
            // connection stays in the loop: the next tick tries again once
            // the helper is back, instead of waiting for the tab to reopen.
            if !active || minutes == 0 {
                return;
            }
            if !should_autofetch(connected, busy, minutes) || !due(minutes, last.elapsed()) {
                continue;
            }

            // The command refuses while an operation runs; leaving `last` alone
            // then makes the next tick try again. `Trigger::Automatic` is the
            // quiet path: success does not toast.
            let started = window.update(cx, |_, window, cx| {
                this.update(cx, |view, cx| {
                    if !should_autofetch(
                        view.is_active() && view.autofetch_ready(),
                        view.has_operation_in_progress(),
                        minutes,
                    ) {
                        return false;
                    }
                    view.remote_command(Command::Fetch, Trigger::Automatic, window, cx);
                    view.remote.is_running()
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

/// Whether this tick should start a quiet fetch.
///
/// A remote tab is included once it has a live connection. Disconnected and
/// busy are skipped, not postponed past the interval: the following tick
/// tries again. The fetch itself uses [`Trigger::Automatic`], which stays
/// quiet unless it fails.
pub(crate) fn should_autofetch(connected: bool, busy: bool, minutes: u32) -> bool {
    connected && !busy && minutes > 0
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

    #[test]
    fn a_connected_idle_tab_schedules_a_quiet_fetch() {
        assert!(should_autofetch(true, false, 15));
        // The scheduler asks for `Trigger::Automatic`, which does not toast
        // when the fetch works. A local tab and a remote tab share that path.
        assert_eq!(Trigger::Automatic, quiet_fetch().1);
        assert!(matches!(quiet_fetch().0, Command::Fetch));
    }

    #[test]
    fn a_disconnected_or_busy_tab_does_not_schedule() {
        assert!(!should_autofetch(false, false, 15));
        assert!(!should_autofetch(true, true, 15));
        assert!(!should_autofetch(false, true, 15));
        assert!(!should_autofetch(true, false, 0));
    }

    /// The command the scheduler submits. Kept next to the gate so a remote
    /// tab cannot grow a second, noisy fetch.
    fn quiet_fetch() -> (Command, Trigger) {
        (Command::Fetch, Trigger::Automatic)
    }
}
