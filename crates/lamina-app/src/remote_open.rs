//! Opening a repository that lives on another machine.
//!
//! The dialog collects a host and a path, and lets the user browse folders on
//! the host before connecting. Connecting and browsing run on worker threads:
//! the worker may block in `ssh`, and the askpass callback may block until the
//! modal answers. Neither of those threads touches GPUI. They push
//! [`ConnectMsg`] or [`BrowseMsg`] values into an unbounded channel that one
//! foreground task drains, the same way a local session delivers
//! [`SessionEvent`]s.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc::{self as futures_mpsc, UnboundedSender};
use futures::channel::oneshot;
use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Context, Div, Entity, Focusable as _,
    FontWeight, IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, WeakEntity, Window, div, px,
};
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _};
use gpui_kit_assets::IconName;
use lamina_core::askpass::{Answer, Prompt};
use lamina_core::session::{RepositorySession, SessionEvent};
use lamina_remote_client::{
    InstallOptions, RemoteBrowser, RemoteDirListing, RemoteSession, RemoteTarget, SshOptions,
    install_helper,
};
use log::{info, warn};

use crate::settings::Settings;
use crate::style::Design;
use crate::ui;
use crate::workspace::Workspace;

/// How long a password prompt waits for the modal before `ssh` is cancelled.
const ASKPASS_WAIT: Duration = Duration::from_secs(150);

/// Maximum height of the scrollable folder list inside the remote-open dialog.
const BROWSE_LIST_MAX_HEIGHT: f32 = 220.0;

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

/// Host, optional port, and an optional starting folder parsed from the
/// dialog's two fields. Unlike [`RemoteSpec`], the path may be absent so the
/// folder picker can start at the remote home directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BrowseTarget {
    pub host: String,
    pub port: Option<u16>,
    pub initial_path: Option<String>,
}

/// A test points `ssh` at a local script and the helper directory at a copy
/// of `lamina-remote`. Production leaves this unset.
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

/// What either worker reports before its result: a phase label, a password
/// prompt, or the failure that ends the run. Both workers send these through
/// their own channel, so the phase and askpass plumbing is written once and each
/// message enum only adds its result. The askpass reply channel is answered by
/// the modal; the worker blocks on it, the UI thread does not.
pub(crate) enum WorkerEvent {
    Phase(ConnectPhase),
    Askpass {
        prompt: String,
        secret: bool,
        reply: mpsc::Sender<Option<String>>,
    },
    Failed(String),
}

/// Messages a connect worker sends. The repository tab drains them.
pub(crate) enum ConnectMsg {
    Worker(WorkerEvent),
    Ready(Box<dyn RepositorySession>),
    Session(SessionEvent),
}

/// Messages a folder-picker worker sends. Only the dialog drains them, so a
/// repository tab never sees a browser.
pub(crate) enum BrowseMsg {
    Worker(WorkerEvent),
    Ready {
        browser: Arc<RemoteBrowser>,
        listing: RemoteDirListing,
        /// Shown when the requested starting path failed and the worker fell
        /// back to the remote home directory.
        warning: Option<String>,
    },
}

impl From<WorkerEvent> for ConnectMsg {
    fn from(event: WorkerEvent) -> Self {
        Self::Worker(event)
    }
}

impl From<WorkerEvent> for BrowseMsg {
    fn from(event: WorkerEvent) -> Self {
        Self::Worker(event)
    }
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
///
/// The only argv `ssh` refuses to build is one whose destination starts with
/// '-', and the refusal is reported in the interface's language because it
/// reaches the status bar.
#[cfg(feature = "terminal")]
pub(crate) fn terminal_command(
    spec: &RemoteSpec,
    ssh: Option<&SshOverride>,
) -> Result<(String, Vec<String>), String> {
    let program = ssh_program(ssh);
    let argv = lamina_remote_client::terminal_argv(&program, spec.port, &spec.host, &spec.path)
        .map_err(|_| t!("remote_repo.err.host_flag", host = spec.host.as_str()).to_string())?;
    let mut parts = argv.iter().map(|part| part.to_string_lossy().into_owned());
    let program = parts.next().unwrap_or_default();
    Ok((program, parts.collect()))
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
    let target = parse_ssh_url_parts(rest, "")?;
    let path = target.initial_path.unwrap_or_default();
    validate_path(&path)?;
    Ok(Some(RemoteSpec {
        host: target.host,
        port: target.port,
        path,
    }))
}

/// Parses the dialog's two fields for folder browsing. A pasted `ssh://` URL
/// or `host:/path` in the host field supplies the starting path; otherwise the
/// path field is used when non-empty and `None` starts at remote home.
pub(crate) fn parse_browse_target(
    host_field: &str,
    path_field: &str,
) -> Result<BrowseTarget, String> {
    let host_field = host_field.trim();
    let path_field = path_field.trim();
    if host_field.is_empty() {
        return Err(t!("remote_repo.err.host").to_string());
    }
    if let Some(rest) = host_field.strip_prefix("ssh://") {
        return parse_ssh_url_parts(rest, path_field);
    }
    if let Some((authority, path)) = split_scp(host_field) {
        let (host, port) = split_authority(authority)?;
        validate_host(&host)?;
        return Ok(BrowseTarget {
            host,
            port,
            initial_path: Some(path.to_owned()),
        });
    }
    let (host, port) = split_authority(host_field)?;
    validate_host(&host)?;
    Ok(BrowseTarget {
        host,
        port,
        initial_path: (!path_field.is_empty()).then(|| path_field.to_owned()),
    })
}

/// Splits the dialog's two fields. A single `ssh://` URL or `host:/path`
/// pasted into the host field wins over whatever is in the path field.
pub(crate) fn parse_remote_fields(
    host_field: &str,
    path_field: &str,
) -> Result<RemoteSpec, String> {
    let target = parse_browse_target(host_field, path_field)?;
    let path = target.initial_path.unwrap_or_default();
    validate_path(&path)?;
    Ok(RemoteSpec {
        host: target.host,
        port: target.port,
        path,
    })
}

/// `LAMINA_REMOTE_HELPER_DIR` when it is set, otherwise `remote-helpers/`
/// beside the executable. A release build is expected to put the musl
/// binaries there; packaging them is not this crate's job.
pub(crate) fn resolve_helper_dir() -> Result<PathBuf, String> {
    if let Ok(value) = std::env::var(lamina_remote_client::HELPER_DIR_ENV)
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
    spawn_worker("lamina-remote-connect", sender, move |worker| {
        connect_worker(spec, ssh, worker)
    });
}

/// Installs the helper, opens a [`RemoteBrowser`], and lists the starting
/// folder (or remote home). Its own channel keeps the browser result away from
/// repository tabs; askpass prompts and progress still go through
/// [`WorkerEvent`] exactly as they do for [`spawn_connect`].
pub(crate) fn spawn_browse_open(
    target: BrowseTarget,
    ssh: Option<SshOverride>,
    sender: UnboundedSender<BrowseMsg>,
) {
    spawn_worker("lamina-remote-browse-open", sender, move |worker| {
        browse_open_worker(target, ssh, worker)
    });
}

/// Runs `work` on a thread named `name`. When the thread cannot start, the
/// failure goes down the same channel a worker failure would, so the UI shows
/// it through the normal path.
fn spawn_worker<M>(
    name: &str,
    sender: UnboundedSender<M>,
    work: impl FnOnce(UnboundedSender<M>) + Send + 'static,
) where
    M: From<WorkerEvent> + Send + 'static,
{
    let worker = sender.clone();
    let built = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || work(worker));
    if let Err(error) = built {
        let _ = sender.unbounded_send(WorkerEvent::Failed(error.to_string()).into());
    }
}

/// Drops a [`RemoteBrowser`] off the UI thread so waiting on `ssh` to exit
/// never stalls a frame.
pub(crate) fn drop_browser_in_background(browser: Arc<RemoteBrowser>) {
    let _ = std::thread::Builder::new()
        .name("lamina-remote-browser-drop".to_owned())
        .spawn(move || drop(browser));
}

/// Opens the askpass modal on `window` and delivers the answer to `reply`.
/// Falls back to cancelling `reply` if the window is gone so the blocked `ssh`
/// helper never hangs.
pub(crate) fn show_askpass_prompt(
    window: Option<AnyWindowHandle>,
    prompt: String,
    secret: bool,
    reply: mpsc::Sender<Option<String>>,
    cx: &mut App,
) {
    let Some(window) = window else {
        let _ = reply.send(None);
        return;
    };
    // The modal owns one sender. This clone answers only when the modal could
    // not be opened, so the blocked helper is not left waiting.
    let fallback = reply.clone();
    let opened = window.update(cx, |_, window, cx| {
        ui::askpass_prompt(
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

struct PreparedHost {
    target: RemoteTarget,
    helper: String,
    options: SshOptions,
}

/// Shared host setup for repository sessions and folder browsing: resolves the
/// helper directory, wires the askpass handler, installs the remote helper,
/// and builds the [`RemoteTarget`] and [`SshOptions`].
fn prepare_remote_host<M>(
    host: &str,
    port: Option<u16>,
    ssh: Option<&SshOverride>,
    sender: &UnboundedSender<M>,
) -> Option<PreparedHost>
where
    M: From<WorkerEvent> + Send + 'static,
{
    let _ = sender.unbounded_send(WorkerEvent::Phase(ConnectPhase::Connecting).into());
    let program = ssh_program(ssh);
    let helper_dir = match ssh {
        Some(ssh) => ssh.helper_dir.clone(),
        None => match resolve_helper_dir() {
            Ok(dir) => dir,
            Err(error) => {
                warn!("remote: cannot connect to {host}: {error}");
                let _ = sender.unbounded_send(WorkerEvent::Failed(error).into());
                return None;
            }
        },
    };
    let askpass_program = std::env::current_exe().ok();
    let askpass = askpass_handler(sender.clone());
    let _ = sender.unbounded_send(WorkerEvent::Phase(ConnectPhase::PreparingHelper).into());
    let install = InstallOptions {
        program: program.clone(),
        host: host.to_owned(),
        port,
        helper_dir: Some(helper_dir.clone()),
        askpass_program: askpass_program.clone(),
        askpass: Some(Arc::clone(&askpass)),
    };
    let helper = match install_helper(&install) {
        Ok(helper) => {
            info!("remote: helper ready at {helper} on {host}");
            helper
        }
        Err(error) => {
            warn!("remote: helper install failed for {host}: {error}");
            let _ = sender.unbounded_send(WorkerEvent::Failed(error.to_string()).into());
            return None;
        }
    };
    let _ = sender.unbounded_send(WorkerEvent::Phase(ConnectPhase::Opening).into());
    let options = SshOptions {
        program,
        helper_dir: Some(helper_dir),
        askpass_program,
        askpass: Some(askpass),
    };
    let target = match port {
        Some(port) => RemoteTarget::new(host.to_owned()).with_port(port),
        None => RemoteTarget::new(host.to_owned()),
    };
    Some(PreparedHost {
        target,
        helper,
        options,
    })
}

fn connect_worker(spec: RemoteSpec, ssh: Option<SshOverride>, sender: UnboundedSender<ConnectMsg>) {
    info!("remote: connecting to {} at {}", spec.host, spec.path);
    let Some(prepared) = prepare_remote_host(&spec.host, spec.port, ssh.as_ref(), &sender) else {
        return;
    };
    let events = sender.clone();
    // The callback runs on the client's reader thread. It only enqueues.
    // Blocking here, or dropping the session here, deadlocks the read loop.
    let connected = RemoteSession::open_ssh(
        prepared.target,
        spec.path,
        prepared.helper,
        move |event| {
            let _ = events.unbounded_send(ConnectMsg::Session(event));
        },
        prepared.options,
    );
    match connected {
        Ok(session) => {
            info!("remote: connected to {}", spec.host);
            let _ = sender.unbounded_send(ConnectMsg::Ready(Box::new(session)));
        }
        Err(error) => {
            warn!("remote: connect failed for {}: {error}", spec.host);
            let _ = sender.unbounded_send(WorkerEvent::Failed(error.to_string()).into());
        }
    }
}

fn browse_open_worker(
    target: BrowseTarget,
    ssh: Option<SshOverride>,
    sender: UnboundedSender<BrowseMsg>,
) {
    info!("remote: opening folder browser on {}", target.host);
    let Some(prepared) = prepare_remote_host(&target.host, target.port, ssh.as_ref(), &sender)
    else {
        return;
    };
    let browser = match RemoteBrowser::open(&prepared.target, prepared.helper, &prepared.options) {
        Ok(browser) => Arc::new(browser),
        Err(error) => {
            warn!("remote: browse open failed for {}: {error}", target.host);
            let _ = sender.unbounded_send(WorkerEvent::Failed(error.to_string()).into());
            return;
        }
    };
    let initial = match target.initial_path.as_deref() {
        Some(path) => match browser.list(Some(path)) {
            Ok(listing) => Ok((listing, None)),
            Err(error) => {
                // Keep a live connection usable when the typed path does not
                // exist or is unreadable: show home alongside the error.
                match browser.list(None) {
                    Ok(home_listing) => Ok((home_listing, Some(error.to_string()))),
                    Err(_) => Err(error.to_string()),
                }
            }
        },
        None => browser
            .list(None)
            .map(|listing| (listing, None))
            .map_err(|error| error.to_string()),
    };
    match initial {
        Ok((listing, warning)) => {
            info!(
                "remote: browser ready on {} at {}",
                target.host, listing.path
            );
            let _ = sender.unbounded_send(BrowseMsg::Ready {
                browser,
                listing,
                warning,
            });
        }
        Err(error) => {
            warn!(
                "remote: initial folder listing failed for {}: {error}",
                target.host
            );
            let _ = sender.unbounded_send(WorkerEvent::Failed(error).into());
        }
    }
}

fn spawn_browse_list(
    browser: Arc<RemoteBrowser>,
    path: Option<String>,
) -> oneshot::Receiver<Result<RemoteDirListing, String>> {
    let (sender, receiver) = oneshot::channel();
    let built = std::thread::Builder::new()
        .name("lamina-remote-browse-list".to_owned())
        .spawn(move || {
            let result = browser
                .list(path.as_deref())
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
    if let Err(error) = built {
        let (err_tx, err_rx) = oneshot::channel();
        let _ = err_tx.send(Err(error.to_string()));
        return err_rx;
    }
    receiver
}

fn askpass_handler<M>(sender: UnboundedSender<M>) -> lamina_remote_client::AskpassHandler
where
    M: From<WorkerEvent> + Send + 'static,
{
    Arc::new(move |prompt: Prompt| {
        let (reply_tx, reply_rx) = mpsc::channel();
        if sender
            .unbounded_send(
                WorkerEvent::Askpass {
                    prompt: prompt.text,
                    secret: prompt.secret,
                    reply: reply_tx,
                }
                .into(),
            )
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

fn parse_ssh_url_parts(rest: &str, path_field: &str) -> Result<BrowseTarget, String> {
    let (authority, url_path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    if authority.is_empty() {
        return Err(t!("remote_repo.err.host").to_string());
    }
    let (host, port) = split_authority(authority)?;
    validate_host(&host)?;
    let initial_path = match url_path {
        Some(path) if !path.is_empty() => Some(normalize_url_path(path)),
        _ => (!path_field.is_empty()).then(|| path_field.to_owned()),
    };
    Ok(BrowseTarget {
        host,
        port,
        initial_path,
    })
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

/// Joins one folder entry onto a canonical remote POSIX path without doubling
/// the leading slash at `/`.
fn join_remote_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn format_authority(host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// True when the host field carries a pasted URL or `host:/path` whose path
/// would override the separate path field on submit.
fn host_has_embedded_path(host_field: &str) -> bool {
    let trimmed = host_field.trim();
    trimmed.starts_with("ssh://") || split_scp(trimmed).is_some()
}

/// True when the host field still addresses the same `(host, port)` as the
/// active browser session.
fn same_browse_host(host_field: &str, host: &str, port: Option<u16>) -> bool {
    matches!(
        parse_browse_target(host_field, ""),
        Ok(target) if target.host == host && target.port == port
    )
}

/// Monotonic request counter so answers from superseded folder clicks or a
/// closed picker are ignored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RequestGate {
    latest: u64,
}

impl RequestGate {
    fn next(&mut self) -> u64 {
        self.latest = self.latest.wrapping_add(1);
        self.latest
    }

    fn invalidate(&mut self) {
        self.latest = self.latest.wrapping_add(1);
    }

    fn is_current(&self, token: u64) -> bool {
        token == self.latest
    }
}

fn browse_phase_label(phase: ConnectPhase) -> String {
    match phase {
        ConnectPhase::Connecting => t!("remote_repo.browse.connecting").to_string(),
        ConnectPhase::PreparingHelper => t!("remote_repo.browse.preparing").to_string(),
        ConnectPhase::Opening => t!("remote_repo.browse.opening").to_string(),
        other => other.label(),
    }
}

struct BrowsePanel {
    host: String,
    port: Option<u16>,
    /// Kept open while the picker is visible so each folder click is one round
    /// trip. Dropped in the background when the picker or dialog closes.
    browser: Option<Arc<RemoteBrowser>>,
    connect_phase: Option<ConnectPhase>,
    loading: bool,
    listing: Option<RemoteDirListing>,
    error: Option<String>,
    _connect_task: Option<Task<()>>,
    _list_task: Option<Task<()>>,
}

impl BrowsePanel {
    fn is_connecting(&self) -> bool {
        self.connect_phase.is_some()
    }
}

struct RemoteDialog {
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    ssh: Option<SshOverride>,
    host: Entity<InputState>,
    path: Entity<InputState>,
    error: Option<String>,
    browse: Option<BrowsePanel>,
    browse_gate: RequestGate,
    _host_events: Subscription,
}

pub(crate) fn open_dialog(workspace: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|cx| RemoteDialog::new(workspace.downgrade(), None, window, cx));
    let focus = dialog.read(cx).host.focus_handle(cx);
    let content = dialog.clone();
    let confirm = dialog.clone();
    let on_close = dialog.clone();
    window.open_dialog(cx, move |card, _, cx| {
        let zoom = Settings::get(cx).appearance.zoom();
        let content = content.clone();
        let confirm = confirm.clone();
        let on_close = on_close.clone();
        card.title(t!("remote_repo.title").to_string())
            .w(px(560.0 * zoom))
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
            .on_close(move |_, _, cx| {
                on_close.update(cx, |dialog, cx| dialog.close_browser(cx));
            })
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

impl RemoteDialog {
    fn new(
        workspace: WeakEntity<Workspace>,
        ssh: Option<SshOverride>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let host = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("remote_repo.host_placeholder").to_string())
        });
        let path = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("remote_repo.path_placeholder").to_string())
        });
        let host_events = cx.subscribe(&host, |this, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.on_host_change(cx);
            }
        });
        Self {
            workspace,
            window: window.window_handle(),
            ssh,
            host,
            path,
            error: None,
            browse: None,
            browse_gate: RequestGate::default(),
            _host_events: host_events,
        }
    }

    fn on_host_change(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        if let Some(panel) = &self.browse {
            let host_value = self.host.read(cx).value();
            if !same_browse_host(host_value.as_ref(), &panel.host, panel.port) {
                self.close_browser(cx);
                return;
            }
        }
        cx.notify();
    }

    fn start_browse(&mut self, cx: &mut Context<Self>) {
        let host = self.host.read(cx).value().to_string();
        let path = self.path.read(cx).value().to_string();
        let target = match parse_browse_target(&host, &path) {
            Ok(target) => target,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.error = None;

        // Reuse an already-healthy browser for the same host so re-clicking
        // Browse jumps to the typed path in one round trip. If the panel had
        // an error, reconnect fresh in case the SSH transport died.
        if let Some(panel) = &self.browse
            && panel.host == target.host
            && panel.port == target.port
        {
            if panel.is_connecting() {
                return;
            }
            if panel.browser.is_some() && panel.error.is_none() {
                self.list_folder(target.initial_path, cx);
                return;
            }
        }

        self.close_browser_silent();
        let token = self.browse_gate.next();
        let (sender, mut receiver) = futures_mpsc::unbounded();
        spawn_browse_open(target.clone(), self.ssh.clone(), sender);
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = receiver.next().await {
                if this
                    .update(cx, |dialog, cx| dialog.on_browse_msg(token, message, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.browse = Some(BrowsePanel {
            host: target.host,
            port: target.port,
            browser: None,
            connect_phase: Some(ConnectPhase::Connecting),
            loading: false,
            listing: None,
            error: None,
            _connect_task: Some(task),
            _list_task: None,
        });
        cx.notify();
    }

    fn on_browse_msg(&mut self, token: u64, message: BrowseMsg, cx: &mut Context<Self>) {
        if !self.browse_gate.is_current(token) {
            match message {
                BrowseMsg::Ready { browser, .. } => drop_browser_in_background(browser),
                BrowseMsg::Worker(WorkerEvent::Askpass { reply, .. }) => {
                    let _ = reply.send(None);
                }
                BrowseMsg::Worker(_) => {}
            }
            return;
        }
        match message {
            BrowseMsg::Worker(WorkerEvent::Phase(phase)) => {
                if let Some(panel) = &mut self.browse {
                    panel.connect_phase = Some(phase);
                }
                cx.notify();
            }
            BrowseMsg::Worker(WorkerEvent::Askpass {
                prompt,
                secret,
                reply,
            }) => {
                show_askpass_prompt(Some(self.window), prompt, secret, reply, cx);
            }
            BrowseMsg::Worker(WorkerEvent::Failed(error)) => {
                if let Some(panel) = &mut self.browse {
                    panel.connect_phase = None;
                    panel.loading = false;
                    panel.error = Some(error);
                    panel._connect_task = None;
                }
                cx.notify();
            }
            BrowseMsg::Ready {
                browser,
                listing,
                warning,
            } => {
                if let Some(panel) = &mut self.browse {
                    panel.browser = Some(browser);
                    panel.connect_phase = None;
                    panel.loading = false;
                    panel.listing = Some(listing);
                    panel.error = warning;
                    panel._connect_task = None;
                } else {
                    drop_browser_in_background(browser);
                }
                cx.notify();
            }
        }
    }

    fn list_folder(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.browse else {
            return;
        };
        let Some(browser) = panel.browser.as_ref().map(Arc::clone) else {
            return;
        };
        let token = self.browse_gate.next();
        panel.loading = true;
        panel.error = None;
        let receiver = spawn_browse_list(browser, path);
        panel._list_task = Some(cx.spawn(async move |this, cx| {
            let Ok(result) = receiver.await else {
                return;
            };
            let _ = this.update(cx, |dialog, cx| dialog.on_browse_listed(token, result, cx));
        }));
        cx.notify();
    }

    fn on_browse_listed(
        &mut self,
        token: u64,
        result: Result<RemoteDirListing, String>,
        cx: &mut Context<Self>,
    ) {
        if !self.browse_gate.is_current(token) {
            return;
        }
        let Some(panel) = &mut self.browse else {
            return;
        };
        panel.loading = false;
        panel._list_task = None;
        match result {
            Ok(listing) => {
                panel.listing = Some(listing);
                panel.error = None;
            }
            Err(error) => {
                panel.error = Some(error);
            }
        }
        cx.notify();
    }

    fn use_current_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &self.browse else {
            return;
        };
        let Some(listing) = &panel.listing else {
            return;
        };
        let chosen = listing.path.clone();
        let authority = format_authority(&panel.host, panel.port);
        self.close_browser_silent();
        self.path
            .update(cx, |state, cx| state.set_value(chosen, window, cx));
        // If the host field held `ssh://host/old` or `host:/old`, strip the
        // embedded path so Connect opens the folder the user just picked.
        if host_has_embedded_path(self.host.read(cx).value().as_ref()) {
            self.host
                .update(cx, |state, cx| state.set_value(authority, window, cx));
        }
        self.error = None;
        cx.notify();
    }

    fn close_browser(&mut self, cx: &mut Context<Self>) {
        if self.browse.is_none() {
            return;
        }
        self.close_browser_silent();
        cx.notify();
    }

    fn close_browser_silent(&mut self) {
        self.browse_gate.invalidate();
        if let Some(panel) = self.browse.take()
            && let Some(browser) = panel.browser
        {
            drop_browser_in_background(browser);
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
            self.close_browser_silent();
            window.close_dialog(cx);
            return;
        };
        if let Err(error) = workspace.update(cx, |workspace, cx| {
            workspace.open_remote(spec, true, self.ssh.clone(), window, cx)
        }) {
            self.error = Some(error);
            cx.notify();
            return;
        }
        self.close_browser_silent();
        window.close_dialog(cx);
    }

    fn render_browser_panel(
        &self,
        panel: &BrowsePanel,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(listing) = &panel.listing {
            let mut frame = ui::panel(design).header(ui::path_label(listing.path.clone(), design));
            if listing.repository {
                frame = frame.header(repository_badge(design));
            }
            if panel.loading {
                frame = frame.header(ui::spinner("remote-browse-loading", design, 12.0));
            }
            frame = frame.header(close_picker_button("remote-browse-close", cx));

            let mut list = ui::scrolling_list("remote-browse-list", BROWSE_LIST_MAX_HEIGHT, design);

            if let Some(parent) = &listing.parent {
                let parent = parent.clone();
                list = list.child(
                    ui::list_row("remote-browse-parent", ui::RowState::NORMAL, design)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.list_folder(Some(parent.clone()), cx);
                        }))
                        .child(
                            Icon::new(IconName::ArrowUp)
                                .with_size(design.px(13.0))
                                .text_color(design.muted),
                        )
                        .child(div().font_weight(FontWeight::MEDIUM).child(".."))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(design.tiny()))
                                .text_color(design.muted)
                                .child(t!("remote_repo.browse.parent").to_string()),
                        ),
                );
            }

            if listing.entries.is_empty() {
                list = list.child(
                    div()
                        .px_3()
                        .py_3()
                        .text_size(px(design.small()))
                        .text_color(design.muted)
                        .child(t!("remote_repo.browse.empty").to_string()),
                );
            } else {
                for (index, entry) in listing.entries.iter().enumerate() {
                    let next_path = join_remote_path(&listing.path, &entry.name);
                    let mut row =
                        ui::list_row(("remote-browse-entry", index), ui::RowState::NORMAL, design)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.list_folder(Some(next_path.clone()), cx);
                            }))
                            .child(
                                Icon::new(IconName::Folder)
                                    .with_size(design.px(13.0))
                                    .text_color(if entry.repository {
                                        design.accent_text
                                    } else {
                                        design.muted
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(entry.name.clone()),
                            );
                    if entry.repository {
                        row = row.child(repository_badge(design));
                    }
                    list = list.child(row);
                }
            }

            frame = frame.child(list);

            if listing.truncated {
                frame = frame.child(
                    ui::banner(
                        ui::BannerKind::Info,
                        t!("remote_repo.browse.truncated").to_string(),
                        design,
                    )
                    .wrap(),
                );
            }

            if let Some(error) = &panel.error {
                frame = frame.child(danger_banner(error, design));
            }

            frame = frame.footer(
                ui::button(
                    "remote-browse-use",
                    t!("remote_repo.browse.use_folder").to_string(),
                )
                .kind(if listing.repository {
                    ui::ButtonKind::Primary
                } else {
                    ui::ButtonKind::Normal
                })
                .size(ui::ButtonSize::Small)
                .disabled(panel.loading)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.use_current_folder(window, cx);
                })),
            );
            return frame.into_any_element();
        }

        if let Some(phase) = panel.connect_phase {
            return ui::panel(design)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .w_full()
                        .px_3()
                        .py_3()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_size(px(design.small()))
                                .text_color(design.muted)
                                .child(ui::spinner("remote-browse-connect", design, 14.0))
                                .child(browse_phase_label(phase)),
                        )
                        .child(close_picker_button("remote-browse-cancel", cx)),
                )
                .into_any_element();
        }

        if let Some(error) = &panel.error {
            return danger_banner(error, design);
        }

        div().into_any_element()
    }
}

/// The dismiss control in a picker header or progress row. Both close the
/// picker, so they share one handler and differ only in their element id.
fn close_picker_button(id: &'static str, cx: &mut Context<RemoteDialog>) -> impl IntoElement {
    ui::icon_button(id, IconName::X)
        .ghost()
        .size(ui::ButtonSize::Tiny)
        .tooltip(t!("common.close").to_string())
        .on_click(cx.listener(|this, _, _, cx| {
            this.close_browser(cx);
        }))
}

/// The chip that marks a folder as a Git repository, in a header or a row.
fn repository_badge(design: &Design) -> AnyElement {
    ui::status_badge(
        t!("remote_repo.browse.repository").to_string(),
        ui::BadgeTone::Accent,
        design,
    )
}

/// A failure the user has to read, wrapped rather than truncated: a long
/// `ssh` message is the whole point of the banner.
fn danger_banner(message: &str, design: &Design) -> AnyElement {
    ui::banner(ui::BannerKind::Danger, message.to_owned(), design)
        .wrap()
        .into_any_element()
}

impl Drop for RemoteDialog {
    fn drop(&mut self) {
        self.close_browser_silent();
    }
}

impl Render for RemoteDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let host_value = self.host.read(cx).value();
        let can_browse = parse_browse_target(host_value.as_ref(), "").is_ok();
        let is_connecting = self.browse.as_ref().is_some_and(BrowsePanel::is_connecting);

        let mut form = div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(labeled(
                t!("remote_repo.host").to_string(),
                Input::new(&self.host).w_full().bg(design.field),
                &design,
            ))
            .child(caption(t!("remote_repo.host_hint").to_string(), &design))
            .child(labeled(
                t!("remote_repo.path").to_string(),
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Input::new(&self.path).flex_1().bg(design.field))
                    .child(
                        ui::button("remote-repo-browse", t!("remote_repo.browse").to_string())
                            .kind(ui::ButtonKind::Normal)
                            .size(ui::ButtonSize::Medium)
                            .disabled(!can_browse)
                            .loading(is_connecting)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_browse(cx);
                            })),
                    ),
                &design,
            ));

        if let Some(panel) = &self.browse {
            form = form.child(self.render_browser_panel(panel, &design, cx));
        }

        if let Some(error) = &self.error {
            form = form.child(danger_banner(error, &design));
        }
        form
    }
}

/// A small muted line: a field label or the hint under a field.
fn caption(text: String, design: &Design) -> Div {
    div()
        .text_size(px(design.tiny()))
        .text_color(design.muted)
        .child(text)
}

/// A caption above a control, filling the form's width.
fn labeled(label: String, control: impl IntoElement, design: &Design) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .child(caption(label, design))
        .child(control)
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

    fn browse_target(host: &str, port: Option<u16>, initial_path: Option<&str>) -> BrowseTarget {
        BrowseTarget {
            host: host.to_owned(),
            port,
            initial_path: initial_path.map(str::to_owned),
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
    fn browse_target_allows_an_empty_path_and_reuses_authority_parsing() {
        assert_eq!(
            parse_browse_target("example.com", "").unwrap(),
            browse_target("example.com", None, None)
        );
        assert_eq!(
            parse_browse_target("git@example.com:2222", "  ").unwrap(),
            browse_target("git@example.com", Some(2222), None)
        );
        assert_eq!(
            parse_browse_target("example.com", "~/src").unwrap(),
            browse_target("example.com", None, Some("~/src"))
        );
        assert_eq!(
            parse_browse_target("example.com", "src").unwrap(),
            browse_target("example.com", None, Some("src"))
        );
        assert_eq!(
            parse_browse_target("ssh://git@example.com:2222", "").unwrap(),
            browse_target("git@example.com", Some(2222), None)
        );
        assert_eq!(
            parse_browse_target("ssh://example.com/srv/repos", "/ignored").unwrap(),
            browse_target("example.com", None, Some("/srv/repos"))
        );
        assert_eq!(
            parse_browse_target("example.com:~/src", "").unwrap(),
            browse_target("example.com", None, Some("~/src"))
        );
        assert!(parse_browse_target("  ", "").is_err());
        assert!(parse_browse_target("ssh://", "").is_err());
        assert!(parse_browse_target("example.com:99999", "").is_err());
    }

    #[test]
    fn request_gate_ignores_stale_and_invalidated_tokens() {
        let mut gate = RequestGate::default();
        let first = gate.next();
        assert!(gate.is_current(first));

        let second = gate.next();
        assert!(!gate.is_current(first));
        assert!(gate.is_current(second));

        gate.invalidate();
        assert!(!gate.is_current(second));

        let third = gate.next();
        assert!(gate.is_current(third));
    }

    #[test]
    fn join_remote_path_handles_root_and_regular_folders() {
        assert_eq!(join_remote_path("/", "home"), "/home");
        assert_eq!(join_remote_path("/home/alice", "src"), "/home/alice/src");
        assert_eq!(join_remote_path("/home/alice/", "src"), "/home/alice/src");
    }

    #[test]
    fn browse_host_helpers_track_authority_changes_and_embedded_paths() {
        assert!(same_browse_host("example.com", "example.com", None));
        assert!(same_browse_host("  example.com  ", "example.com", None));
        assert!(same_browse_host(
            "git@example.com:2222",
            "git@example.com",
            Some(2222)
        ));
        assert!(!same_browse_host("example.com:2222", "example.com", None));
        assert!(!same_browse_host("other.com", "example.com", None));
        assert!(!same_browse_host("", "example.com", None));

        assert!(host_has_embedded_path("ssh://example.com/srv/repo"));
        assert!(host_has_embedded_path("example.com:/srv/repo"));
        assert!(host_has_embedded_path("git@example.com:~/repo"));
        assert!(!host_has_embedded_path("example.com"));
        assert!(!host_has_embedded_path("git@example.com:2222"));

        assert_eq!(format_authority("example.com", None), "example.com");
        assert_eq!(
            format_authority("git@example.com", Some(2222)),
            "git@example.com:2222"
        );
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
        let (program, args) = terminal_command(&target, Some(&ssh)).expect("terminal command");
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

        assert_eq!(
            terminal_command(&target, None).expect("terminal command").0,
            "ssh"
        );
        assert_eq!(ssh_program(None), PathBuf::from("ssh"));
    }

    #[cfg(feature = "terminal")]
    #[test]
    fn a_terminal_command_refuses_a_host_that_ssh_would_read_as_options() {
        // A destination beginning with '-' is parsed as flags before `--`, so
        // no command may be built for it; the refusal is shown in the status
        // bar, which is why it has to be in the interface's language.
        let target = spec(
            "-oProxyCommand=touch /tmp/lamina-terminal-pwned",
            None,
            "/srv/repo",
        );
        let error = terminal_command(&target, None).expect_err("a flag-shaped host has no command");
        assert!(
            error.contains("may not begin with '-'"),
            "the refusal names what is wrong with the host: {error}"
        );
    }

    #[cfg(feature = "terminal")]
    #[test]
    fn the_terminal_tab_title_names_the_host_and_the_whole_path() {
        assert_eq!(
            terminal_tab_title(&spec("git@example.com", None, "/srv/my repo")),
            "git@example.com:/srv/my repo"
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    fn remote_dialog_browses_folders_fills_path_and_drops_helper_on_host_change(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::time::Instant;

        use crate::repo_view::remote_fixture::{
            install_host_helper, layout_fake_ssh, scratch_dir, wait_for_helper,
        };

        cx.dispatcher.allow_parking();
        let scratch = scratch_dir("lamina-app-browse-ui");
        let laid = layout_fake_ssh(&scratch);
        let helper = wait_for_helper();
        install_host_helper(&helper, &laid.helpers);

        let home = scratch.join("home");
        std::fs::create_dir_all(home.join("src/app/.git")).unwrap();
        std::fs::create_dir_all(home.join("src/notes")).unwrap();
        let canonical_home = std::fs::canonicalize(&home).unwrap();
        let canonical_app = std::fs::canonicalize(home.join("src/app")).unwrap();

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
        });

        let ssh = Some(SshOverride {
            program: laid.ssh.clone(),
            helper_dir: laid.helpers.clone(),
        });
        let window = cx
            .add_window(|window, cx| RemoteDialog::new(WeakEntity::new_invalid(), ssh, window, cx));
        let dialog = window.root(cx).expect("dialog root");

        window
            .update(cx, |dialog, window, cx| {
                dialog.host.update(cx, |state, cx| {
                    state.set_value("git.example.com", window, cx)
                });
                dialog.start_browse(cx);
            })
            .expect("start browse");

        let wait_for_path = |cx: &mut gpui::TestAppContext, expected: &str| {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                std::thread::sleep(Duration::from_millis(30));
                cx.run_until_parked();
                let state = cx.read(|cx| {
                    let dialog = dialog.read(cx);
                    let panel = dialog.browse.as_ref();
                    (
                        panel.and_then(|p| p.listing.as_ref().map(|l| l.path.clone())),
                        panel.and_then(|p| p.error.clone()),
                    )
                });
                if let Some(error) = state.1 {
                    let log = std::fs::read_to_string(&laid.log).unwrap_or_default();
                    panic!("browse failed: {error}\nssh log:\n{log}");
                }
                if state.0.as_deref() == Some(expected) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {expected}, got {:?}",
                    state.0
                );
            }
        };

        let home_str = canonical_home.display().to_string();
        let src_str = format!("{home_str}/src");
        let app_str = canonical_app.display().to_string();

        wait_for_path(cx, &home_str);

        // Step into `src` on the kept-open browser.
        cx.update(|cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.list_folder(Some(src_str.clone()), cx)
            });
        });
        wait_for_path(cx, &src_str);

        cx.read(|cx| {
            let listing = dialog
                .read(cx)
                .browse
                .as_ref()
                .and_then(|panel| panel.listing.as_ref())
                .expect("src listing");
            let entries: Vec<_> = listing
                .entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.repository))
                .collect();
            assert_eq!(entries, [("app", true), ("notes", false)]);
            assert_eq!(listing.parent.as_deref(), Some(home_str.as_str()));
            assert!(!listing.repository);
        });

        // Step into `src/app` and check that it is marked as a repository.
        cx.update(|cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.list_folder(Some(app_str.clone()), cx)
            });
        });
        wait_for_path(cx, &app_str);

        cx.read(|cx| {
            let listing = dialog
                .read(cx)
                .browse
                .as_ref()
                .and_then(|panel| panel.listing.as_ref())
                .expect("app listing");
            assert!(listing.repository);
        });

        // Only one `--browse` helper was started for all three listings.
        let log = std::fs::read_to_string(&laid.log).unwrap();
        assert_eq!(
            log.matches("<--browse>").count(),
            1,
            "folder clicks must reuse the open browser: {log}"
        );

        // Choosing "Use this folder" fills the path field and closes the picker.
        window
            .update(cx, |dialog, window, cx| {
                dialog.use_current_folder(window, cx);
            })
            .expect("use folder");

        cx.read(|cx| {
            let dialog = dialog.read(cx);
            assert_eq!(dialog.path.read(cx).value().as_ref(), app_str.as_str());
            assert!(dialog.browse.is_none(), "picker closes after choosing");
        });

        // Re-opening Browse starts from the path field (`app_str`) and changing
        // the host field closes the picker.
        cx.update(|cx| {
            dialog.update(cx, |dialog, cx| dialog.start_browse(cx));
        });
        wait_for_path(cx, &app_str);

        let helper_pid = std::fs::read_to_string(&laid.pid)
            .expect("the browse helper recorded its pid")
            .trim()
            .to_owned();

        // `InputState::set_value` suppresses `Change` to avoid programmatic
        // feedback loops; emit `Change` on `host` outside `dialog`'s borrow
        // to match user typing.
        let host_input = cx.read(|cx| dialog.read(cx).host.clone());
        window
            .update(cx, |_, window, cx| {
                host_input.update(cx, |state, cx| {
                    state.set_value("other.example.com", window, cx);
                    cx.emit(InputEvent::Change);
                });
            })
            .expect("change host");
        cx.run_until_parked();

        cx.read(|cx| {
            assert!(
                dialog.read(cx).browse.is_none(),
                "changing the host must close the browser and drop its helper"
            );
        });

        let exit_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", &helper_pid])
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                break;
            }
            assert!(
                Instant::now() < exit_deadline,
                "browse helper pid {helper_pid} stayed alive after the host changed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = std::fs::remove_dir_all(&scratch);
    }
}
