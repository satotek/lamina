//! A repository that lives on another machine.
//!
//! Connecting runs on a worker thread. That thread, and the session's reader
//! thread, only push [`ConnectMsg`] values. One foreground task drains them.
//! The reader callback must not touch GPUI or drop the session: doing either
//! deadlocks the read loop.

use std::sync::mpsc;

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement as _, Styled as _, Window, div,
};
use strata_core::session::{RepoLocation, RepositorySession, SessionEvent};

use super::RepoView;
use crate::remote_open::{
    ConnectMsg, ConnectPhase, RemoteLaunch, spawn_connect, ssh_uri, tab_title, unavailable_reason,
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
        self.title = crate::data::window_title(&name, None);
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
        });
        // No local checkout, so the commit-graph offer and the config viewer
        // have nothing to key off.
        self.repository_key = None;
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
        let (sender, mut receiver) = futures_mpsc::unbounded();
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
            ConnectMsg::Phase(phase) => {
                if let Some(tab) = &mut self.remote_tab {
                    tab.phase = phase;
                }
                self.notify_if_active(cx);
            }
            ConnectMsg::Askpass {
                prompt,
                secret,
                reply,
            } => self.prompt_remote_askpass(prompt, secret, reply, cx),
            ConnectMsg::Failed(error) => {
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
        let Some(window) = self.window_handle else {
            let _ = reply.send(None);
            return;
        };
        // The modal owns one sender. This clone answers only when the modal
        // could not be opened, so the blocked helper is not left waiting.
        let fallback = reply.clone();
        let opened = window.update(cx, |_, window, cx| {
            crate::ui::askpass_prompt(
                window,
                cx,
                t!("history.authentication").to_string(),
                prompt,
                secret,
                move |answer, _, _| {
                    let _ = reply.send(answer);
                },
            );
        });
        if opened.is_err() {
            let _ = fallback.send(None);
        }
    }

    fn install_remote_session(
        &mut self,
        session: Box<dyn RepositorySession>,
        cx: &mut Context<Self>,
    ) {
        // The handshake reports an SSH location. A local path here would be a
        // helper bug; the tab keeps the name the dialog already showed.
        let (name, stored) = match session.location() {
            RepoLocation::Ssh { host, port, path } => (
                tab_title(&host, &path),
                Some(std::path::PathBuf::from(ssh_uri(&host, port, &path))),
            ),
            RepoLocation::Local(_) => (self.name.clone().unwrap_or_default(), None),
        };
        let head = session.reader().head().ok();
        self.name = Some(name.clone());
        self.title = crate::data::window_title(&name, head.as_ref());
        let old = self.session.replace(session);
        if let Some(tab) = &mut self.remote_tab {
            tab.phase = ConnectPhase::LoadingHistory;
            tab.buffering = false;
            tab.lost = false;
            tab.reconnecting = false;
            tab.error = None;
            tab.placeholder = false;
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

    pub(crate) fn is_ssh(&self) -> bool {
        self.remote_tab.is_some()
            || self
                .session
                .as_ref()
                .is_some_and(|session| matches!(session.location(), RepoLocation::Ssh { .. }))
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
        if !self.is_ssh() || !error.contains(strata_remote_client::CONNECTION_LOST) {
            return false;
        }
        if let Some(tab) = &mut self.remote_tab {
            tab.lost = true;
            tab.reconnecting = false;
        }
        true
    }

    pub(super) fn swallow_unsupported(&self, error: &str) -> bool {
        self.is_ssh() && error.contains(strata_remote_client::UNSUPPORTED)
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
    #[cfg(unix)]
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use gpui::TestAppContext;

    use super::super::Launch;
    use super::super::RepoView;
    use crate::remote_open::{RemoteLaunch, RemoteSpec};
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
            assert!(view.is_ssh());
            assert_eq!(view.commit_count(), 0);
        });
    }

    #[cfg(unix)]
    #[gpui::test]
    fn a_remote_tab_shows_history_a_diff_and_a_lost_connection(cx: &mut TestAppContext) {
        let repo = strata_core::test_support::TestRepo::new();
        repo.write("README.md", "alpha\n");
        repo.commit("alpha");
        // The helper's reader thread delivers events. The test scheduler treats
        // that wake as non-deterministic unless parking is allowed.
        cx.dispatcher.allow_parking();

        let scratch = std::env::temp_dir().join(format!(
            "strata-app-ssh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&scratch).expect("scratch dir");
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
            assert_eq!(
                view.history_reset_blocked().as_deref(),
                Some(reason.as_str())
            );
            assert!(view.commit_count() > 0);
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

    #[cfg(unix)]
    struct LaidOut {
        ssh: PathBuf,
        helpers: PathBuf,
        log: PathBuf,
        pid: PathBuf,
    }

    #[cfg(unix)]
    fn layout_fake_ssh(scratch: &Path) -> LaidOut {
        let bin = scratch.join("bin");
        let home = scratch.join("home");
        let helpers = scratch.join("helpers");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&helpers).unwrap();
        let log = scratch.join("ssh.log");
        let pid = scratch.join("session.pid");
        let uname = "#!/bin/sh\nif [ \"$1\" = \"-s\" ]; then\n  echo Linux\n  exit 0\nfi\nif [ \"$1\" = \"-m\" ]; then\n  echo x86_64\n  exit 0\nfi\necho unknown\nexit 0\n";
        write_exec(&bin.join("uname"), uname);
        let script = format!(
            r#"#!/bin/sh
export HOME={home}
unset XDG_CACHE_HOME
export PATH={bin}:$PATH
LOG={log}
{{
  printf 'CMD'
  for arg in "$@"; do
    printf ' <%s>' "$arg"
  done
  printf '\n'
}} >> "$LOG"
after=0
cmd=
for arg in "$@"; do
  if [ "$after" = 1 ]; then
    cmd="$cmd $arg"
    continue
  fi
  if [ "$arg" = "--" ]; then
    after=1
  fi
done
cmd=${{cmd# }}
if [ -z "$cmd" ]; then
  echo "fake ssh: no remote command" >&2
  exit 2
fi
case "$cmd" in
  *--stdio*)
    # A background command would have its stdin pointed at /dev/null, and the
    # helper would exit before the handshake. Replace this process instead, so
    # the recorded pid is the helper and killing it closes the pipes.
    echo $$ > {pid}
    # The arguments are already shell-quoted by the client. Splitting them
    # here is only safe because this test's paths have no spaces.
    set -- $cmd
    exec "$@"
    ;;
esac
exec sh -c "$cmd"
"#,
            home = sh_quote(&home.display().to_string()),
            bin = sh_quote(&bin.display().to_string()),
            log = sh_quote(&log.display().to_string()),
            pid = sh_quote(&pid.display().to_string()),
        );
        let ssh = bin.join("ssh");
        write_exec(&ssh, &script);
        LaidOut {
            ssh,
            helpers,
            log,
            pid,
        }
    }

    #[cfg(unix)]
    fn sh_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    fn write_exec(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, text).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// The workspace build places `strata-remote` next to this test binary's
    /// `deps` directory. A package test can start before that binary is linked.
    #[cfg(unix)]
    fn wait_for_helper() -> PathBuf {
        let exe = std::env::current_exe().expect("current exe");
        let mut dir = exe.parent().expect("binary directory").to_path_buf();
        if dir.file_name().is_some_and(|name| name == "deps") {
            dir.pop();
        }
        let helper = dir.join("strata-remote");
        let deadline = Instant::now() + Duration::from_secs(180);
        while !helper.is_file() {
            if Instant::now() > deadline {
                panic!("strata-remote was not built at {}", helper.display());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        helper
    }

    #[cfg(unix)]
    fn install_host_helper(source: &Path, dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let dest = dir.join("strata-remote-x86_64-unknown-linux-musl");
        std::fs::copy(source, &dest).expect("copy helper");
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).unwrap();
    }
}
