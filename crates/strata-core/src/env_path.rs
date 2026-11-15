//! The PATH a desktop launch inherits.
//!
//! macOS starts an app opened from Finder, the Dock or a `.dmg` with launchd's
//! minimal environment, and a Linux desktop file starts it much the same way:
//! the process sees `/usr/bin:/bin:/usr/sbin:/sbin`, not the PATH the user's
//! terminal has. Tools installed by Homebrew, Nix, Cargo or into `~/.local/bin`
//! are then reported missing, and `git` resolves to the system copy instead of
//! the user's own.
//!
//! The login shell knows the real list, so [`login_shell_path`] asks it once at
//! startup and merges the answer into the PATH the process already has. Keeping
//! both matters: an entry only the launcher set survives, and a shell that fails
//! or hangs costs the app nothing but the wait.
//!
//! Windows is not covered: a desktop shortcut there inherits the user's
//! environment, so there is nothing to recover.

use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Marks the start and the end of the PATH in the shell's output.
///
/// An interactive shell prints its own greeting, prompts and escape codes
/// around whatever the command prints, so only the text between the markers can
/// be trusted.
const BEGIN_MARKER: &str = "__STRATA_PATH_BEGIN__";
const END_MARKER: &str = "__STRATA_PATH_END__";

/// How long the login shell may take. Its rc files can do anything, including a
/// network lookup, and the window must not wait for that.
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// The shell to ask when `$SHELL` is unset.
#[cfg(target_os = "macos")]
const DEFAULT_SHELL: &str = "/bin/zsh";
#[cfg(not(target_os = "macos"))]
const DEFAULT_SHELL: &str = "/bin/sh";

/// The PATH the process should use: the login shell's entries first, then the
/// entries it was started with.
///
/// Never panics and never yields an empty PATH, so a shell that cannot be run
/// degrades to the launcher's PATH plus the usual install directories.
pub fn login_shell_path() -> OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let shell = login_shell();

    if let Some(shell_path) = query_path(&shell, QUERY_TIMEOUT) {
        let count = std::env::split_paths(&shell_path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .count();
        let merged = merge_paths(&shell_path, &current);
        log::info!(
            "PATH: took {count} entries from {} and kept the ones the process started with",
            shell.display()
        );
        log::debug!(
            "PATH after the login-shell merge: {}",
            merged.to_string_lossy()
        );
        return merged;
    }

    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let user = std::env::var_os("USER")
        .filter(|user| !user.is_empty())
        .map(|user| user.to_string_lossy().into_owned());
    let extras = existing_dirs(fallback_dirs(home.as_deref(), user.as_deref()));
    let extras = std::env::join_paths(extras).unwrap_or_default();
    let merged = merge_paths(&current, &extras);
    log::info!(
        "PATH: {} did not report one; added the usual package-manager directories instead",
        shell.display()
    );
    log::debug!(
        "PATH after the fallback merge: {}",
        merged.to_string_lossy()
    );
    merged
}

/// `$SHELL`, or the platform's default when it is unset or blank.
fn login_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SHELL))
}

/// Runs `shell` as a login shell and asks it for its PATH.
///
/// Returns `None` when the shell cannot be started, does not print both
/// markers, or takes longer than `timeout`; a shell killed at the deadline is
/// reaped so no process is left behind.
fn query_path(shell: &Path, timeout: Duration) -> Option<OsString> {
    // `printenv` rather than `$PATH` so the answer does not depend on the
    // shell's own variable syntax.
    let script = format!("printf '{BEGIN_MARKER}'; printenv PATH; printf '{END_MARKER}'");
    query_script(shell, &script, timeout)
}

fn query_script(shell: &Path, script: &str, timeout: Duration) -> Option<OsString> {
    let mut child = match Command::new(shell)
        .args(["-l", "-i", "-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            log::debug!("cannot run the login shell {}: {error}", shell.display());
            return None;
        }
    };

    // Read on a thread of its own: a chatty rc file that fills the pipe would
    // otherwise block the shell while this side waits for it to exit.
    let mut pipe = child.stdout.take()?;
    let (sender, output_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = pipe.read_to_end(&mut output);
        let _ = sender.send(output);
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                log::info!(
                    "{} did not report a PATH within {} ms",
                    shell.display(),
                    timeout.as_millis()
                );
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                log::debug!("cannot wait for {}: {error}", shell.display());
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // An interactive rc file may start a daemon that inherits stdout and keeps
    // the pipe open long after the shell exits, so the read is only waited on
    // until the deadline; a reader still blocked then is left behind rather
    // than holding up the window.
    let output = output_rx
        .recv_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(200)),
        )
        .ok()?;
    let status = status?;
    if !status.success() {
        log::debug!(
            "{} exited with {status}; using its output anyway",
            shell.display()
        );
    }

    let text = String::from_utf8_lossy(&output);
    parse_marked_path(&text).map(OsString::from)
}

/// The text between the two markers, trimmed, or `None` when either marker is
/// missing.
fn parse_marked_path(output: &str) -> Option<&str> {
    let start = output.find(BEGIN_MARKER)? + BEGIN_MARKER.len();
    let rest = &output[start..];
    let end = rest.find(END_MARKER)?;
    Some(rest[..end].trim())
}

/// `first`'s entries followed by `second`'s, without repeats.
fn merge_paths(first: &OsStr, second: &OsStr) -> OsString {
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in std::env::split_paths(first).chain(std::env::split_paths(second)) {
        // An empty entry means "the current directory", which is not something
        // the app should hand to git hooks by accident.
        if entry.as_os_str().is_empty() {
            continue;
        }
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    // Only an entry containing the separator could fail here, and `split_paths`
    // never produces one; keeping `second` beats returning an empty PATH.
    std::env::join_paths(entries).unwrap_or_else(|_| second.to_os_string())
}

/// The directories a login shell normally adds, in the order they are offered.
///
/// `home` is `None` when the process has no HOME, and `user` `None` when it has
/// no USER; the per-user and per-home entries are left out then.
pub fn fallback_dirs(home: Option<&Path>, user: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = home {
        dirs.push(home.join(".nix-profile/bin"));
    }
    if let Some(user) = user {
        dirs.push(PathBuf::from(format!("/etc/profiles/per-user/{user}/bin")));
    }
    dirs.push(PathBuf::from("/run/current-system/sw/bin"));
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs
}

/// Keeps the entries that name a directory, so a PATH never gains a path that
/// does not exist on this machine.
fn existing_dirs(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    dirs.into_iter().filter(|dir| dir.is_dir()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_ignores_the_banner_around_the_markers() {
        let noisy = format!(
            "\u{1b}[1mWelcome to the shell\u{1b}[0m\r\n{BEGIN_MARKER}/opt/homebrew/bin:/usr/bin{END_MARKER}\r\nprompt % \u{1b}]0;title\u{7}"
        );
        assert_eq!(
            parse_marked_path(&noisy),
            Some("/opt/homebrew/bin:/usr/bin")
        );
    }

    #[test]
    fn parsing_needs_both_markers_in_order() {
        assert_eq!(parse_marked_path(""), None);
        assert_eq!(parse_marked_path("just a banner"), None);
        assert_eq!(parse_marked_path(BEGIN_MARKER), None);
        assert_eq!(parse_marked_path(END_MARKER), None);
        assert_eq!(
            parse_marked_path(&format!("{END_MARKER}/usr/bin{BEGIN_MARKER}")),
            None
        );
        assert_eq!(
            parse_marked_path(&format!("{BEGIN_MARKER}{END_MARKER}")),
            Some("")
        );
    }

    #[test]
    fn merging_keeps_the_shell_order_and_drops_repeats() {
        let shell = OsStr::new("/opt/homebrew/bin:/usr/bin:/bin");
        let existing = OsStr::new("/usr/bin::/bin:/usr/sbin:/opt/homebrew/bin:/sbin");
        assert_eq!(
            std::env::split_paths(&merge_paths(shell, existing)).collect::<Vec<_>>(),
            [
                "/opt/homebrew/bin",
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn merging_an_empty_login_path_keeps_the_launch_path() {
        let merged = merge_paths(OsStr::new(""), OsStr::new("/usr/bin:/bin"));
        assert_eq!(
            std::env::split_paths(&merged).collect::<Vec<_>>(),
            ["/usr/bin", "/bin"].map(PathBuf::from)
        );
    }

    #[test]
    fn fallback_directories_are_the_usual_install_locations_in_order() {
        let home = Path::new("/home/me");
        assert_eq!(
            fallback_dirs(Some(home), Some("me")),
            [
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
                home.join(".nix-profile/bin"),
                PathBuf::from("/etc/profiles/per-user/me/bin"),
                PathBuf::from("/run/current-system/sw/bin"),
                home.join(".local/bin"),
                home.join(".cargo/bin"),
            ]
        );
        assert_eq!(
            fallback_dirs(None, None),
            [
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/run/current-system/sw/bin"),
            ]
        );
    }

    #[test]
    fn only_the_fallback_directories_that_exist_are_kept() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path();
        std::fs::create_dir_all(home.join(".local/bin")).expect("create ~/.local/bin");

        let kept = existing_dirs(fallback_dirs(Some(home), None));
        assert!(kept.contains(&home.join(".local/bin")));
        assert!(!kept.contains(&home.join(".nix-profile/bin")));
        assert!(!kept.contains(&home.join(".cargo/bin")));
    }

    #[test]
    fn a_real_shell_answers_between_the_markers() {
        let path = query_path(Path::new("/bin/sh"), Duration::from_secs(5))
            .expect("/bin/sh prints its PATH");
        assert!(!path.is_empty());
        assert!(path.to_string_lossy().split(':').any(|dir| !dir.is_empty()));
    }

    #[test]
    fn a_shell_that_never_answers_is_killed_at_the_deadline() {
        let started = Instant::now();
        let answer = query_script(Path::new("/bin/sh"), "sleep 30", Duration::from_millis(200));
        assert_eq!(answer, None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the query returned only after the sleep would have finished"
        );
    }
}
