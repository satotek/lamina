//! The askpass bridge end to end: real git starts the Lamina executable as its
//! helper, the helper reaches the app's channel, and the answer comes back on
//! stdout.
//!
//! `git credential fill` is the smallest real git command that asks for a
//! username and a password, so these tests cover git's side of the bridge
//! without an HTTP server.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lamina_core::askpass::{Answer, AskpassHost, Prompt};
use tempfile::TempDir;

/// A bridge this stuck is a bug; the test fails rather than waits on git.
const TIMEOUT: Duration = Duration::from_secs(30);

/// An empty git config, so no developer credential helper answers instead.
fn empty_config(directory: &Path) -> PathBuf {
    let path = directory.join("gitconfig");
    std::fs::write(&path, "").unwrap();
    path
}

fn run_git(host: &AskpassHost, config: &Path, stdin: &str) -> Output {
    let mut command = Command::new("git");
    command
        .args(["credential", "fill"])
        .env("GIT_ASKPASS", env!("CARGO_BIN_EXE_lamina"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", config)
        .env("GIT_CONFIG_SYSTEM", config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in host.env() {
        command.env(key, value);
    }

    let mut child = command.spawn().expect("git runs");
    child
        .stdin
        .take()
        .expect("git stdin")
        .write_all(stdin.as_bytes())
        .expect("write git stdin");

    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().expect("waiting for git") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("git did not finish: the askpass bridge is stuck");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    child.wait_with_output().expect("git output")
}

#[test]
fn git_asks_the_app_for_a_username_and_password() {
    let temp = TempDir::new().unwrap();
    let config = empty_config(temp.path());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = AskpassHost::start({
        let seen = Arc::clone(&seen);
        move |prompt: Prompt| {
            seen.lock().unwrap().push(prompt.clone());
            Some(Answer::new(if prompt.secret { "s3cret" } else { "alice" }))
        }
    })
    .expect("askpass host");

    let output = run_git(&host, &config, "url=https://example.com/repo.git\n\n");
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("username=alice"), "{text}");
    assert!(text.contains("password=s3cret"), "{text}");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "git asks for both fields: {seen:?}");
    assert_eq!(seen[0].text, "Username for 'https://example.com': ");
    assert!(!seen[0].secret, "a username must stay readable");
    assert_eq!(
        seen[1].text, "Password for 'https://alice@example.com': ",
        "the password prompt names the user the app supplied"
    );
    assert!(seen[1].secret, "a password must be masked");
}

#[test]
fn cancelling_the_prompt_fails_git_instead_of_hanging() {
    let temp = TempDir::new().unwrap();
    let config = empty_config(temp.path());
    let host = AskpassHost::start(|_| None).expect("askpass host");

    let output = run_git(&host, &config, "url=https://example.com/repo.git\n\n");

    assert!(
        !output.status.success(),
        "a cancelled prompt fails the command"
    );
    let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    assert!(
        error.contains("could not read username") || error.contains("terminal prompts disabled"),
        "git's authentication error reaches the caller: {error}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "nothing that looks like a credential is printed on cancel"
    );
}
