//! A repository that lives on another machine.
//!
//! Connecting runs on a worker thread. That thread, and the session's reader
//! thread, only push [`ConnectMsg`] values. One foreground task drains them.
//! The reader callback must not touch GPUI or drop the session: doing either
//! deadlocks the read loop.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement as _, Styled as _, Window, div,
};
use lamina_core::session::{RepoLocation, RepositorySession, SessionEvent};
use log::{info, warn};

use super::RepoView;
use crate::remote_open::{
    ConnectMsg, ConnectPhase, RemoteLaunch, WorkerEvent, spawn_connect, tab_title,
    unavailable_reason,
};
use crate::style::Design;
use crate::ui;

/// Connect state kept for the life of the tab, including after the history is
/// on screen, so write actions stay disabled and a dropped connection can
/// reconnect in place.
pub(super) struct RemoteTab {
    spec: crate::remote_open::RemoteSpec,
    ssh: Option<crate::remote_open::SshOverride>,
    phase: ConnectPhase,
    /// A restored tab shows Connect instead of dialling out at startup.
    placeholder: bool,
    lost: bool,
    reconnecting: bool,
    /// Session events that arrive before the new session replaces the old one.
    /// Applying them early would paint the new snapshot over the one still on
    /// screen, or hand them to a session that is about to be dropped.
    buffering: bool,
    pending: Vec<SessionEvent>,
    error: Option<String>,
    /// Latest idle round trip copied off the session. `None` until a probe is
    /// answered, and while the tab is lost or reconnecting.
    rtt: Option<Duration>,
}

/// What a connected remote tab tells the workspace to record.
///
/// The identity is the `ssh://` URI the dialog produced, not the root the
/// handshake resolved: the helper expands `~` on the host, and a reopen
/// compares the user's own spelling — the resolved form would open a second
/// tab for the same repository.
fn stored_identity(tab: Option<&RemoteTab>) -> Option<std::path::PathBuf> {
    tab.map(|tab| tab.spec.stored_path())
}

impl RepoView {
    pub(super) fn begin_remote(
        &mut self,
        launch: RemoteLaunch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = tab_title(&launch.spec.host, &launch.spec.path);
        self.name = Some(name.clone());
        self.title = if launch.auto_connect {
            crate::data::window_title(&name, None)
        } else {
            crate::data::disconnected_window_title(&name)
        };
        window.set_window_title(&self.title);
        let auto_connect = launch.auto_connect;
        self.remote_tab = Some(RemoteTab {
            spec: launch.spec,
            ssh: launch.ssh,
            phase: ConnectPhase::Connecting,
            placeholder: !auto_connect,
            lost: false,
            reconnecting: false,
            buffering: false,
            pending: Vec::new(),
            error: None,
            rtt: None,
        });
        // No local checkout, so the commit-graph offer and the config viewer
        // have nothing to key off.
        self.repository_key = None;
        // The title-bar field is the same one a local tab uses. Creating it
        // here means a connect that finishes later can search immediately.
        self.search.init_input(window, cx);
        if self.active {
            self.publish_repository(cx);
        }
        if auto_connect {
            self.start_remote_connect(cx);
        }
    }

    /// Starts a connect when a restored tab is still waiting, or a previous
    /// attempt failed before a session existed. An open session is left alone.
    pub(crate) fn ensure_remote_connect(&mut self, cx: &mut Context<Self>) {
        let waiting = self
            .remote_tab
            .as_ref()
            .is_some_and(|tab| tab.placeholder || (tab.error.is_some() && self.session.is_none()));
        if waiting {
            self.start_remote_connect(cx);
        }
    }

    /// Starts a connect attempt. A tab that already has a snapshot keeps it
    /// until the new session is ready.
    fn start_remote_connect(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.remote_tab.as_mut() else {
            return;
        };
        let spec = tab.spec.clone();
        let ssh = tab.ssh.clone();
        tab.placeholder = false;
        tab.error = None;
        tab.phase = ConnectPhase::Connecting;
        tab.buffering = true;
        tab.pending.clear();
        tab.lost = false;
        tab.reconnecting = self.session.is_some();
        tab.rtt = None;
        let (sender, mut receiver) = futures_mpsc::unbounded();
        if tab.reconnecting {
            info!(
                "remote: reconnecting to {} at {}",
                tab.spec.host, tab.spec.path
            );
        }
        spawn_connect(spec, ssh, sender);
        // Replacing the task drops the previous receiver. The old worker's
        // sends then fail, and an unsent `Ready` drops its session on that
        // worker rather than on the UI thread.
        self._bridge = Some(cx.spawn(async move |this, cx| {
            while let Some(message) = receiver.next().await {
                if this
                    .update(cx, |this, cx| this.on_connect_msg(message, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn on_connect_msg(&mut self, message: ConnectMsg, cx: &mut Context<Self>) {
        match message {
            ConnectMsg::Worker(WorkerEvent::Phase(phase)) => {
                if let Some(tab) = &mut self.remote_tab {
                    tab.phase = phase;
                }
                self.notify_if_active(cx);
            }
            ConnectMsg::Worker(WorkerEvent::Askpass {
                prompt,
                secret,
                reply,
            }) => self.prompt_remote_askpass(prompt, secret, reply, cx),
            ConnectMsg::Worker(WorkerEvent::Failed(error)) => {
                if self.session.is_some() {
                    warn!("remote: connection lost: {error}");
                } else {
                    warn!("remote: connect failed: {error}");
                }
                if let Some(tab) = &mut self.remote_tab {
                    tab.buffering = false;
                    tab.pending.clear();
                    tab.reconnecting = false;
                    tab.error = Some(error);
                    if self.session.is_some() {
                        tab.lost = true;
                    }
                }
                self.notify_if_active(cx);
            }
            ConnectMsg::Ready(session) => {
                if self.remote_tab.is_none() {
                    super::drop_session_in_background(session);
                    return;
                }
                self.install_remote_session(session, cx);
            }
            ConnectMsg::Session(event) => {
                let buffering = self.remote_tab.as_ref().is_some_and(|tab| tab.buffering);
                if buffering {
                    if let Some(tab) = &mut self.remote_tab {
                        tab.pending.push(event);
                    }
                } else {
                    self.on_event(event, cx);
                }
            }
        }
    }

    /// The askpass helper blocks on `reply`. The modal answers it; this method
    /// does not, and it must not call back into the session.
    fn prompt_remote_askpass(
        &self,
        prompt: String,
        secret: bool,
        reply: mpsc::Sender<Option<String>>,
        cx: &mut Context<Self>,
    ) {
        crate::remote_open::show_askpass_prompt(self.window_handle, prompt, secret, reply, cx);
    }

    fn install_remote_session(
        &mut self,
        session: Box<dyn RepositorySession>,
        cx: &mut Context<Self>,
    ) {
        // The handshake reports an SSH location. A local path here would be a
        // helper bug; the tab keeps the name the dialog already showed.
        let (name, stored) = match session.location() {
            RepoLocation::Ssh { host, path, .. } => (
                tab_title(&host, &path),
                stored_identity(self.remote_tab.as_ref()),
            ),
            RepoLocation::Local(_) => (self.name.clone().unwrap_or_default(), None),
        };
        // The connect worker waits for the first snapshot chunk before it
        // reports the session, so the head is already published here. A
        // reader call would be a synchronous round trip on the UI thread.
        let head = session.snapshot().head.clone();
        self.name = Some(name.clone());
        self.title = crate::data::window_title(&name, head.as_ref());
        info!("remote: session ready for {name}");
        let old = self.adopt_session(session);
        if let Some(tab) = &mut self.remote_tab {
            tab.phase = ConnectPhase::LoadingHistory;
            tab.buffering = false;
            tab.lost = false;
            tab.reconnecting = false;
            tab.error = None;
            tab.placeholder = false;
            // The previous session's sample is not this connection's.
            tab.rtt = None;
        }
        let pending = self
            .remote_tab
            .as_mut()
            .map(|tab| std::mem::take(&mut tab.pending))
            .unwrap_or_default();
        for event in pending {
            self.on_event(event, cx);
        }
        self.note_remote_history();
        if let Some(stored) = stored {
            self.apply_remote_location(stored, cx);
        }
        if self.active {
            self.publish_repository(cx);
        }
        if let Some(old) = old {
            super::drop_session_in_background(old);
        }
        self.watch_connection_rtt(cx);
        // The fetch timer starts only once a session exists. A restored tab
        // stays on the Connect page until then.
        self.sync_autofetch(cx);
        if let Some(handle) = self.window_handle {
            let title = self.title.clone();
            let _ = handle.update(cx, |_, window, _| window.set_window_title(&title));
        }
        self.notify_if_active(cx);
    }

    fn apply_remote_location(&self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        else {
            return;
        };
        // This runs inside this view's own update, and redrawing the tab
        // strip notifies the active tab's view, which is this one when the
        // connection finishes in front. Updating the workspace here would
        // lease this view a second time and panic, so wait until the current
        // update has returned.
        let view = cx.entity_id();
        cx.defer(move |cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.note_remote_path(view, path, cx)
            });
        });
    }

    /// Leaves the loading page once the first history chunk is on screen.
    pub(super) fn note_remote_history(&mut self) {
        let Some(tab) = self.remote_tab.as_mut() else {
            return;
        };
        if tab.phase != ConnectPhase::LoadingHistory {
            return;
        }
        let ready = self.session.as_ref().is_some_and(|session| {
            let commits = &session.snapshot().commits;
            commits.complete || !commits.is_empty()
        });
        if ready {
            tab.phase = ConnectPhase::Ready;
        }
    }

    /// A live session that can take a fetch. A remote tab that is still
    /// dialling, lost, or reconnecting is not: the fetch would fail or land
    /// on a helper that is about to be replaced.
    pub(crate) fn autofetch_ready(&self) -> bool {
        if self.session.is_none() {
            return false;
        }
        let Some(tab) = &self.remote_tab else {
            return true;
        };
        !tab.lost && !tab.reconnecting && !tab.placeholder && tab.error.is_none()
    }

    /// True for the whole life of a remote tab, before a session exists and
    /// after it does. A local tab stays false.
    /// A remote tab with no session that is not dialling: the restored
    /// Connect placeholder, or an attempt that failed.
    pub(super) fn remote_not_connected(&self) -> bool {
        self.session.is_none()
            && self
                .remote_tab
                .as_ref()
                .is_some_and(|tab| tab.placeholder || tab.error.is_some())
    }

    /// Why a command that needs a session cannot run.
    ///
    /// A restored remote tab has no repository open yet, but the generic
    /// no-repository reason would tell the user to open one instead of the
    /// Connect button the tab is showing.
    pub(crate) fn no_session_reason(&self) -> String {
        if self.remote_not_connected() {
            t!("remote.not_connected").to_string()
        } else {
            t!("remote.no_session").to_string()
        }
    }

    /// Asks for a fresh status. A watched remote tab already hears outside
    /// edits; this is how the user asks anyway, and how an unwatched helper
    /// catches up. A local tab uses the same request; its watcher is not replaced.
    pub(crate) fn refresh_working_tree(&mut self, cx: &mut Context<Self>) {
        if self.session.is_none() {
            if self.is_ssh() {
                self.status_note = Some(t!("title_bar.not_connected").to_string());
                self.notify_if_active(cx);
            }
            return;
        }
        self.reload_working_tree(cx);
    }

    fn reload_working_tree(&mut self, cx: &mut Context<Self>) {
        // Shared with the focus refresh, so focusing the window just after
        // an explicit refresh does not ask the helper twice.
        self.last_remote_refresh = Some(Instant::now());
        if let Some(session) = self.session.as_ref() {
            session.reload();
        }
        cx.notify();
    }

    pub(super) fn on_window_activated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !window.is_window_active() {
            return;
        }
        self.refresh_remote_on_focus(cx);
    }

    /// Reloads a connected remote tab when the window is focused, at most
    /// once per [`REMOTE_REFRESH_INTERVAL`], and only when the helper could
    /// not watch the disk. A live watcher already pushes those edits.
    fn refresh_remote_on_focus(&mut self, cx: &mut Context<Self>) {
        if !self.is_ssh() || self.session.is_none() || self.remote_not_connected() {
            return;
        }
        let degraded = self
            .session
            .as_ref()
            .is_some_and(|session| !session.watches_filesystem());
        if !focus_refresh_applies(
            degraded,
            remote_refresh_due(self.last_remote_refresh, Instant::now()),
        ) {
            return;
        }
        self.reload_working_tree(cx);
    }

    pub(crate) fn is_ssh(&self) -> bool {
        self.remote_tab.is_some()
            || self
                .session
                .as_ref()
                .is_some_and(|session| matches!(session.location(), RepoLocation::Ssh { .. }))
    }

    /// True when a remote tab has a live connection. Commands that run
    /// something against the host wait for this; a lost connection closes them
    /// again until the reconnect banner succeeds.
    pub(crate) fn remote_connected(&self) -> bool {
        self.session.is_some() && !self.remote_tab.as_ref().is_some_and(|tab| tab.lost)
    }

    /// The command the embedded terminal runs for this tab's host, with the
    /// same `ssh` program and options the session dials with.
    ///
    /// `Ok(None)` means there is nothing to run it against yet; an error is the
    /// refusal that must be shown instead of starting a terminal.
    #[cfg(feature = "terminal")]
    pub(super) fn remote_terminal_command(
        &self,
    ) -> Result<Option<crate::terminal_panel::TerminalCommand>, String> {
        let Some(tab) = self.remote_tab.as_ref() else {
            return Ok(None);
        };
        if !self.remote_connected() {
            return Ok(None);
        }
        let (program, args) = crate::remote_open::terminal_command(&tab.spec, tab.ssh.as_ref())?;
        Ok(Some(crate::terminal_panel::TerminalCommand {
            program,
            args,
            title: crate::remote_open::terminal_tab_title(&tab.spec),
        }))
    }

    /// Records the remote-repository reason and reports that the caller should
    /// stop. The status bar shows it; a toast would read as a failed command.
    pub(crate) fn refuse_if_ssh(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.is_ssh() {
            return false;
        }
        self.status_note = Some(unavailable_reason());
        self.notify_if_active(cx);
        true
    }

    /// Refuses `operation` only when this tab is remote and the helper cannot
    /// run it. Supported writes go through the same session path as a local tab.
    pub(crate) fn refuse_unsupported(
        &mut self,
        operation: &lamina_core::session::Operation,
        cx: &mut Context<Self>,
    ) -> bool {
        self.is_ssh()
            && !lamina_remote_client::is_supported_operation(operation)
            && self.refuse_if_ssh(cx)
    }

    pub(super) fn connect_page_visible(&self) -> bool {
        let Some(tab) = &self.remote_tab else {
            return false;
        };
        if tab.placeholder || self.session.is_none() {
            return true;
        }
        if tab.reconnecting || tab.lost || tab.error.is_some() {
            return false;
        }
        tab.phase == ConnectPhase::LoadingHistory && self.commit_count() == 0
    }

    pub(super) fn connect_page(&self, design: &Design) -> AnyElement {
        let Some(tab) = &self.remote_tab else {
            return div().into_any_element();
        };
        if tab.placeholder {
            return self.connect_placeholder(design);
        }
        if let Some(error) = &tab.error
            && self.session.is_none()
        {
            return self.connect_failure_page(error, design);
        }
        ui::loading_state(tab.phase.label(), design).into_any_element()
    }

    fn connect_placeholder(&self, design: &Design) -> AnyElement {
        let entity = self.entity.clone();
        self.connect_card(
            t!("remote_repo.placeholder").to_string(),
            Some(t!("remote_repo.placeholder_detail").to_string()),
            design,
        )
        .child(connect_actions(
            entity,
            t!("remote_repo.connect").to_string(),
            "remote-placeholder-connect",
        ))
        .into_any_element()
    }

    fn connect_failure_page(&self, error: &str, design: &Design) -> AnyElement {
        let entity = self.entity.clone();
        self.connect_card(
            t!("remote_repo.failed").to_string(),
            Some(error.to_owned()),
            design,
        )
        .child(connect_actions(
            entity,
            t!("common.retry").to_string(),
            "remote-retry",
        ))
        .into_any_element()
    }

    fn connect_card(&self, title: String, detail: Option<String>, design: &Design) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_3()
            .p_8()
            .bg(design.background)
            .child(
                div()
                    .text_size(design.px(16.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .when_some(detail, |card, detail| {
                card.child(
                    div()
                        .max_w(design.px(640.0))
                        .text_size(design.px(12.0))
                        .text_color(design.muted)
                        .child(detail),
                )
            })
    }

    pub(super) fn connection_banner(
        &self,
        entity: Entity<Self>,
        design: &Design,
    ) -> Option<AnyElement> {
        let tab = self.remote_tab.as_ref()?;
        self.session.as_ref()?;
        if tab.reconnecting {
            return Some(
                ui::banner(ui::BannerKind::Info, tab.phase.label(), design).into_any_element(),
            );
        }
        if let Some(error) = &tab.error {
            let retry = entity.clone();
            return Some(
                ui::banner(ui::BannerKind::Danger, error.clone(), design)
                    .wrap()
                    .action(retry_button(retry, "remote-banner-retry"))
                    .into_any_element(),
            );
        }
        if tab.lost {
            let retry = entity;
            return Some(
                ui::banner(
                    ui::BannerKind::Warning,
                    t!("remote_repo.lost").to_string(),
                    design,
                )
                .action(retry_button(retry, "remote-reconnect"))
                .into_any_element(),
            );
        }
        None
    }

    /// A load failure that means the SSH transport went away. The last
    /// snapshot stays; the banner offers a reconnect.
    pub(super) fn note_connection_lost(&mut self, error: &str) -> bool {
        if !self.is_ssh() || !error.contains(lamina_remote_client::CONNECTION_LOST) {
            return false;
        }
        warn!("remote: connection lost: {error}");
        if let Some(tab) = &mut self.remote_tab {
            tab.lost = true;
            tab.reconnecting = false;
            tab.rtt = None;
        }
        true
    }

    /// The status line's round trip, hidden while a banner already explains
    /// that the connection is down or coming back.
    pub(super) fn connection_quality(&self) -> Option<ConnectionRttStatus> {
        let tab = self.remote_tab.as_ref()?;
        if tab.lost || tab.reconnecting || self.session.is_none() {
            return None;
        }
        tab.rtt.map(connection_rtt_status)
    }

    fn watch_connection_rtt(&mut self, cx: &mut Context<Self>) {
        self._rtt = Some(cx.spawn(async move |this, cx| {
            loop {
                let alive = this.update(cx, |this, cx| {
                    let sample = this
                        .session
                        .as_ref()
                        .and_then(|session| session.connection_rtt());
                    let Some(tab) = this.remote_tab.as_mut() else {
                        return false;
                    };
                    if tab.rtt == sample {
                        return true;
                    }
                    tab.rtt = sample;
                    this.notify_if_active(cx);
                    true
                });
                if !alive.unwrap_or(false) {
                    break;
                }
                cx.background_executor().timer(CONNECTION_RTT_POLL).await;
            }
        }));
    }

    pub(super) fn swallow_unsupported(&self, error: &str) -> bool {
        self.is_ssh() && error.contains(lamina_remote_client::UNSUPPORTED)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn diff_row_count(&self) -> usize {
        self.diff_rows.len()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn connection_lost(&self) -> bool {
        self.remote_tab.as_ref().is_some_and(|tab| tab.lost)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn connect_failure(&self) -> Option<&str> {
        self.remote_tab
            .as_ref()
            .and_then(|tab| tab.error.as_deref())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn files_error(&self) -> Option<&str> {
        self.files_error.as_deref()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn diff_error(&self) -> Option<&str> {
        self.diff_error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn is_connect_placeholder(&self) -> bool {
        self.remote_tab.as_ref().is_some_and(|tab| tab.placeholder)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn history_reset_blocked(&self) -> Option<String> {
        self.history_menu_state()
            .and_then(|state| state.blocked(super::history_ops::HistoryAction::Reset))
    }
}

/// How often a focused window may ask a remote helper for a fresh status.
const REMOTE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// How often the status line copies the idle round trip. The probe itself is
/// slower; this only bounds how long a new sample waits to be drawn.
const CONNECTION_RTT_POLL: Duration = Duration::from_secs(1);

/// Shown as a slow connection above this idle round trip. The status tooltip
/// names the same threshold, so the two have to move together.
pub(super) const SLOW_CONNECTION: Duration = Duration::from_millis(300);

pub(super) struct ConnectionRttStatus {
    pub label: String,
    pub slow: bool,
}

pub(super) fn connection_rtt_status(rtt: Duration) -> ConnectionRttStatus {
    let ms = u64::try_from(rtt.as_millis()).unwrap_or(u64::MAX);
    ConnectionRttStatus {
        label: t!("status.ssh_rtt", ms = ms).to_string(),
        slow: rtt > SLOW_CONNECTION,
    }
}

/// True when a focus refresh should run. An explicit refresh records `last`,
/// so the two do not stack.
pub(super) fn remote_refresh_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= REMOTE_REFRESH_INTERVAL)
}

/// Focus refresh is the stopgap for a helper that could not watch. A live
/// watcher already pushes outside edits, so focusing the window must not
/// ask for another status.
pub(super) fn focus_refresh_applies(degraded: bool, due: bool) -> bool {
    degraded && due
}

fn connect_actions(
    entity: gpui::WeakEntity<RepoView>,
    primary: String,
    id: &'static str,
) -> gpui::Div {
    let retry = entity.clone();
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            ui::button(id, primary)
                .size(ui::ButtonSize::Medium)
                .kind(ui::ButtonKind::Primary)
                .on_click(move |_, _, cx| {
                    if let Some(entity) = retry.upgrade() {
                        entity.update(cx, |view, cx| view.start_remote_connect(cx));
                    }
                }),
        )
        .child(
            ui::button("remote-close", t!("common.close").to_string())
                .size(ui::ButtonSize::Medium)
                .kind(ui::ButtonKind::Ghost)
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(crate::actions::CloseTab), cx);
                }),
        )
}

fn retry_button(entity: Entity<RepoView>, id: &'static str) -> impl IntoElement {
    let label = if id == "remote-reconnect" {
        t!("remote_repo.reconnect").to_string()
    } else {
        t!("common.retry").to_string()
    };
    ui::button(id, label)
        .size(ui::ButtonSize::Tiny)
        .on_click(move |_, _, cx| {
            entity.update(cx, |view, cx| view.start_remote_connect(cx));
        })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gpui::TestAppContext;

    use super::super::Launch;
    use super::super::RepoView;
    #[cfg(unix)]
    use super::super::remote_fixture::{install_host_helper, layout_fake_ssh, wait_for_helper};
    use super::{connection_rtt_status, focus_refresh_applies, remote_refresh_due};
    use crate::remote_open::{RemoteLaunch, RemoteSpec};

    fn placeholder_tab(spec: RemoteSpec) -> super::RemoteTab {
        super::RemoteTab {
            spec,
            ssh: None,
            phase: super::ConnectPhase::Connecting,
            placeholder: true,
            lost: false,
            reconnecting: false,
            buffering: false,
            pending: Vec::new(),
            error: None,
            rtt: None,
        }
    }

    #[test]
    fn a_connected_tab_keeps_the_identity_the_dialog_produced() {
        // The helper resolves `~` on the host; the recorded identity must stay
        // the user's spelling, or reopening the same dialog entry would add a
        // second tab instead of activating this one.
        let spec = RemoteSpec {
            host: "git@example.com".into(),
            port: None,
            path: "~/repo".into(),
        };
        let tab = placeholder_tab(spec.clone());
        assert_eq!(
            super::stored_identity(Some(&tab)),
            Some(spec.stored_path()),
            "the identity must be what place_remote compares against"
        );
        assert!(
            crate::remote_open::parse_stored(&spec.stored_path()).is_some(),
            "the identity must restore as the same remote repository"
        );
        assert_eq!(super::stored_identity(None), None);
    }

    #[test]
    fn connection_rtt_labels_a_slow_link_above_300_ms() {
        rust_i18n::set_locale("en");
        let fine = connection_rtt_status(Duration::from_millis(42));
        assert!(!fine.slow);
        assert_eq!(fine.label, t!("status.ssh_rtt", ms = 42u64).to_string());

        let edge = connection_rtt_status(Duration::from_millis(300));
        assert!(!edge.slow);
        assert_eq!(edge.label, t!("status.ssh_rtt", ms = 300u64).to_string());

        let slow = connection_rtt_status(Duration::from_millis(301));
        assert!(slow.slow);
        assert_eq!(slow.label, t!("status.ssh_rtt", ms = 301u64).to_string());
    }

    #[test]
    fn a_focus_refresh_waits_five_seconds() {
        let now = Instant::now();
        assert!(remote_refresh_due(None, now));
        assert!(!remote_refresh_due(Some(now), now));
        assert!(!remote_refresh_due(Some(now), now + Duration::from_secs(4)));
        assert!(remote_refresh_due(Some(now), now + Duration::from_secs(5)));
    }

    #[test]
    fn focus_refresh_runs_only_when_the_watcher_is_down() {
        let now = Instant::now();
        assert!(!focus_refresh_applies(false, remote_refresh_due(None, now)));
        assert!(focus_refresh_applies(true, remote_refresh_due(None, now)));
        assert!(!focus_refresh_applies(
            true,
            remote_refresh_due(Some(now), now)
        ));
    }
    #[cfg(unix)]
    use crate::remote_open::{SshOverride, unavailable_reason};

    fn open_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(crate::settings::Settings::default());
        });
    }

    #[gpui::test]
    fn a_restored_remote_tab_shows_connect_without_dialling(cx: &mut TestAppContext) {
        open_app(cx);
        let window = cx.add_window(|window, cx| {
            RepoView::new(
                Launch::Remote(RemoteLaunch {
                    spec: RemoteSpec {
                        host: "git@example.com".into(),
                        port: None,
                        path: "/home/repo".into(),
                    },
                    auto_connect: false,
                    ssh: None,
                }),
                window,
                cx,
            )
        });
        let view = window.root(cx).expect("the window's root view");
        cx.run_until_parked();
        cx.read(|cx| {
            let view = view.read(cx);
            assert!(view.session.is_none(), "startup must not dial");
            assert!(view.is_connect_placeholder());
            assert!(
                view.remote_not_connected(),
                "the title bar must not say loading"
            );
            let title = view.window_title();
            assert!(
                title.contains(t!("title_bar.not_connected").as_ref()),
                "window title should match the branch chip, got {title}"
            );
            assert!(
                !title.contains(&format!("({})", t!("title_bar.loading"))),
                "a Connect placeholder is not still loading, got {title}"
            );
            assert!(view.is_ssh());
            assert_eq!(view.commit_count(), 0);
        });
    }

    #[cfg(unix)]
    #[gpui::test]
    fn a_remote_merge_commit_counts_files_against_its_first_parent(cx: &mut TestAppContext) {
        let repo = lamina_core::test_support::TestRepo::new();
        repo.write("base.txt", "base\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "side"]);
        repo.write("one.txt", "one\n");
        repo.write("two.txt", "two\n");
        repo.commit("side");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("main.txt", "main\n");
        repo.commit("main");
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        let merge = repo.head();
        cx.dispatcher.allow_parking();

        let scratch = super::super::remote_fixture::scratch_dir("lamina-app-ssh-count");
        let laid = layout_fake_ssh(&scratch);
        let helper = wait_for_helper();
        install_host_helper(&helper, &laid.helpers);

        open_app(cx);
        let window = cx.add_window(|window, cx| {
            RepoView::new(
                Launch::Remote(RemoteLaunch {
                    spec: RemoteSpec {
                        host: "git.example.com".into(),
                        port: None,
                        path: repo.path().to_string_lossy().into_owned(),
                    },
                    auto_connect: true,
                    ssh: Some(SshOverride {
                        program: laid.ssh.clone(),
                        helper_dir: laid.helpers.clone(),
                    }),
                }),
                window,
                cx,
            )
        });
        let view = window.root(cx).expect("the window's root view");

        let deadline = Instant::now() + Duration::from_secs(90);
        while cx.read(|cx| view.read(cx).commit_count()) == 0 {
            assert!(Instant::now() < deadline, "the remote history did not load");
            std::thread::sleep(Duration::from_millis(50));
            cx.run_until_parked();
        }

        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.observe_visible_changed_file_counts(vec![merge], cx);
            });
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            std::thread::sleep(Duration::from_millis(50));
            // The count worker waits on a debounce timer; a test clock only
            // moves when the test moves it.
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            let count = cx.read(|cx| view.read(cx).commit_changed_file_count(merge));
            if let Some(count) = count {
                assert_eq!(
                    count, 2,
                    "a merge counts the two files its second parent added"
                );
                break;
            }
            if Instant::now() > deadline {
                let log = std::fs::read_to_string(&laid.log).unwrap_or_default();
                panic!("the changed-file count never arrived\nssh log:\n{log}");
            }
        }

        // The details pane reads the file list separately. A merge must not
        // look empty there either.
        cx.update(|cx| {
            view.update(cx, |view, cx| view.select(Some(0), cx));
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            std::thread::sleep(Duration::from_millis(50));
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            let files = cx.read(|cx| view.read(cx).files.as_ref().map(|files| files.len()));
            if let Some(len) = files {
                assert_eq!(
                    len, 2,
                    "the merge changed two files against its first parent"
                );
                break;
            }
            if Instant::now() > deadline {
                panic!("the merge's changed-file list never loaded");
            }
        }

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[cfg(unix)]
    #[gpui::test]
    fn a_remote_tab_shows_history_a_diff_and_a_lost_connection(cx: &mut TestAppContext) {
        let repo = lamina_core::test_support::TestRepo::new();
        repo.write("README.md", "alpha\n");
        repo.commit("alpha");
        // The helper's reader thread delivers events. The test scheduler treats
        // that wake as non-deterministic unless parking is allowed.
        cx.dispatcher.allow_parking();

        let scratch = super::super::remote_fixture::scratch_dir("lamina-app-ssh");
        let laid = layout_fake_ssh(&scratch);
        let helper = wait_for_helper();
        install_host_helper(&helper, &laid.helpers);

        open_app(cx);
        let window = cx.add_window(|window, cx| {
            RepoView::new(
                Launch::Remote(RemoteLaunch {
                    spec: RemoteSpec {
                        host: "git.example.com".into(),
                        port: None,
                        path: repo.path().to_string_lossy().into_owned(),
                    },
                    auto_connect: true,
                    ssh: Some(SshOverride {
                        program: laid.ssh.clone(),
                        helper_dir: laid.helpers.clone(),
                    }),
                }),
                window,
                cx,
            )
        });
        let view = window.root(cx).expect("the window's root view");

        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            std::thread::sleep(Duration::from_millis(50));
            cx.run_until_parked();
            let (count, failure) = cx.read(|cx| {
                let view = view.read(cx);
                (
                    view.commit_count(),
                    view.connect_failure().map(str::to_owned),
                )
            });
            if let Some(failure) = failure {
                let log = std::fs::read_to_string(&laid.log).unwrap_or_default();
                panic!("connect failed: {failure}\nssh log:\n{log}");
            }
            if count > 0 {
                break;
            }
            if Instant::now() > deadline {
                let log = std::fs::read_to_string(&laid.log).unwrap_or_default();
                panic!("timed out waiting for history\nssh log:\n{log}");
            }
        }

        // The window's own update already holds the root view, so the selection
        // goes through the app context instead.
        cx.update(|cx| {
            view.update(cx, |view, cx| view.select(Some(0), cx));
        });

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            std::thread::sleep(Duration::from_millis(50));
            cx.run_until_parked();
            let (rows, files_error, diff_error, failure) = cx.read(|cx| {
                let view = view.read(cx);
                (
                    view.diff_row_count(),
                    view.files_error().map(str::to_owned),
                    view.diff_error().map(str::to_owned),
                    view.connect_failure().map(str::to_owned),
                )
            });
            if rows > 0 {
                break;
            }
            if files_error.is_some() || diff_error.is_some() || failure.is_some() {
                panic!(
                    "diff did not load: files={files_error:?} diff={diff_error:?} connect={failure:?}"
                );
            }
            if Instant::now() > deadline {
                panic!(
                    "timed out waiting for the commit diff: files={files_error:?} diff={diff_error:?}"
                );
            }
        }

        let reason = unavailable_reason();
        cx.read(|cx| {
            let view = view.read(cx);
            assert_eq!(view.history_reset_blocked(), None);
            assert_eq!(
                view.history_menu_state()
                    .and_then(|state| {
                        state.blocked(super::super::history_ops::HistoryAction::CreateTag)
                    })
                    .as_deref(),
                Some(reason.as_str())
            );
            assert!(view.commit_count() > 0);
            // The title names HEAD's branch, which the connect worker's
            // snapshot already carried: no extra reader round trip is needed.
            let title = view.window_title();
            assert!(
                title.contains("main"),
                "the title should name HEAD's branch, got {title}"
            );
        });

        let pid = std::fs::read_to_string(&laid.pid)
            .expect("the session recorded its pid")
            .trim()
            .to_owned();
        let killed = std::process::Command::new("kill")
            .arg(&pid)
            .status()
            .expect("kill the helper");
        assert!(killed.success(), "kill {pid} failed");

        let rows_before = cx.read(|cx| view.read(cx).commit_count());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            std::thread::sleep(Duration::from_millis(50));
            cx.run_until_parked();
            if cx.read(|cx| view.read(cx).connection_lost()) {
                break;
            }
            if Instant::now() > deadline {
                let log = std::fs::read_to_string(&laid.log).unwrap_or_default();
                panic!("timed out waiting for the connection-lost banner\nssh log:\n{log}");
            }
        }
        cx.read(|cx| {
            let view = view.read(cx);
            assert!(view.connection_lost());
            assert_eq!(view.commit_count(), rows_before, "the last snapshot stays");
            assert!(view.connect_failure().is_none());
        });

        let _ = std::fs::remove_dir_all(&scratch);
    }
}
