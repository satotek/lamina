//! Write operations through the `git` executable.
//!
//! Reads go through `gix`; every mutation runs the real `git` so user hooks,
//! signing, credential helpers and LFS behave exactly as in a terminal.
//! Invocations are recorded in a [`CommandLog`] for the UI's command log.
//!
//! Path lists are normally passed after `--`. Long lists go through
//! `--pathspec-from-file=-` with NUL separators instead, because Windows caps
//! a process command line at roughly 32 KiB.
//!
//! Network commands also get an askpass channel (see [`crate::askpass`]) when
//! the handle was built with [`Git::with_askpass`], so git can ask the window
//! for a username, password or ssh passphrase instead of failing.

mod blame;
mod clone;
mod command_log;
mod history;
mod manage;
mod merge;
mod ops;
mod progress;
mod rebase;

pub use clone::{CloneOptions, CloneOutcome, clone_repository, init_repository};
pub use command_log::{CommandLog, CommandRecord, MAX_CAPTURE_BYTES, MAX_LOG_ENTRIES, StreamLine};
pub use history::CherryPickOutcome;
pub use ops::{ApplyMode, CommitOptions, StashEntry};
pub use rebase::{
    MoveDirection, RebaseAction, RebaseStep, drop_commit, move_commit, reword_commit,
    squash_commit_into_parent,
};

use std::ffi::OsString;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::askpass::{Answer, AskpassHost, Prompt};
use crate::{Error, Result};

/// Oldest git whose `--pathspec-from-file`, `restore --staged` and `switch`
/// behaviour Strata relies on.
pub const MIN_GIT_VERSION: (u32, u32, u32) = (2, 30, 0);

/// Handle for running `git` in one working tree.
///
/// Cheap to clone; safe to share across threads. All calls are blocking, so
/// the UI should run them on its executor rather than the render thread.
#[derive(Clone)]
pub struct Git {
    workdir: PathBuf,
    binary: PathBuf,
    env: Vec<(OsString, OsString)>,
    log: Arc<CommandLog>,
    /// Answers credential prompts for network commands. `None` keeps the old
    /// behaviour: git fails when it cannot ask.
    askpass: Option<Arc<dyn Fn(Prompt) -> Option<Answer> + Send + Sync>>,
    /// The executable that implements the helper mode, normally this binary.
    askpass_helper: PathBuf,
}

impl Git {
    /// `workdir` is the directory `git` is run in. A relative path is resolved
    /// against the process working directory now, not at call time.
    pub fn new(workdir: PathBuf) -> Self {
        let workdir = if workdir.is_absolute() {
            workdir
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(workdir)
        } else {
            workdir
        };
        Self {
            workdir,
            // Resolve now so a broken PATH is visible in the log rather than
            // differing between invocations.
            binary: find_git_binary().unwrap_or_else(|| PathBuf::from("git")),
            env: Vec::new(),
            log: Arc::new(CommandLog::new()),
            askpass: None,
            // git runs this path in helper mode; a bundle or an AppImage still
            // starts the same executable.
            askpass_helper: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("strata")),
        }
    }

    /// Use a specific executable instead of the one found on `PATH`.
    pub fn with_git_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }

    /// Add an environment variable for every invocation. Applied after the
    /// defaults and in call order, so tests can override anything.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Share a command log with another handle.
    pub fn with_log(mut self, log: Arc<CommandLog>) -> Self {
        self.log = log;
        self
    }

    /// Answer git's credential questions through `handler`.
    ///
    /// Network commands (`clone`, `fetch`, `pull`, `push`, ...) get a private
    /// askpass channel for their lifetime; the handler is called on the
    /// command's thread and may block until the user answers. `None` cancels.
    /// Commands that never contact a remote get no channel and no askpass
    /// environment.
    pub fn with_askpass(
        mut self,
        handler: impl Fn(Prompt) -> Option<Answer> + Send + Sync + 'static,
    ) -> Self {
        self.askpass = Some(Arc::new(handler));
        self
    }

    /// The executable `GIT_ASKPASS` / `SSH_ASKPASS` point at.
    ///
    /// Defaults to the running executable; tests and embedders that ship the
    /// helper elsewhere can name it explicitly.
    pub fn with_askpass_helper(mut self, program: impl Into<PathBuf>) -> Self {
        self.askpass_helper = program.into();
        self
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Snapshot of every invocation, oldest first.
    pub fn log(&self) -> Vec<CommandRecord> {
        self.log.snapshot()
    }

    /// The shared log itself, for subscribing or sharing with other handles.
    pub fn log_handle(&self) -> Arc<CommandLog> {
        Arc::clone(&self.log)
    }

    pub fn set_log_subscriber(&self, subscriber: impl Fn(&CommandRecord) + Send + Sync + 'static) {
        self.log.set_subscriber(subscriber);
    }

    pub fn clear_log(&self) {
        self.log.clear();
    }

    /// Run `git` and return its output. A non-zero exit becomes
    /// [`Error::GitCommand`] carrying the captured stderr.
    pub fn run<S: AsRef<str>>(&self, args: &[S], stdin: Option<&[u8]>) -> Result<Output> {
        let args = to_args(args);
        let output = self.run_output(&args, stdin)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(command_error(&args, &output))
        }
    }

    /// Run `git` and return its output whatever the exit status; only a spawn
    /// or I/O failure is an error.
    ///
    /// A multi-commit cherry-pick expects exit 1 both for a conflict and for an
    /// empty pick, and needs the streams to tell them apart, so it cannot use
    /// [`run`](Self::run), which folds a failure into a message.
    fn run_output(&self, args: &[String], stdin: Option<&[u8]>) -> Result<Output> {
        let started_at = SystemTime::now();
        let (mut command, _askpass) = match self.prepared(args) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.log_spawn_failure(args, started_at, &error);
                return Err(error);
            }
        };
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let error = Error::from(error);
                self.log_spawn_failure(args, started_at, &error);
                return Err(error);
            }
        };
        if let Some(data) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            let data = data.to_vec();
            // Feed stdin from a thread: git can fill the output pipes
            // before consuming stdin, which would deadlock a blocking write.
            thread::spawn(move || {
                let _ = pipe.write_all(&data);
            });
        }
        let output = child.wait_with_output()?;
        self.log_output(args, started_at, &output);
        Ok(output)
    }

    /// Like [`run`](Self::run), but reports stderr lines as they arrive.
    ///
    /// Pass `--progress` for fetch/push so git emits progress even when stderr
    /// is a pipe. stdout is buffered and returned with the output; only stderr
    /// is streamed, because that is where git writes progress.
    pub fn run_streaming<S: AsRef<str>>(
        &self,
        args: &[S],
        mut on_line: impl FnMut(StreamLine),
    ) -> Result<Output> {
        let args = to_args(args);
        let started_at = SystemTime::now();
        let (mut command, _askpass) = match self.prepared(&args) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.log_spawn_failure(&args, started_at, &error);
                return Err(error);
            }
        };
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let error = Error::from(error);
                self.log_spawn_failure(&args, started_at, &error);
                return Err(error);
            }
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::other("git output pipes are unavailable"));
        };
        let stdout_reader = thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut stdout = stdout;
            let _ = stdout.read_to_end(&mut buffer);
            buffer
        });
        let mut stderr_bytes = Vec::new();
        if let Err(error) = read_stream_lines(stderr, &mut stderr_bytes, &mut |text| {
            on_line(StreamLine { text })
        }) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            return Err(error.into());
        }
        let status = child.wait()?;
        let stdout_bytes = stdout_reader.join().unwrap_or_default();
        let output = Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        };
        self.log_output(&args, started_at, &output);
        if output.status.success() {
            Ok(output)
        } else {
            Err(command_error(&args, &output))
        }
    }

    /// Like [`run`](Self::run), but delivers each stdout line (`\n`-terminated)
    /// to `on_line` while the command runs, and kills the child process if
    /// `cancelled()` becomes true.
    ///
    /// Returns `Ok(true)` when the command finished with exit code 0, or
    /// `Ok(false)` when `cancelled()` stopped it.
    pub(crate) fn run_stdout_cancellable<S: AsRef<str>>(
        &self,
        args: &[S],
        cancelled: &(impl Fn() -> bool + Send + Sync),
        mut on_line: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<bool> {
        if cancelled() {
            return Ok(false);
        }
        let args = to_args(args);
        let started_at = SystemTime::now();
        let (mut command, _askpass) = match self.prepared(&args) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.log_spawn_failure(&args, started_at, &error);
                return Err(error);
            }
        };
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let error = Error::from(error);
                self.log_spawn_failure(&args, started_at, &error);
                return Err(error);
            }
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::other("git output pipes are unavailable"));
        };

        struct WatchState {
            child: Mutex<Option<Child>>,
            done: (Mutex<bool>, Condvar),
            killed: AtomicBool,
        }

        let state = WatchState {
            child: Mutex::new(Some(child)),
            done: (Mutex::new(false), Condvar::new()),
            killed: AtomicBool::new(false),
        };
        let mut stdout_sample = Vec::new();
        let mut was_cancelled = false;

        let (read_result, stderr_bytes, status) = thread::scope(|scope| {
            let stderr_reader = scope.spawn(|| {
                let mut buffer = Vec::new();
                let mut stderr = stderr;
                let _ = stderr.read_to_end(&mut buffer);
                buffer
            });
            let cancel_watcher = scope.spawn(|| {
                loop {
                    if cancelled() {
                        state.killed.store(true, Ordering::SeqCst);
                        if let Some(child) = state
                            .child
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .as_mut()
                        {
                            let _ = child.kill();
                        }
                        break;
                    }
                    let done = state.done.0.lock().unwrap_or_else(PoisonError::into_inner);
                    if *done {
                        break;
                    }
                    let (guard, _) = state
                        .done
                        .1
                        .wait_timeout(done, Duration::from_millis(25))
                        .unwrap_or_else(PoisonError::into_inner);
                    if *guard {
                        break;
                    }
                }
            });

            let read_result = (|| -> Result<()> {
                let mut reader = io::BufReader::new(stdout);
                let mut line = Vec::new();
                loop {
                    if cancelled() {
                        was_cancelled = true;
                        break;
                    }
                    line.clear();
                    let read = reader.read_until(b'\n', &mut line)?;
                    if read == 0 {
                        break;
                    }
                    if cancelled() {
                        was_cancelled = true;
                        break;
                    }
                    if stdout_sample.len() < MAX_CAPTURE_BYTES {
                        let remaining = MAX_CAPTURE_BYTES - stdout_sample.len();
                        let take = remaining.min(line.len());
                        stdout_sample.extend_from_slice(&line[..take]);
                    }
                    on_line(&line)?;
                }
                Ok(())
            })();

            // Wake the watcher before waiting on the child so normal
            // completion never stalls for the polling interval.
            *state.done.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
            state.done.1.notify_all();
            let _ = cancel_watcher.join();

            let mut child = state
                .child
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .expect("child handle present");
            if was_cancelled || read_result.is_err() || cancelled() {
                let _ = child.kill();
            }
            let status = child.wait();
            let stderr_bytes = stderr_reader.join().unwrap_or_default();
            (read_result, stderr_bytes, status)
        });

        let status = status?;
        let output = Output {
            status,
            stdout: stdout_sample,
            stderr: stderr_bytes,
        };
        self.log_output(&args, started_at, &output);

        if was_cancelled || state.killed.load(Ordering::SeqCst) || cancelled() {
            return Ok(false);
        }
        read_result?;
        if output.status.success() {
            Ok(true)
        } else {
            Err(command_error(&args, &output))
        }
    }

    /// Like [`run_stdout_cancellable`](Self::run_stdout_cancellable) for a
    /// command that reports progress on stderr, such as `git commit-graph
    /// write --progress`. Both `\n` and `\r` end a line, so a progress counter
    /// that rewrites itself is reported as it moves.
    ///
    /// Returns `Ok(true)` when the command finished with exit code 0, or
    /// `Ok(false)` when `cancelled()` stopped it.
    pub(crate) fn run_stderr_cancellable<S: AsRef<str>>(
        &self,
        args: &[S],
        cancelled: &(impl Fn() -> bool + Send + Sync),
        mut on_line: impl FnMut(&str),
    ) -> Result<bool> {
        if cancelled() {
            return Ok(false);
        }
        let args = to_args(args);
        let started_at = SystemTime::now();
        let (mut command, _askpass) = match self.prepared(&args) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.log_spawn_failure(&args, started_at, &error);
                return Err(error);
            }
        };
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let error = Error::from(error);
                self.log_spawn_failure(&args, started_at, &error);
                return Err(error);
            }
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::other("git output pipes are unavailable"));
        };

        struct WatchState {
            child: Mutex<Option<Child>>,
            done: (Mutex<bool>, Condvar),
            killed: AtomicBool,
        }

        let state = WatchState {
            child: Mutex::new(Some(child)),
            done: (Mutex::new(false), Condvar::new()),
            killed: AtomicBool::new(false),
        };
        let mut stderr_bytes = Vec::new();

        let (read_result, stdout_bytes, status) = thread::scope(|scope| {
            let stdout_reader = scope.spawn(|| {
                let mut buffer = Vec::new();
                let mut stdout = stdout;
                let _ = stdout.read_to_end(&mut buffer);
                buffer
            });
            let cancel_watcher = scope.spawn(|| {
                loop {
                    if cancelled() {
                        state.killed.store(true, Ordering::SeqCst);
                        if let Some(child) = state
                            .child
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .as_mut()
                        {
                            let _ = child.kill();
                        }
                        break;
                    }
                    let done = state.done.0.lock().unwrap_or_else(PoisonError::into_inner);
                    if *done {
                        break;
                    }
                    let (guard, _) = state
                        .done
                        .1
                        .wait_timeout(done, Duration::from_millis(25))
                        .unwrap_or_else(PoisonError::into_inner);
                    if *guard {
                        break;
                    }
                }
            });

            let read_result =
                read_stream_lines(stderr, &mut stderr_bytes, &mut |text| on_line(&text));

            // Wake the watcher before waiting on the child so normal
            // completion never stalls for the polling interval.
            *state.done.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
            state.done.1.notify_all();
            let _ = cancel_watcher.join();

            let mut child = state
                .child
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .expect("child handle present");
            if read_result.is_err() || cancelled() {
                let _ = child.kill();
            }
            let status = child.wait();
            let stdout_bytes = stdout_reader.join().unwrap_or_default();
            (read_result, stdout_bytes, status)
        });

        let status = status?;
        let output = Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        };
        self.log_output(&args, started_at, &output);

        if state.killed.load(Ordering::SeqCst) || cancelled() {
            return Ok(false);
        }
        read_result?;
        if output.status.success() {
            Ok(true)
        } else {
            Err(command_error(&args, &output))
        }
    }

    /// Parse `git --version`, e.g. `2.39.3 (Apple Git-145)` into `(2, 39, 3)`.
    pub fn version(&self) -> Result<(u32, u32, u32)> {
        let output = self.run(&["--version"], None)?;
        let text = String::from_utf8_lossy(&output.stdout);
        parse_version(&text).ok_or_else(|| {
            Error::other(format!(
                "could not parse `git --version` output: {}",
                text.trim()
            ))
        })
    }

    /// Fail early with a helpful message when the installed git is too old.
    pub fn ensure_supported_version(&self) -> Result<()> {
        let found = self.version()?;
        if found < MIN_GIT_VERSION {
            return Err(Error::other(format!(
                "git {} is too old; Strata needs {}.{}.{} or newer",
                format_version(found),
                MIN_GIT_VERSION.0,
                MIN_GIT_VERSION.1,
                MIN_GIT_VERSION.2,
            )));
        }
        Ok(())
    }

    /// True when HEAD resolves to a commit. Unborn branches report `false` and
    /// need different index commands (see `unstage_paths`).
    fn has_head(&self) -> Result<bool> {
        match self.run(&["rev-parse", "--verify", "--quiet", "HEAD"], None) {
            Ok(_) => Ok(true),
            Err(Error::GitCommand { code: Some(1), .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn run_unit(&self, args: &[String], stdin: Option<Vec<u8>>) -> Result<()> {
        self.run(args, stdin.as_deref()).map(|_| ())
    }

    /// The configured command plus the askpass channel it needs, if any.
    ///
    /// The host is returned instead of installed on the command because it has
    /// to outlive the child: dropping it closes the channel the helper is
    /// still meant to reach.
    fn prepared(&self, args: &[String]) -> Result<(Command, Option<AskpassHost>)> {
        let mut command = self.command(args);
        let host = match &self.askpass {
            Some(handler) if is_network_command(args) => {
                let host = AskpassHost::start_with_handler(
                    Arc::clone(handler),
                    crate::askpass::IO_TIMEOUT,
                )
                .map_err(|error| {
                    Error::other(format!(
                        "could not start the credential prompt channel: {error}"
                    ))
                })?;
                host.apply(&mut command, &self.askpass_helper);
                Some(host)
            }
            _ => None,
        };
        Ok((command, host))
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.binary);
        command.current_dir(&self.workdir).args(args);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Never prompt on a terminal: the askpass bridge, when there is one,
        // answers for git; without one the command fails loudly instead of
        // hanging a GUI process.
        command.env("GIT_TERMINAL_PROMPT", "0");
        // Git's messages are parsed (branch names, stash list), so pin English.
        command.env("LC_ALL", "C");
        if is_read_only(args) {
            // Avoid touching the index just for a read; the UI watches mtimes.
            command.env("GIT_OPTIONAL_LOCKS", "0");
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: no console window flashes for every git call.
            command.creation_flags(0x0800_0000);
        }
        command
    }

    fn log_output(&self, args: &[String], started_at: SystemTime, output: &Output) {
        let duration = started_at.elapsed().unwrap_or_default();
        self.log.record(CommandRecord::from_output(
            args.to_vec(),
            started_at,
            duration,
            output,
        ));
    }

    fn log_spawn_failure(&self, args: &[String], started_at: SystemTime, error: &Error) {
        let duration = started_at.elapsed().unwrap_or_default();
        let text = error.to_string();
        self.log.record(CommandRecord::capture(
            args.to_vec(),
            started_at,
            duration,
            None,
            &[],
            text.as_bytes(),
        ));
    }
}

fn to_args<S: AsRef<str>>(args: &[S]) -> Vec<String> {
    args.iter().map(|arg| arg.as_ref().to_owned()).collect()
}

fn command_error(args: &[String], output: &Output) -> Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Some failures (e.g. partial clone warnings) only explain themselves on
    // stdout; an empty stderr would make a useless error message.
    let detail = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Error::GitCommand {
        args: args.join(" "),
        code: output.status.code(),
        stderr: detail.trim().to_owned(),
    }
}

/// Read `reader` line by line, treating both `\n` and `\r` as terminators so
/// git's carriage-return progress updates surface immediately.
fn read_stream_lines(
    mut reader: impl Read,
    sink: &mut Vec<u8>,
    on_line: &mut impl FnMut(String),
) -> io::Result<()> {
    let mut chunk = [0u8; 8192];
    let mut pending = Vec::new();
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        sink.extend_from_slice(&chunk[..read]);
        pending.extend_from_slice(&chunk[..read]);
        let mut emitted = 0;
        for (index, byte) in pending.iter().enumerate() {
            if *byte == b'\n' || *byte == b'\r' {
                if index > emitted {
                    on_line(String::from_utf8_lossy(&pending[emitted..index]).into_owned());
                }
                emitted = index + 1;
            }
        }
        pending.drain(..emitted);
    }
    if !pending.is_empty() {
        on_line(String::from_utf8_lossy(&pending).into_owned());
    }
    Ok(())
}

fn parse_version(output: &str) -> Option<(u32, u32, u32)> {
    let token = output
        .lines()
        .next()?
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut numbers = [0u32; 3];
    let mut found = 0;
    for (slot, part) in numbers.iter_mut().zip(token.split('.')) {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        match digits.parse() {
            Ok(number) => {
                *slot = number;
                found += 1;
            }
            Err(_) => break,
        }
    }
    (found > 0).then_some((numbers[0], numbers[1], numbers[2]))
}

fn format_version(version: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

fn find_git_binary() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(format!("git{}", std::env::consts::EXE_SUFFIX)))
        .find(|candidate| candidate.is_file())
}

/// Commands that may contact a remote, and so may ask for a credential.
///
/// `pull` fetches, and `submodule update` / `remote update` clone or fetch too.
/// Every other command runs without an askpass channel.
fn is_network_command(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("clone" | "fetch" | "pull" | "push" | "ls-remote") => true,
        Some("submodule") => matches!(args.get(1).map(String::as_str), Some("update")),
        Some("remote") => matches!(args.get(1).map(String::as_str), Some("update")),
        _ => false,
    }
}

/// Commands that only read. They get `GIT_OPTIONAL_LOCKS=0` so listing the
/// repo does not rewrite the index mtime behind the file watcher's back.
fn is_read_only(args: &[String]) -> bool {
    let mut rest = args.iter().map(String::as_str);
    let Some(subcommand) = rest.next() else {
        return false;
    };
    match subcommand {
        "--version" | "version" | "status" | "log" | "diff" | "show" | "rev-parse" | "rev-list"
        | "for-each-ref" | "cat-file" | "ls-files" | "ls-tree" | "blame" | "shortlog"
        | "describe" | "check-ref-format" => true,
        "stash" => matches!(rest.next(), Some("list" | "show")),
        "branch" => {
            let rest: Vec<&str> = rest.collect();
            rest.is_empty() || rest.iter().any(|arg| *arg == "-l" || *arg == "--list")
        }
        "remote" => {
            let rest: Vec<&str> = rest.collect();
            rest.is_empty() || rest == ["-v"]
        }
        _ => false,
    }
}

/// Reject names that would be parsed as options. Everything else (including
/// names git itself refuses) is left to git so its message reaches the user.
fn checked_ref<'a>(name: &'a str, what: &str) -> Result<&'a str> {
    if name.is_empty() {
        return Err(Error::other(format!("{what} name is empty")));
    }
    if name.starts_with('-') {
        return Err(Error::other(format!(
            "{what} name must not start with `-`: {name}"
        )));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn parses_plain_and_vendor_versions() {
        assert_eq!(parse_version("git version 2.56.0\n"), Some((2, 56, 0)));
        assert_eq!(
            parse_version("git version 2.39.3 (Apple Git-145)\n"),
            Some((2, 39, 3))
        );
        assert_eq!(
            parse_version("git version 2.42.0.windows.1\n"),
            Some((2, 42, 0))
        );
        assert_eq!(parse_version("git version 3\n"), Some((3, 0, 0)));
        assert_eq!(parse_version("nonsense"), None);
    }
}

#[cfg(all(test, unix))]
mod askpass_tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::Git;

    /// A stand-in for git that reports the environment it was given.
    fn environment_reporter(directory: &Path) -> PathBuf {
        let path = directory.join("fake-git");
        std::fs::write(
            &path,
            "#!/bin/sh\n\
             echo \"GIT_ASKPASS=${GIT_ASKPASS-}\"\n\
             echo \"SSH_ASKPASS=${SSH_ASKPASS-}\"\n\
             echo \"SSH_ASKPASS_REQUIRE=${SSH_ASKPASS_REQUIRE-}\"\n\
             echo \"GIT_TERMINAL_PROMPT=${GIT_TERMINAL_PROMPT-}\"\n\
             echo \"ENDPOINT=${STRATA_ASKPASS_ENDPOINT-}\"\n\
             echo \"TOKEN=${STRATA_ASKPASS_TOKEN-}\"\n\
             if [ -S \"${STRATA_ASKPASS_ENDPOINT-}\" ]; then echo socket=yes; else echo socket=no; fi\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn value<'a>(output: &'a str, key: &str) -> &'a str {
        output
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("{key} is missing from the environment:\n{output}"))
    }

    #[test]
    fn network_commands_get_the_askpass_bridge_and_others_do_not() {
        let temp = TempDir::new().unwrap();
        let binary = environment_reporter(temp.path());
        let runner = Git::new(temp.path().to_path_buf())
            .with_git_binary(&binary)
            // The helper path is asserted literally; pointing it at the test
            // binary would run the test suite again if git ever invoked it.
            .with_askpass_helper("/opt/strata")
            .with_askpass(|_| None)
            // A developer's own environment must not decide the assertions.
            .with_env("GIT_ASKPASS", "")
            .with_env("SSH_ASKPASS", "")
            .with_env("SSH_ASKPASS_REQUIRE", "")
            .with_env("STRATA_ASKPASS_ENDPOINT", "")
            .with_env("STRATA_ASKPASS_TOKEN", "");

        let output = runner.run(&["fetch", "origin"], None).unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        assert_eq!(value(&text, "GIT_ASKPASS"), "/opt/strata");
        assert_eq!(value(&text, "SSH_ASKPASS"), "/opt/strata");
        assert_eq!(value(&text, "SSH_ASKPASS_REQUIRE"), "force");
        assert_eq!(
            value(&text, "GIT_TERMINAL_PROMPT"),
            "0",
            "the bridge must not re-enable terminal prompts"
        );
        assert_eq!(value(&text, "socket"), "yes", "the channel is live");
        let endpoint = value(&text, "ENDPOINT").to_owned();
        assert!(!value(&text, "TOKEN").is_empty());

        // The host is dropped with the command, so the socket is gone.
        assert!(
            !Path::new(&endpoint).exists(),
            "the askpass socket must not outlive the command"
        );

        let output = runner.run(&["status"], None).unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        assert_eq!(
            value(&text, "GIT_ASKPASS"),
            "",
            "a local command must not start a credential channel"
        );
        assert_eq!(value(&text, "ENDPOINT"), "");
        assert_eq!(value(&text, "TOKEN"), "");
    }

    #[test]
    fn a_network_command_without_a_handler_gets_no_bridge() {
        let temp = TempDir::new().unwrap();
        let binary = environment_reporter(temp.path());
        let runner = Git::new(temp.path().to_path_buf())
            .with_git_binary(&binary)
            .with_env("GIT_ASKPASS", "")
            .with_env("STRATA_ASKPASS_ENDPOINT", "");

        let output = runner.run(&["push"], None).unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        assert_eq!(value(&text, "GIT_ASKPASS"), "");
        assert_eq!(value(&text, "ENDPOINT"), "");
    }
}
