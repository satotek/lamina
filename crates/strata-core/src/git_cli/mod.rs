//! Write operations through the `git` executable.
//!
//! Reads go through `gix`; every mutation runs the real `git` so user hooks,
//! signing, credential helpers and LFS behave exactly as in a terminal.
//! Invocations are recorded in a [`CommandLog`] for the UI's command log.
//!
//! Path lists are normally passed after `--`. Long lists go through
//! `--pathspec-from-file=-` with NUL separators instead, because Windows caps
//! a process command line at roughly 32 KiB.

mod command_log;
mod merge;
mod ops;
mod progress;

pub use command_log::{CommandLog, CommandRecord, MAX_CAPTURE_BYTES, MAX_LOG_ENTRIES, StreamLine};
pub use ops::{ApplyMode, CommitOptions, StashEntry};

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

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
        let started_at = SystemTime::now();
        let mut command = self.command(&args);
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.log_spawn_failure(&args, started_at, &error);
                return Err(error.into());
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
        self.log_output(&args, started_at, &output);
        if output.status.success() {
            Ok(output)
        } else {
            Err(command_error(&args, &output))
        }
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
        let mut child = match self.command(&args).spawn() {
            Ok(child) => child,
            Err(error) => {
                self.log_spawn_failure(&args, started_at, &error);
                return Err(error.into());
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

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.binary);
        command.current_dir(&self.workdir).args(args);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Never prompt: a credential helper that needs a terminal must fail
        // loudly instead of hanging a GUI process.
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

    fn log_spawn_failure(&self, args: &[String], started_at: SystemTime, error: &io::Error) {
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
        | "describe" => true,
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
