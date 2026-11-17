//! Opening a repository that lives on another machine.
//!
//! The dialog only collects a host and a path. Connecting runs on a worker
//! thread: the worker may block in `ssh`, and the askpass callback may block
//! until the modal answers. Neither of those threads touches GPUI. They push
//! [`ConnectMsg`] values into an unbounded channel that one foreground task
//! drains, the same way a local session delivers [`SessionEvent`]s.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    Styled as _, WeakEntity, Window, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputState};
use log::{info, warn};
use strata_core::askpass::{Answer, Prompt};
use strata_core::session::{RepositorySession, SessionEvent};
use strata_remote_client::{
    InstallOptions, RemoteSession, RemoteTarget, SshOptions, install_helper,
};

use crate::settings::Settings;
use crate::style::Design;
use crate::ui;
use crate::workspace::Workspace;

/// How long a password prompt waits for the modal before `ssh` is cancelled.
const ASKPASS_WAIT: Duration = Duration::from_secs(150);

/// What the user typed, after a pasted URL has been split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteSpec {
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
}

impl RemoteSpec {
    pub(crate) fn ssh_uri(&self) -> String {
        ssh_uri(&self.host, self.port, &self.path)
    }

    pub(crate) fn stored_path(&self) -> PathBuf {
        PathBuf::from(self.ssh_uri())
    }
}

/// A test points `ssh` at a local script and the helper directory at a copy
/// of `strata-remote`. Production leaves this unset.
#[derive(Clone, Debug)]
pub(crate) struct SshOverride {
    pub program: PathBuf,
    pub helper_dir: PathBuf,
}

/// How a remote tab should start. Saved windows restore with `auto_connect`
/// false so launching the app does not dial out.
#[derive(Clone, Debug)]
pub(crate) struct RemoteLaunch {
    pub spec: RemoteSpec,
    pub auto_connect: bool,
    pub ssh: Option<SshOverride>,
}

/// Progress shown in the tab while `ssh` is still coming up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectPhase {
    Connecting,
    PreparingHelper,
    Opening,
    LoadingHistory,
    Ready,
}

impl ConnectPhase {
    pub(crate) fn label(self) -> String {
        match self {
            Self::Connecting => t!("remote_repo.phase.connecting").to_string(),
            Self::PreparingHelper => t!("remote_repo.phase.helper").to_string(),
            Self::Opening => t!("remote_repo.phase.opening").to_string(),
            Self::LoadingHistory => t!("remote_repo.phase.history").to_string(),
            Self::Ready => t!("remote_repo.phase.ready").to_string(),
        }
    }
}

/// Messages the worker sends. The askpass reply channel is answered by the
/// modal; the worker blocks on it, the UI thread does not.
pub(crate) enum ConnectMsg {
    Phase(ConnectPhase),
    Askpass {
        prompt: String,
        secret: bool,
        reply: mpsc::Sender<Option<String>>,
    },
    Failed(String),
    Ready(Box<dyn RepositorySession>),
    Session(SessionEvent),
}

/// Why a write, or anything that needs a local checkout, cannot run.
pub(crate) fn unavailable_reason() -> String {
    t!("remote_repo.unavailable").to_string()
}

/// `ssh://host[:port]/path` without a doubled slash. A stored path of
/// `/home/repo` becomes `ssh://host/home/repo`, which parses back to the same
/// absolute path.
pub(crate) fn ssh_uri(host: &str, port: Option<u16>, path: &str) -> String {
    let body = path.strip_prefix('/').unwrap_or(path);
    match port {
        Some(port) => format!("ssh://{host}:{port}/{body}"),
        None => format!("ssh://{host}/{body}"),
    }
}

/// Tab and window title: `host:repo-name`. The host is the alias or
/// `user@host` the user addressed, not a path on this machine.
pub(crate) fn tab_title(host: &str, path: &str) -> String {
    let repo = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_owned());
    format!("{host}:{repo}")
}

/// The embedded terminal's tab title. The local tab strip has room for the
/// whole path, and the path is what tells two terminals on one host apart.
#[cfg(feature = "terminal")]
pub(crate) fn terminal_tab_title(spec: &RemoteSpec) -> String {
    format!("{}:{}", spec.host, spec.path)
}

/// The `ssh` a tab dials with. The terminal runs the same one, so a test that
/// points the session at a script reaches the terminal too.
pub(crate) fn ssh_program(ssh: Option<&SshOverride>) -> PathBuf {
    match ssh {
        Some(ssh) => ssh.program.clone(),
        None => PathBuf::from("ssh"),
    }
}

/// The program and arguments the embedded terminal runs for a remote tab: the
/// session's own `ssh` invocation, with a tty and a remote command that enters
/// the repository and becomes the host's login shell.
#[cfg(feature = "terminal")]
pub(crate) fn terminal_command(
    spec: &RemoteSpec,
    ssh: Option<&SshOverride>,
) -> (String, Vec<String>) {
    let program = ssh_program(ssh);
    let argv = strata_remote_client::terminal_argv(&program, spec.port, &spec.host, &spec.path);
    let mut parts = argv.iter().map(|part| part.to_string_lossy().into_owned());
    let program = parts.next().unwrap_or_default();
    (program, parts.collect())
}

pub(crate) fn is_stored_remote(path: &Path) -> bool {
    path.to_string_lossy().starts_with("ssh://")
}

pub(crate) fn parse_stored(path: &Path) -> Option<RemoteSpec> {
    parse_stored_strict(path).ok().flatten()
}

/// `Ok(None)` for a local path. An `ssh://` value that does not parse is an
/// error, so the picker does not try to stat it.
pub(crate) fn parse_stored_strict(path: &Path) -> Result<Option<RemoteSpec>, String> {
    let text = path.to_string_lossy();
    let Some(rest) = text.strip_prefix("ssh://") else {
        return Ok(None);
    };
    parse_ssh_url(rest, "").map(Some)
}

/// Splits the dialog's two fields. A single `ssh://` URL or `host:/path`
/// pasted into the host field wins over whatever is in the path field.
pub(crate) fn parse_remote_fields(
    host_field: &str,
    path_field: &str,
) -> Result<RemoteSpec, String> {
    let host_field = host_field.trim();
    let path_field = path_field.trim();
    if host_field.is_empty() {
        return Err(t!("remote_repo.err.host").to_string());
    }
    if let Some(rest) = host_field.strip_prefix("ssh://") {
        return parse_ssh_url(rest, path_field);
    }
    if let Some((authority, path)) = split_scp(host_field) {
        let (host, port) = split_authority(authority)?;
        validate_host(&host)?;
        validate_path(path)?;
        return Ok(RemoteSpec {
            host,
            port,
            path: path.to_owned(),
        });
    }
    let (host, port) = split_authority(host_field)?;
    validate_host(&host)?;
    validate_path(path_field)?;
    Ok(RemoteSpec {
        host,
        port,
        path: path_field.to_owned(),
    })
}

/// `STRATA_REMOTE_HELPER_DIR` when it is set, otherwise `remote-helpers/`
/// beside the executable. A release build is expected to put the musl
/// binaries there; packaging them is not this crate's job.
pub(crate) fn resolve_helper_dir() -> Result<PathBuf, String> {
    if let Ok(value) = std::env::var(strata_remote_client::HELPER_DIR_ENV)
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(dir) = executable.parent()
    {
        let bundled = dir.join("remote-helpers");
        if bundled.is_dir() {
            return Ok(bundled);
        }
    }
    Err(t!("remote_repo.err.no_helper").to_string())
}

/// Installs the helper, then opens the repository. The caller drains `sender`
/// on the UI thread. Dropping `sender`'s receiver makes later sends fail,
/// which is how a replaced attempt is discarded.
pub(crate) fn spawn_connect(
    spec: RemoteSpec,
    ssh: Option<SshOverride>,
    sender: UnboundedSender<ConnectMsg>,
) {
    let worker = sender.clone();
    let built = std::thread::Builder::new()
        .name("strata-remote-connect".to_owned())
        .spawn(move || connect_worker(spec, ssh, worker));
    if let Err(error) = built {
        let _ = sender.unbounded_send(ConnectMsg::Failed(error.to_string()));
    }
}

fn connect_worker(spec: RemoteSpec, ssh: Option<SshOverride>, sender: UnboundedSender<ConnectMsg>) {
    info!("remote: connecting to {} at {}", spec.host, spec.path);
    let _ = sender.unbounded_send(ConnectMsg::Phase(ConnectPhase::Connecting));
    let program = ssh_program(ssh.as_ref());
    let helper_dir = match ssh.as_ref() {
        Some(ssh) => ssh.helper_dir.clone(),
        None => match resolve_helper_dir() {
            Ok(dir) => dir,
            Err(error) => {
                warn!("remote: cannot connect to {}: {error}", spec.host);
                let _ = sender.unbounded_send(ConnectMsg::Failed(error));
                return;
            }
        },
    };
    let askpass_program = std::env::current_exe().ok();
    let askpass = askpass_handler(sender.clone());
    let _ = sender.unbounded_send(ConnectMsg::Phase(ConnectPhase::PreparingHelper));
    let install = InstallOptions {
        program: program.clone(),
        host: spec.host.clone(),
        port: spec.port,
        helper_dir: Some(helper_dir.clone()),
        askpass_program: askpass_program.clone(),
        askpass: Some(Arc::clone(&askpass)),
    };
    let helper = match install_helper(&install) {
        Ok(helper) => {
            info!("remote: helper ready at {helper} on {}", spec.host);
            helper
        }
        Err(error) => {
            warn!("remote: helper install failed for {}: {error}", spec.host);
            let _ = sender.unbounded_send(ConnectMsg::Failed(error.to_string()));
            return;
        }
    };
    let _ = sender.unbounded_send(ConnectMsg::Phase(ConnectPhase::Opening));
    let options = SshOptions {
        program,
        helper_dir: Some(helper_dir),
        askpass_program,
        askpass: Some(askpass),
    };
    let target = match spec.port {
        Some(port) => RemoteTarget::new(spec.host.clone()).with_port(port),
        None => RemoteTarget::new(spec.host.clone()),
    };
    let events = sender.clone();
    // The callback runs on the client's reader thread. It only enqueues.
    // Blocking here, or dropping the session here, deadlocks the read loop.
    let connected = RemoteSession::open_ssh(
        target,
        spec.path,
        helper,
        move |event| {
            let _ = events.unbounded_send(ConnectMsg::Session(event));
        },
        options,
    );
    match connected {
        Ok(session) => {
            info!("remote: connected to {}", spec.host);
            let _ = sender.unbounded_send(ConnectMsg::Ready(Box::new(session)));
        }
        Err(error) => {
            warn!("remote: connect failed for {}: {error}", spec.host);
            let _ = sender.unbounded_send(ConnectMsg::Failed(error.to_string()));
        }
    }
}

fn askpass_handler(sender: UnboundedSender<ConnectMsg>) -> strata_remote_client::AskpassHandler {
    Arc::new(move |prompt: Prompt| {
        let (reply_tx, reply_rx) = mpsc::channel();
        if sender
            .unbounded_send(ConnectMsg::Askpass {
                prompt: prompt.text,
                secret: prompt.secret,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        match reply_rx.recv_timeout(ASKPASS_WAIT) {
            Ok(Some(text)) => Some(Answer::from_string(text)),
            _ => None,
        }
    })
}

fn parse_ssh_url(rest: &str, path_field: &str) -> Result<RemoteSpec, String> {
    let (authority, url_path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    if authority.is_empty() {
        return Err(t!("remote_repo.err.host").to_string());
    }
    let (host, port) = split_authority(authority)?;
    validate_host(&host)?;
    let path = match url_path {
        Some(path) if !path.is_empty() => normalize_url_path(path),
        _ => path_field.to_owned(),
    };
    validate_path(&path)?;
    Ok(RemoteSpec { host, port, path })
}

/// `host:/abs` and `user@host:~/repo`. A colon that introduces a port is left
/// for [`split_authority`].
fn split_scp(value: &str) -> Option<(&str, &str)> {
    let bytes = value.as_bytes();
    for index in 0..bytes.len().saturating_sub(1) {
        if bytes[index] == b':' && (bytes[index + 1] == b'/' || bytes[index + 1] == b'~') {
            let authority = &value[..index];
            let path = &value[index + 1..];
            if authority.is_empty() || path.is_empty() {
                return None;
            }
            return Some((authority, path));
        }
    }
    None
}

/// `user@host:22` and `host:2222`. Anything that is not a numeric port stays
/// part of the host, so an ssh config alias may contain a colon.
fn split_authority(value: &str) -> Result<(String, Option<u16>), String> {
    let Some((left, right)) = value.rsplit_once(':') else {
        return Ok((value.to_owned(), None));
    };
    if left.is_empty() || !right.bytes().all(|byte| byte.is_ascii_digit()) || right.is_empty() {
        return Ok((value.to_owned(), None));
    }
    let port = right
        .parse::<u16>()
        .map_err(|_| t!("remote_repo.err.port").to_string())?;
    Ok((left.to_owned(), Some(port)))
}

fn normalize_url_path(path: &str) -> String {
    if path.starts_with('/') || path.starts_with('~') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        Err(t!("remote_repo.err.host").to_string())
    } else {
        Ok(())
    }
}

fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(t!("remote_repo.err.path").to_string());
    }
    if path.starts_with('/') || path.starts_with("~/") || path == "~" {
        return Ok(());
    }
    Err(t!("remote_repo.err.path_shape").to_string())
}

struct RemoteDialog {
    workspace: WeakEntity<Workspace>,
    host: Entity<InputState>,
    path: Entity<InputState>,
    error: Option<String>,
}

pub(crate) fn open_dialog(workspace: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|cx| RemoteDialog::new(workspace.downgrade(), window, cx));
    let focus = dialog.read(cx).host.focus_handle(cx);
    let content = dialog.clone();
    let confirm = dialog.clone();
    window.open_dialog(cx, move |card, _, _| {
        let content = content.clone();
        let confirm = confirm.clone();
        card.title(t!("remote_repo.title").to_string())
            .w(px(560.0))
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                DialogFooter::new()
                    .child(
                        ui::button("remote-repo-cancel", t!("common.cancel").to_string())
                            .size(ui::ButtonSize::Medium)
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child({
                        let confirm = confirm.clone();
                        ui::button("remote-repo-connect", t!("remote_repo.connect").to_string())
                            .size(ui::ButtonSize::Medium)
                            .kind(ui::ButtonKind::Primary)
                            .on_click(move |_, window, cx| {
                                confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                            })
                    }),
            )
            .on_ok(move |_, window, cx| {
                confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                // `submit` closes the dialog when the fields are valid. Returning
                // false keeps an invalid form open instead of closing it twice.
                false
            })
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

impl RemoteDialog {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let host = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("remote_repo.host_placeholder").to_string())
        });
        let path = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("remote_repo.path_placeholder").to_string())
        });
        Self {
            workspace,
            host,
            path,
            error: None,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.host.read(cx).value().to_string();
        let path = self.path.read(cx).value().to_string();
        let spec = match parse_remote_fields(&host, &path) {
            Ok(spec) => spec,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(workspace) = self.workspace.upgrade() else {
            window.close_dialog(cx);
            return;
        };
        if let Err(error) = workspace.update(cx, |workspace, cx| {
            workspace.open_remote(spec, true, None, window, cx)
        }) {
            self.error = Some(error);
            cx.notify();
            return;
        }
        window.close_dialog(cx);
    }
}

impl Render for RemoteDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let mut form = div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(labeled_field(
                t!("remote_repo.host").to_string(),
                &self.host,
                &design,
            ))
            .child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(t!("remote_repo.host_hint").to_string()),
            )
            .child(labeled_field(
                t!("remote_repo.path").to_string(),
                &self.path,
                &design,
            ));
        if let Some(error) = &self.error {
            form = form.child(ui::banner(ui::BannerKind::Danger, error.clone(), &design).wrap());
        }
        form
    }
}

fn labeled_field(label: String, input: &Entity<InputState>, design: &Design) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .child(
            div()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(label),
        )
        .child(Input::new(input).w_full().bg(design.field))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(host: &str, port: Option<u16>, path: &str) -> RemoteSpec {
        RemoteSpec {
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        }
    }

    #[test]
    fn parses_user_host_port_and_a_separate_path() {
        assert_eq!(
            parse_remote_fields("git@example.com:2222", "/srv/repo").unwrap(),
            spec("git@example.com", Some(2222), "/srv/repo")
        );
        assert_eq!(
            parse_remote_fields("example.com", "~/repo").unwrap(),
            spec("example.com", None, "~/repo")
        );
        assert_eq!(
            parse_remote_fields("alias", "~").unwrap(),
            spec("alias", None, "~")
        );
    }

    #[test]
    fn a_pasted_url_or_scp_path_fills_both_fields() {
        assert_eq!(
            parse_remote_fields("ssh://git@example.com:22/srv/repo", "").unwrap(),
            spec("git@example.com", Some(22), "/srv/repo")
        );
        assert_eq!(
            parse_remote_fields("ssh://example.com/home/repo", "/ignored").unwrap(),
            spec("example.com", None, "/home/repo")
        );
        assert_eq!(
            parse_remote_fields("example.com:/srv/repo", "").unwrap(),
            spec("example.com", None, "/srv/repo")
        );
        assert_eq!(
            parse_remote_fields("git@example.com:~/repo", "/ignored").unwrap(),
            spec("git@example.com", None, "~/repo")
        );
    }

    #[test]
    fn rejects_an_empty_host_or_a_relative_path() {
        assert!(parse_remote_fields("  ", "/srv/repo").is_err());
        assert!(parse_remote_fields("example.com", "").is_err());
        assert!(parse_remote_fields("example.com", "repo").is_err());
        assert!(parse_remote_fields("example.com:99999", "/srv/repo").is_err());
    }

    #[test]
    fn ssh_uri_round_trips_without_a_double_slash() {
        let stored = ssh_uri("example.com", Some(22), "/home/repo");
        assert_eq!(stored, "ssh://example.com:22/home/repo");
        assert_eq!(
            parse_stored(Path::new(&stored)).unwrap(),
            spec("example.com", Some(22), "/home/repo")
        );
        assert_eq!(ssh_uri("alias", None, "~/repo"), "ssh://alias/~/repo");
        assert!(is_stored_remote(Path::new("ssh://alias/home/repo")));
        assert!(!is_stored_remote(Path::new("/home/repo")));
    }

    #[test]
    fn the_unavailable_reason_is_the_catalogue_string() {
        assert_eq!(
            unavailable_reason(),
            "not available for remote repositories yet"
        );
    }

    #[test]
    fn tab_title_keeps_the_user_and_the_repository_name() {
        assert_eq!(
            tab_title("git@example.com", "/home/repo"),
            "git@example.com:repo"
        );
        assert_eq!(tab_title("alias", "~/repo"), "alias:repo");
    }

    #[cfg(feature = "terminal")]
    #[test]
    fn the_terminal_runs_the_sessions_ssh_with_a_login_shell() {
        let target = spec("git@example.com", Some(2222), "/srv/my repo");
        let ssh = SshOverride {
            program: PathBuf::from("/opt/fake-ssh"),
            helper_dir: PathBuf::from("/tmp/helpers"),
        };
        let (program, args) = terminal_command(&target, Some(&ssh));
        assert_eq!(
            program, "/opt/fake-ssh",
            "a test override has to reach the terminal as well"
        );
        assert_eq!(args[0], "-t");
        assert!(args.contains(&"2222".to_owned()));
        assert!(args.contains(&"git@example.com".to_owned()));
        assert_eq!(
            args.last().map(String::as_str),
            Some("cd '/srv/my repo' && exec \"$SHELL\" -l")
        );

        assert_eq!(terminal_command(&target, None).0, "ssh");
        assert_eq!(ssh_program(None), PathBuf::from("ssh"));
    }

    #[cfg(feature = "terminal")]
    #[test]
    fn the_terminal_tab_title_names_the_host_and_the_whole_path() {
        assert_eq!(
            terminal_tab_title(&spec("git@example.com", None, "/srv/my repo")),
            "git@example.com:/srv/my repo"
        );
    }
}
