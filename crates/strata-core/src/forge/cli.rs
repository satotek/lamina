//! Running the optional forge command-line programs.
//!
//! Every call runs without a shell, without a terminal to prompt on, and with
//! a hard timeout, so a hung or logged-out CLI cannot pin a background worker
//! or ask a question nobody can see.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// Runs `binary` with `args` in `workdir`, with stdin closed and a hard
/// timeout. `program` names the executable in the error a failed call reports.
pub(crate) fn run(
    binary: &Path,
    args: &[&str],
    workdir: &Path,
    timeout: Duration,
    program: &str,
    envs: &[(&str, &str)],
) -> Result<Output> {
    let output = output(binary, args, workdir, timeout, program, envs)?;
    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Err(Error::other(format!(
        "{program} {} failed: {}",
        args.first().copied().unwrap_or("command"),
        short_reason(&detail)
    )))
}

fn output(
    binary: &Path,
    args: &[&str],
    workdir: &Path,
    timeout: Duration,
    program: &str,
    envs: &[(&str, &str)],
) -> Result<Output> {
    let mut command = crate::tools::process(binary);
    command
        .args(args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in envs {
        command.env(name, value);
    }
    configure(&mut command);

    let child = command
        .spawn()
        .map_err(|error| Error::other(format!("could not run `{program}`: {error}")))?;
    wait_with_timeout(child, timeout, program)
}

/// Applies the process settings every call shares on its platform.
fn configure(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // No console window flashes for a background call.
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

/// Waits for `child`, killing it after `timeout`.
///
/// The pipes are drained on their own threads: a large list can fill a pipe
/// buffer, and a child blocked writing into it would never reach its exit
/// status.
fn wait_with_timeout(mut child: Child, timeout: Duration, program: &str) -> Result<Output> {
    let stdout = child.stdout.take().map(read_in_background);
    let stderr = child.stderr.take().map(read_in_background);
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.map(Join::join);
                let _ = stderr.map(Join::join);
                return Err(Error::other(format!(
                    "{program} did not answer within {} seconds",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                return Err(Error::other(format!(
                    "could not wait for {program}: {error}"
                )));
            }
        }
    };

    Ok(Output {
        status,
        stdout: stdout.map(Join::join).unwrap_or_default(),
        stderr: stderr.map(Join::join).unwrap_or_default(),
    })
}

/// A reader thread plus its handle, kept together so the caller cannot join
/// before taking the bytes.
struct Join(std::thread::JoinHandle<Vec<u8>>);

impl Join {
    fn join(self) -> Vec<u8> {
        self.0.join().unwrap_or_default()
    }
}

fn read_in_background<R: Read + Send + 'static>(mut stream: R) -> Join {
    Join(std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        bytes
    }))
}

/// The first line of a command's complaint, which is the one that names the
/// problem. Long lines are cut so the status bar stays readable.
pub(crate) fn short_reason(detail: &str) -> String {
    let line = detail
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output");
    let mut reason: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        reason.push('…');
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_keep_one_short_line() {
        let error = "HTTP 403: Resource not accessible by integration\n\nTry gh auth login\n";
        assert_eq!(
            short_reason(error),
            "HTTP 403: Resource not accessible by integration"
        );
        assert_eq!(short_reason(" \n\t\n"), "no output");
        let long = "x".repeat(500);
        let reason = short_reason(&long);
        assert_eq!(reason.chars().count(), 201);
        assert!(reason.ends_with('…'));
    }
}
