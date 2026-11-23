//! `ssh` transport for [`RemoteSession`](crate::RemoteSession).
//!
//! The system `ssh` binary owns the network. Its stdin and stdout are the
//! framed protocol; stderr is kept (bounded) so a lost connection can say why.
//! Host-key and password prompts go through the same [`AskpassHost`] the app
//! already uses for git, when the caller supplies both a handler and the
//! askpass program.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use lamina_core::askpass::{Answer, AskpassHost, Prompt};
use lamina_core::session::SessionEvent;
use lamina_core::{Error, Result};

use crate::deploy::{self, InstallOptions};
use crate::session::{ClientConfig, RemoteSession, RemoteTarget, TransportAttach};

/// How long `ssh` may spend establishing the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Keepalive interval and how many missed replies `ssh` tolerates.
const SERVER_ALIVE_INTERVAL: Duration = Duration::from_secs(15);
const SERVER_ALIVE_COUNT_MAX: u32 = 3;

/// Tail of stderr kept for error text. The helper's stdout is the protocol,
/// so diagnostics must not grow without a bound.
const STDERR_CAP: usize = 16 * 1024;

/// Stdout of a short `ssh` command (probe, upload, self-check).
const STDOUT_CAP: usize = 1024 * 1024;

/// Answers an `SSH_ASKPASS` prompt. `None` cancels.
pub type AskpassHandler = Arc<dyn Fn(Prompt) -> Option<Answer> + Send + Sync>;

/// How to reach a host and where the local helper binaries live.
#[derive(Clone)]
pub struct SshOptions {
    /// `ssh` executable. Tests point this at a script. The default is `ssh`.
    pub program: PathBuf,
    /// Directory of `lamina-remote-<target>` files. `None` reads
    /// [`crate::HELPER_DIR_ENV`].
    pub helper_dir: Option<PathBuf>,
    /// Program `SSH_ASKPASS` should run (`lamina --askpass`).
    pub askpass_program: Option<PathBuf>,
    /// Callback that answers prompts. Ignored unless [`Self::askpass_program`]
    /// is also set: `SSH_ASKPASS_REQUIRE=force` without a program would fail
    /// every prompt.
    pub askpass: Option<AskpassHandler>,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            program: PathBuf::from("ssh"),
            helper_dir: None,
            askpass_program: None,
            askpass: None,
        }
    }
}

impl std::fmt::Debug for SshOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SshOptions")
            .field("program", &self.program)
            .field("helper_dir", &self.helper_dir)
            .field("askpass_program", &self.askpass_program)
            .field("askpass", &self.askpass.as_ref().map(|_| "set"))
            .finish()
    }
}

pub(crate) struct AskpassSession {
    host: AskpassHost,
    program: PathBuf,
}

impl AskpassSession {
    fn start(handler: &AskpassHandler, program: &Path) -> Result<Self> {
        let handler = Arc::clone(handler);
        let host = AskpassHost::start(move |prompt| handler(prompt)).map_err(|error| {
            Error::other(format!("could not start the askpass helper: {error}"))
        })?;
        Ok(Self {
            host,
            program: program.to_path_buf(),
        })
    }

    pub(crate) fn apply(&self, command: &mut Command) {
        self.host.apply(command, &self.program);
    }
}

pub(crate) fn start_askpass(
    handler: Option<&AskpassHandler>,
    program: Option<&Path>,
) -> Result<Option<AskpassSession>> {
    match (handler, program) {
        (Some(handler), Some(program)) => Ok(Some(AskpassSession::start(handler, program)?)),
        _ => Ok(None),
    }
}

struct SshChild {
    child: Mutex<Option<Child>>,
    stderr: Mutex<Option<JoinHandle<()>>>,
}

impl SshChild {
    /// Idempotent: a second call is what `Drop` does after the disconnect hook.
    fn shutdown(&self) {
        if let Some(mut child) = lock(&self.child).take() {
            // A helper that ignores stdin EOF would leave the reader join
            // blocked. Killing `ssh` closes both pipes.
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = lock(&self.stderr).take() {
            let _ = handle.join();
        }
    }
}

/// Kept alive for the session. Dropped after the disconnect hook.
struct Retain {
    process: Arc<SshChild>,
    /// Keeps the askpass listener alive for the ssh process. Not read.
    #[allow(dead_code)]
    askpass: Option<AskpassSession>,
}

impl Drop for Retain {
    fn drop(&mut self) {
        self.process.shutdown();
    }
}

pub(crate) struct SshOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl RemoteSession {
    /// Copies the helper to the host when it is not already the right build,
    /// then opens `repo_path` over `ssh`.
    pub fn connect_ssh(
        target: RemoteTarget,
        repo_path: impl Into<String>,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
        options: SshOptions,
    ) -> Result<Self> {
        Self::connect_ssh_with(
            target,
            repo_path,
            on_event,
            options,
            ClientConfig::default(),
        )
    }

    /// [`connect_ssh`](Self::connect_ssh) with a test-supplied ping interval.
    pub fn connect_ssh_with(
        target: RemoteTarget,
        repo_path: impl Into<String>,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
        options: SshOptions,
        config: ClientConfig,
    ) -> Result<Self> {
        let repo_path = repo_path.into();
        if target.host.is_empty() {
            return Err(Error::other("the remote host is empty"));
        }
        if repo_path.is_empty() {
            return Err(Error::other("the remote repository path is empty"));
        }
        let askpass = start_askpass(options.askpass.as_ref(), options.askpass_program.as_deref())?;
        let helper = deploy::install_with(
            &InstallOptions {
                program: options.program.clone(),
                host: target.host.clone(),
                port: target.port,
                helper_dir: options.helper_dir.clone(),
                askpass_program: None,
                askpass: None,
            },
            askpass.as_ref(),
        )?;
        Self::attach_ssh(
            target, repo_path, helper, on_event, options, config, askpass,
        )
    }

    /// Opens `repo_path` with a helper that [`install_helper`] already put on
    /// the host. A caller that reports the install as its own step uses this
    /// so the helper is not checked and copied a second time.
    ///
    /// [`install_helper`]: crate::install_helper
    pub fn open_ssh(
        target: RemoteTarget,
        repo_path: impl Into<String>,
        helper: impl Into<String>,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
        options: SshOptions,
    ) -> Result<Self> {
        let repo_path = repo_path.into();
        let helper = helper.into();
        if target.host.is_empty() {
            return Err(Error::other("the remote host is empty"));
        }
        if repo_path.is_empty() {
            return Err(Error::other("the remote repository path is empty"));
        }
        if helper.is_empty() {
            return Err(Error::other("the remote helper path is empty"));
        }
        let askpass = start_askpass(options.askpass.as_ref(), options.askpass_program.as_deref())?;
        Self::attach_ssh(
            target,
            repo_path,
            helper,
            on_event,
            options,
            ClientConfig::default(),
            askpass,
        )
    }

    fn attach_ssh(
        target: RemoteTarget,
        repo_path: String,
        helper: String,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
        options: SshOptions,
        config: ClientConfig,
        askpass: Option<AskpassSession>,
    ) -> Result<Self> {
        let (reader, writer, attach) = spawn_session(
            &options.program,
            target.port,
            &target.host,
            &helper,
            &repo_path,
            askpass,
        )?;
        Self::connect_attached(reader, writer, target, on_event, config, attach)
    }
}

pub(crate) fn spawn_session(
    program: &Path,
    port: Option<u16>,
    host: &str,
    helper: &str,
    repo: &str,
    askpass: Option<AskpassSession>,
) -> Result<(
    impl Read + Send + 'static,
    impl Write + Send + 'static,
    TransportAttach,
)> {
    let remote_args = vec![
        helper.to_owned(),
        "--stdio".to_owned(),
        "--root".to_owned(),
        repo.to_owned(),
    ];
    let mut child = spawn(program, port, host, &remote_args, askpass.as_ref())?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::other("ssh did not provide stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::other("ssh did not provide stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::other("ssh did not provide stderr"))?;

    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let capture = Arc::clone(&captured);
    let stderr_thread = match thread::Builder::new()
        .name("lamina-remote-ssh-err".to_owned())
        .spawn(move || capture_stderr(stderr, &capture))
    {
        Ok(handle) => handle,
        Err(error) => {
            // Nothing would drain the pipes without the reader, so the child
            // has to be reaped here or it outlives the failed connect.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::other(format!("could not read ssh stderr: {error}")));
        }
    };

    let process = Arc::new(SshChild {
        child: Mutex::new(Some(child)),
        stderr: Mutex::new(Some(stderr_thread)),
    });
    let diagnostic_buf = Arc::clone(&captured);
    let diagnostic: Arc<dyn Fn() -> String + Send + Sync> =
        Arc::new(move || String::from_utf8_lossy(&lock(&diagnostic_buf)).into_owned());
    let killer = Arc::clone(&process);
    let disconnect: Box<dyn FnOnce() + Send> = Box::new(move || killer.shutdown());
    let attach = TransportAttach {
        diagnostic,
        disconnect: Some(disconnect),
        retain: Some(Box::new(Retain { process, askpass })),
    };
    Ok((stdout, stdin, attach))
}

/// Runs one short remote command. `stdin_file` is streamed when uploading the
/// helper; stdout and stderr are read on their own threads so a full pipe
/// cannot stall the upload.
pub(crate) fn ssh_run(
    program: &Path,
    port: Option<u16>,
    host: &str,
    remote_args: &[String],
    stdin_file: Option<&Path>,
    askpass: Option<&AskpassSession>,
) -> Result<SshOutput> {
    let mut child = spawn(program, port, host, remote_args, askpass)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::other("ssh did not provide stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::other("ssh did not provide stderr"))?;
    let mut stdin = child.stdin.take();

    let stdout_thread = thread::spawn(move || read_capped(stdout, STDOUT_CAP));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_capture = Arc::clone(&stderr_buf);
    let stderr_thread = thread::spawn(move || capture_stderr(stderr, &stderr_capture));

    // A remote `mkdir` can fail and close the pipe before `cat` reads. That is
    // the install error; the broken-pipe write is only how we notice it.
    let copy_result = if let Some(mut pipe) = stdin.take() {
        let result = (|| -> io::Result<()> {
            if let Some(path) = stdin_file {
                let mut file = File::open(path)?;
                io::copy(&mut file, &mut pipe)?;
            }
            pipe.flush()?;
            Ok(())
        })();
        drop(pipe);
        result
    } else {
        Ok(())
    };

    let status = child
        .wait()
        .map_err(|error| Error::other(format!("could not wait for ssh: {error}")))?;
    let stdout = stdout_thread.join().unwrap_or_else(|_| Vec::new());
    let _ = stderr_thread.join();
    let stderr = String::from_utf8_lossy(&lock(&stderr_buf)).into_owned();
    if let Err(error) = copy_result
        && status.success()
    {
        return Err(Error::other(format!("could not write to ssh: {error}")));
    }
    Ok(SshOutput {
        status,
        stdout,
        stderr,
    })
}

pub(crate) fn spawn(
    program: &Path,
    port: Option<u16>,
    host: &str,
    remote_args: &[String],
    askpass: Option<&AskpassSession>,
) -> Result<Child> {
    ensure_destination(host)?;
    let argv = ssh_argv(program, port, host, remote_args);
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(askpass) = askpass {
        askpass.apply(&mut command);
    }
    hide_console(&mut command);
    command
        .spawn()
        .map_err(|error| Error::other(format!("could not run {}: {error}", program.display())))
}

/// Local `ssh` argv. Arguments after `--` are quoted because the remote
/// `sshd` joins them and runs the result through the user's login shell.
pub(crate) fn ssh_argv(
    program: &Path,
    port: Option<u16>,
    host: &str,
    remote_args: &[String],
) -> Vec<std::ffi::OsString> {
    let quoted: Vec<std::ffi::OsString> = remote_args
        .iter()
        .map(|arg| shell_quote(arg).into())
        .collect();
    let proxy = proxy_for_host(program, host, port);
    ssh_argv_with(program, port, host, Tty::None, &quoted, proxy.as_deref())
}

/// The local `ssh` argv for an interactive login shell in `repo_path`.
///
/// The connection half is the one [`ssh_argv`] gives the session, so a terminal
/// reaches the host exactly the way the tab behind it does. It differs in
/// asking for a tty and in the remote command, which arrives ready to run
/// instead of as separate arguments to quote.
///
/// The destination is checked the way [`spawn`] checks it: a host beginning
/// with '-' would be option-parsed before `--` is ever read, so no argv is
/// built for it.
pub fn terminal_argv(
    program: &Path,
    port: Option<u16>,
    host: &str,
    repo_path: &str,
) -> Result<Vec<std::ffi::OsString>> {
    ensure_destination(host)?;
    let proxy = proxy_for_host(program, host, port);
    Ok(ssh_argv_with(
        program,
        port,
        host,
        Tty::Interactive,
        &[login_command(repo_path).into()],
        proxy.as_deref(),
    ))
}

/// The remote command a terminal runs: enter the repository and replace that
/// shell with the host's own login shell, so job control and the prompt are the
/// ones the user would get after typing `ssh` themselves.
pub fn login_command(repo_path: &str) -> String {
    format!("cd {} && exec \"$SHELL\" -l", cd_target(repo_path))
}

/// `~` and `~/rest` need the remote shell to expand the home directory, which
/// only happens outside quotes. Everything else is single-quoted for it.
fn cd_target(repo_path: &str) -> String {
    if repo_path == "~" {
        return "\"$HOME\"".to_owned();
    }
    match repo_path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None => shell_quote(repo_path),
    }
}

/// Whether the ssh process needs a remote pty. A shell needs one; the protocol
/// transport must not have one, or the helper's stdout would carry terminal
/// control sequences.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tty {
    None,
    Interactive,
}

fn ssh_argv_with(
    program: &Path,
    port: Option<u16>,
    host: &str,
    tty: Tty,
    remote: &[std::ffi::OsString],
    proxy: Option<&str>,
) -> Vec<std::ffi::OsString> {
    let mut argv = Vec::with_capacity(18 + remote.len());
    argv.push(program.as_os_str().to_os_string());
    argv.push(match tty {
        Tty::None => "-T".into(),
        Tty::Interactive => "-t".into(),
    });
    argv.extend([
        "-o".into(),
        format!("ConnectTimeout={}", CONNECT_TIMEOUT.as_secs()).into(),
        "-o".into(),
        format!("ServerAliveInterval={}", SERVER_ALIVE_INTERVAL.as_secs()).into(),
        "-o".into(),
        format!("ServerAliveCountMax={SERVER_ALIVE_COUNT_MAX}").into(),
        "-o".into(),
        "NumberOfPasswordPrompts=1".into(),
    ]);
    if let Some(proxy) = proxy {
        argv.push("-o".into());
        argv.push(format!("ProxyCommand={proxy}").into());
    }
    if let Some(port) = port {
        argv.push("-p".into());
        argv.push(port.to_string().into());
    }
    argv.push(host.into());
    argv.push("--".into());
    argv.extend(remote.iter().cloned());
    argv
}

/// The `ProxyCommand` value for `host`, or `None` when the connection goes
/// direct.
///
/// The app publishes the proxy in the environment; `LAMINA_NO_PROXY` decides
/// which hosts bypass it. `ssh` on Windows has no `nc`, so the tunnel is this
/// executable's hidden connector rather than an external program.
fn proxy_for_host(program: &Path, host: &str, port: Option<u16>) -> Option<String> {
    if !lamina_core::proxy::ssh::applies_to(host, port.unwrap_or(22)) {
        return None;
    }
    // `ssh -G` option-parses everything before the destination, so a host
    // beginning with '-' cannot be inspected here. `spawn` refuses it right
    // after, and no proxy is needed for a connection that never starts.
    if host.starts_with('-') {
        return None;
    }
    // A ProxyCommand or ProxyJump in the user's own ssh config is a deliberate
    // route to that host (a bastion, a jump host); the app proxy must not
    // silently replace it.
    if ssh_config_already_forwards(program, host, port) {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    Some(lamina_core::proxy::ssh::proxy_command(&executable))
}

/// Reads `ssh -G` for the effective configuration. Anything unexpected is
/// treated as "the user configured something", so an unreadable config leaves
/// the user's own route in place instead of overriding it.
fn ssh_config_already_forwards(program: &Path, host: &str, port: Option<u16>) -> bool {
    let mut command = Command::new(program);
    command.arg("-G");
    if let Some(port) = port {
        command.arg("-p").arg(port.to_string());
    }
    command.arg(host);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    hide_console(&mut command);
    let Ok(output) = command.output() else {
        return true;
    };
    config_forwards(&String::from_utf8_lossy(&output.stdout))
}

/// Reads the keys `ssh -G` prints. An unset `ProxyCommand` or `ProxyJump`
/// prints as `none` on some releases and is omitted on others; either way it
/// is not a user route.
fn config_forwards(config: &str) -> bool {
    config
        .lines()
        .filter_map(|line| line.trim().split_once(' '))
        .filter(|(key, _)| {
            key.eq_ignore_ascii_case("proxycommand") || key.eq_ignore_ascii_case("proxyjump")
        })
        .any(|(_, value)| {
            let value = value.trim();
            !value.is_empty() && !value.eq_ignore_ascii_case("none")
        })
}

/// POSIX single quotes. Safe tokens are left alone so the command line stays
/// readable and tests can match it exactly.
pub(crate) fn shell_quote(value: &str) -> String {
    if is_shell_safe(value) {
        return value.to_owned();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn is_shell_safe(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'/' | b'.' | b'_' | b'-' | b'+' | b':' | b'@' | b'%' | b'='
                )
        })
}

/// A destination `ssh` would parse as an option instead of a host.
///
/// Everything before the destination is option-parsed, so a host beginning
/// with '-' is read as `ssh` flags (`-oProxyCommand=…`, `-F …`) and never
/// reaches the network as an address. There is no quoting that helps: the
/// destination is not a remote-shell word. Real hosts, addresses and
/// `user@host` forms never begin with '-', so refusing is unambiguous.
fn ensure_destination(host: &str) -> Result<()> {
    if host.starts_with('-') {
        return Err(Error::other(format!(
            "the remote host may not begin with '-': {host}"
        )));
    }
    Ok(())
}

fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: stdout is the protocol, so a console must not flash.
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

pub(crate) fn capture_stderr(mut stderr: impl Read, buf: &Mutex<Vec<u8>>) {
    let mut chunk = [0u8; 1024];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => append_tail(buf, &chunk[..n]),
        }
    }
}

fn append_tail(buf: &Mutex<Vec<u8>>, bytes: &[u8]) {
    let mut guard = lock(buf);
    guard.extend_from_slice(bytes);
    if guard.len() > STDERR_CAP {
        let extra = guard.len() - STDERR_CAP;
        guard.drain(..extra);
    }
}

fn read_capped(mut reader: impl Read, cap: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // Keep reading past the cap so the peer is not stuck on a full
                // pipe; only the stored prefix is returned.
                if buf.len() < cap {
                    let room = cap - buf.len();
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
    buf
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_carries_timeouts_port_and_the_remote_command() {
        let argv = ssh_argv(
            Path::new("ssh"),
            Some(2222),
            "example.test",
            &[
                "/opt/lamina-remote".into(),
                "--stdio".into(),
                "--root".into(),
                "/srv/repo".into(),
            ],
        );
        let text: Vec<String> = argv
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            text,
            vec![
                "ssh",
                "-T",
                "-o",
                "ConnectTimeout=15",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "NumberOfPasswordPrompts=1",
                "-p",
                "2222",
                "example.test",
                "--",
                "/opt/lamina-remote",
                "--stdio",
                "--root",
                "/srv/repo",
            ]
        );
        assert!(
            text.iter()
                .all(|arg| !arg.contains("StrictHostKeyChecking")),
            "host-key checking must stay at the user's ssh config"
        );
    }

    #[test]
    fn remote_arguments_with_spaces_are_single_quoted() {
        let argv = ssh_argv(
            Path::new("ssh"),
            None,
            "host",
            &["/srv/my repo".into(), "a'b".into()],
        );
        let text: Vec<String> = argv
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        assert!(!text.iter().any(|arg| arg == "-p"));
        assert_eq!(text[text.len() - 2], "'/srv/my repo'");
        assert_eq!(text[text.len() - 1], "'a'\\''b'");
    }

    /// Everything up to the remote command, minus the flag that decides whether
    /// ssh allocates a tty.
    fn connection_args(argv: &[std::ffi::OsString]) -> Vec<String> {
        argv.iter()
            .map(|part| part.to_string_lossy().into_owned())
            .take_while(|arg| arg != "--")
            .filter(|arg| arg != "-t" && arg != "-T")
            .collect()
    }

    #[test]
    fn the_terminal_reaches_the_host_the_way_the_session_does() {
        let program = Path::new("ssh");
        let session = ssh_argv(
            program,
            Some(2222),
            "git@example.test",
            &["/opt/lamina-remote".into(), "--stdio".into()],
        );
        let terminal = terminal_argv(program, Some(2222), "git@example.test", "/srv/repo");
        let terminal = terminal.expect("a real host builds a terminal command");
        assert_eq!(
            connection_args(&session),
            connection_args(&terminal),
            "a terminal repeats the address, port and options of the session"
        );
        let shared = connection_args(&terminal);
        assert_eq!(shared.last().map(String::as_str), Some("git@example.test"));
        assert!(shared.iter().any(|arg| arg == "2222"));
        assert!(shared.iter().any(|arg| arg == "ConnectTimeout=15"));

        let terminal: Vec<String> = terminal
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        assert_eq!(terminal[1], "-t", "a shell needs a remote tty");
        assert_eq!(
            session[1].to_string_lossy(),
            "-T",
            "the protocol stream must stay free of terminal control sequences"
        );
    }

    #[test]
    fn the_login_command_quotes_the_repository_path() {
        assert_eq!(
            login_command("/srv/repo"),
            "cd /srv/repo && exec \"$SHELL\" -l"
        );
        assert_eq!(
            login_command("/srv/my repo"),
            "cd '/srv/my repo' && exec \"$SHELL\" -l"
        );
        assert_eq!(
            login_command("/srv/it's here"),
            "cd '/srv/it'\\''s here' && exec \"$SHELL\" -l"
        );
        assert_eq!(
            login_command("/srv/$HOME/`id`"),
            "cd '/srv/$HOME/`id`' && exec \"$SHELL\" -l",
            "a path is data, never a command the remote shell may expand"
        );
        assert_eq!(
            login_command("~/my repo"),
            "cd \"$HOME\"/'my repo' && exec \"$SHELL\" -l"
        );
        assert_eq!(login_command("~"), "cd \"$HOME\" && exec \"$SHELL\" -l");
    }

    #[test]
    fn the_terminal_remote_command_is_one_argument() {
        let argv = terminal_argv(Path::new("ssh"), None, "host", "/srv/my repo");
        let argv = argv.expect("a real host builds a terminal command");
        let text: Vec<String> = argv
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        assert!(!text.iter().any(|arg| arg == "-p"));
        assert_eq!(text[text.len() - 2], "--");
        assert_eq!(
            text[text.len() - 1],
            "cd '/srv/my repo' && exec \"$SHELL\" -l",
            "sshd joins the arguments after `--` into one command line"
        );
    }

    #[test]
    fn a_proxy_command_is_inserted_before_the_host() {
        let proxy = "/opt/lamina --proxy-connect %h %p";
        let argv = ssh_argv_with(
            Path::new("ssh"),
            Some(2222),
            "example.test",
            Tty::None,
            &["/opt/lamina-remote".into(), "--stdio".into()],
            Some(proxy),
        );
        let text: Vec<String> = argv
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        let option = text
            .iter()
            .position(|arg| arg == &format!("ProxyCommand={proxy}"))
            .expect("the tunnel is an ssh option");
        assert_eq!(text[option - 1], "-o");
        let host = text
            .iter()
            .position(|arg| arg == "example.test")
            .expect("host");
        assert!(option < host, "options must precede the destination");
        assert_eq!(text[host + 1], "--");
        assert_eq!(text[host + 2], "/opt/lamina-remote");
    }

    #[test]
    fn the_users_own_forwarding_wins_over_the_app_proxy() {
        assert!(!config_forwards("host example.test\nproxycommand none\n"));
        assert!(!config_forwards("host example.test\nport 22\n"));
        assert!(config_forwards(
            "host example.test\nproxyjump bastion.example\n"
        ));
        assert!(config_forwards(
            "host example.test\nproxycommand /usr/bin/nc %h %p\n"
        ));
        assert!(config_forwards(
            "host example.test\nProxyCommand ssh -W %h:%p bastion\n"
        ));
    }

    #[test]
    fn a_host_beginning_with_a_dash_is_refused_as_an_ssh_destination() {
        // Everything before the destination is option-parsed by ssh, so such
        // a host would be read as flags instead of an address.
        assert!(ensure_destination("-oProxyCommand=touch /tmp/pwned").is_err());
        assert!(ensure_destination("-F /etc/ssh/other_config").is_err());
        assert!(ensure_destination("example.test").is_ok());
        assert!(ensure_destination("[::1]").is_ok());
        assert!(ensure_destination("git@example.test").is_ok());
    }

    #[test]
    fn a_terminal_refuses_a_destination_ssh_would_parse_as_options() {
        // The terminal once built its argv directly, which put such a host
        // before `--` and let ssh run the injected command locally.
        let error = terminal_argv(
            Path::new("ssh"),
            None,
            "-oProxyCommand=touch /tmp/lamina-terminal-pwned",
            "/srv/repo",
        )
        .expect_err("a host that parses as ssh options is not a destination");
        assert!(
            error.to_string().contains("may not begin with '-'"),
            "{error}"
        );
    }

    #[test]
    fn stderr_capture_keeps_only_the_tail() {
        let buf = Mutex::new(Vec::new());
        append_tail(&buf, &[b'a'; 100]);
        append_tail(&buf, &vec![b'b'; STDERR_CAP]);
        let stored = buf.lock().expect("stderr buffer");
        assert_eq!(stored.len(), STDERR_CAP);
        assert!(stored.iter().all(|byte| *byte == b'b'));
    }
}
