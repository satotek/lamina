//! A credential git asks for on the helper host is forwarded to the app.
//!
//! The helper has to be the spawned binary. An in-process `serve` reports the
//! test harness as `current_exe`, so git would not start this program's
//! askpass mode. No SSH: the child speaks the protocol on its stdio.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lamina_core::session::{Operation, OperationId, RepositorySession, SessionEvent};
use lamina_core::test_support::TestRepo;
use lamina_remote_client::{RemoteSession, RemoteTarget};

const TIMEOUT: Duration = Duration::from_secs(30);

/// A local HTTP server that answers 401 until the request carries `expected`.
///
/// `expected` is the `Authorization` value. It is compared and discarded, never
/// written to a log or an error string.
struct AuthWall {
    url: String,
    matched: Arc<AtomicBool>,
    any_authorization: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AuthWall {
    fn start(expected: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the credential wall");
        listener
            .set_nonblocking(true)
            .expect("credential wall is nonblocking");
        let port = listener.local_addr().expect("credential wall port").port();
        let matched = Arc::new(AtomicBool::new(false));
        let any_authorization = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let matched = Arc::clone(&matched);
            let any_authorization = Arc::clone(&any_authorization);
            let shutdown = Arc::clone(&shutdown);
            move || serve_wall(listener, &expected, &matched, &any_authorization, &shutdown)
        });
        Self {
            url: format!("http://127.0.0.1:{port}/repo.git"),
            matched,
            any_authorization,
            shutdown,
            thread: Some(thread),
        }
    }
}

impl Drop for AuthWall {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_wall(
    listener: TcpListener,
    expected: &str,
    matched: &AtomicBool,
    any_authorization: &AtomicBool,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                let _ = answer_once(&mut stream, expected, matched, any_authorization);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

fn answer_once(
    stream: &mut TcpStream,
    expected: &str,
    matched: &AtomicBool,
    any_authorization: &AtomicBool,
) -> std::io::Result<()> {
    let headers = read_headers(stream)?;
    if headers.is_empty() {
        return Ok(());
    }
    let response = if authorization_matches(&headers, expected) {
        matched.store(true, Ordering::SeqCst);
        OK_RESPONSE
    } else {
        if has_authorization(&headers) {
            any_authorization.store(true, Ordering::SeqCst);
        }
        DENY_RESPONSE
    };
    stream.write_all(response)
}

fn read_headers(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < 16 * 1024 {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn authorization_matches(headers: &str, expected: &str) -> bool {
    headers.lines().any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case("authorization") && value.trim() == expected
    })
}

fn has_authorization(headers: &str) -> bool {
    headers.lines().any(|line| {
        line.split_once(':')
            .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
    })
}

const DENY_RESPONSE: &[u8] = b"\
HTTP/1.1 401 Unauthorized\r\n\
WWW-Authenticate: Basic realm=\"lamina\"\r\n\
Content-Length: 0\r\n\
Connection: close\r\n\
\r\n";

/// Empty 200: git has authenticated and then fails the fetch because this is
/// not a repository. The test only needs to have seen the credential.
const OK_RESPONSE: &[u8] = b"\
HTTP/1.1 200 OK\r\n\
Content-Length: 0\r\n\
Connection: close\r\n\
\r\n";

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let (chunks, rest) = input.as_chunks::<3>();
    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
    }
    match rest {
        [a] => {
            let n = u32::from(*a) << 16;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        [a, b] => {
            let n = (u32::from(*a) << 16) | (u32::from(*b) << 8);
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// Kills a helper that a failing assertion would otherwise leave running.
struct Reap(Option<Child>);

impl Reap {
    fn finish(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            let child = self.0.as_mut().expect("helper");
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    break child.wait().expect("wait for killed helper");
                }
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(error) => panic!("wait for helper: {error}"),
            }
        };
        self.0.take();
        status
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn contains_secret(text: &str, secret: &str) -> bool {
    text.contains(secret)
}

#[test]
fn a_fetch_forwards_the_credential_prompt_without_logging_it() {
    // A fixture, not a real credential. Assertion text must not interpolate it:
    // a failure message would then store the value the test exists to protect.
    let secret = "lamina-remote-askpass-fixture";
    let expected = format!(
        "Basic {}",
        base64_encode(format!("alice:{secret}").as_bytes())
    );
    let wall = AuthWall::start(expected);

    let repo = TestRepo::new();
    repo.git(&["config", "user.name", "Lamina Test"]);
    repo.git(&["config", "user.email", "lamina@example.invalid"]);
    repo.write("README.md", "one\n");
    repo.commit("initial");
    // An empty helper resets the list, including one from the machine's
    // system config. Without that, the helper answers before askpass and the
    // prompt never reaches the app. The spawned process also gets an empty
    // global config: `Session` does not inherit the fixture's isolation, and a
    // keychain helper would open a dialog instead of failing the test.
    repo.git(&["config", "--local", "credential.helper", ""]);
    let isolated = repo.path().with_file_name("askpass-gitconfig");
    std::fs::write(&isolated, "").expect("empty gitconfig");

    let mut child = Command::new(env!("CARGO_BIN_EXE_lamina-remote"))
        .arg("--stdio")
        .arg("--root")
        .arg(repo.path())
        .env("GIT_CONFIG_GLOBAL", &isolated)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("LAMINA_ASKPASS_ENDPOINT")
        .env_remove("LAMINA_ASKPASS_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lamina-remote");
    let stderr = child.stderr.take().expect("helper stderr");
    let stderr_thread = thread::spawn(move || {
        let mut stderr = stderr;
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let mut helper = Reap(Some(child));
    let stdin = helper
        .0
        .as_mut()
        .expect("helper")
        .stdin
        .take()
        .expect("helper stdin");
    let stdout = helper
        .0
        .as_mut()
        .expect("helper")
        .stdout
        .take()
        .expect("helper stdout");

    let (tx, rx) = mpsc::channel();
    let remote =
        RemoteSession::connect(stdout, stdin, RemoteTarget::new("askpass"), move |event| {
            let _ = tx.send(event);
        })
        .expect("connect to the spawned helper");

    let deadline = Instant::now() + TIMEOUT;
    while remote.snapshot().head.is_none() {
        if Instant::now() >= deadline {
            panic!("snapshot did not arrive");
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("event channel closed"),
        }
    }

    let fetch = remote.run(Operation::Fetch {
        remote: Some(wall.url.clone()),
        prune: false,
    });
    let error = drive_fetch(&remote, &rx, fetch, secret);

    // Drop the session before reaping so stdin EOF shuts the helper down
    // instead of the guard killing it mid-protocol.
    drop(remote);
    let status = helper.finish();
    let stderr_bytes = stderr_thread.join().expect("stderr thread");
    assert!(
        status.success(),
        "helper exited with {status} and no credential in that status"
    );
    assert!(
        !contains_secret(&String::from_utf8_lossy(&stderr_bytes), secret),
        "helper stderr included the credential"
    );
    assert!(
        error
            .as_deref()
            .is_none_or(|text| !contains_secret(text, secret)),
        "operation error included the credential"
    );
    let failed = error.is_some();
    assert!(failed, "an empty http response is not a repository");
    if !wall.matched.load(Ordering::SeqCst) {
        assert!(
            !wall.any_authorization.load(Ordering::SeqCst),
            "git presented a credential other than the one the app answered"
        );
        panic!("git did not present the credential the app answered");
    }

    drop(wall);
}

/// Answers credential prompts until `id` finishes.
///
/// Returns the operation error. The secret is only compared, never formatted
/// into a panic message.
fn drive_fetch(
    remote: &RemoteSession,
    events: &Receiver<SessionEvent>,
    id: OperationId,
    secret: &str,
) -> Option<String> {
    let deadline = Instant::now() + TIMEOUT;
    let mut prompts = 0u32;
    let mut saw_username = false;
    let mut saw_secret = false;
    loop {
        if Instant::now() >= deadline {
            panic!("fetch did not finish");
        }
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(SessionEvent::AskpassPrompt {
                id: prompt_id,
                prompt,
                secret: is_secret,
            }) if prompt_id == id => {
                prompts += 1;
                if prompts > 8 {
                    panic!("too many credential prompts");
                }
                assert!(
                    !contains_secret(&prompt, secret),
                    "askpass prompt included the credential"
                );
                let answer = if is_secret {
                    saw_secret = true;
                    secret.to_owned()
                } else {
                    saw_username = true;
                    "alice".to_owned()
                };
                assert!(
                    remote.answer_askpass(prompt_id, Some(answer)),
                    "the prompt was already gone"
                );
            }
            Ok(SessionEvent::OperationProgress {
                id: progress_id,
                text,
                ..
            }) if progress_id == id => {
                assert!(
                    !contains_secret(&text, secret),
                    "progress text included the credential"
                );
            }
            Ok(SessionEvent::OperationFinished {
                id: finished_id,
                error,
            }) if finished_id == id => {
                assert!(saw_username, "git did not ask for a username");
                assert!(saw_secret, "git did not ask for a password");
                return error;
            }
            Ok(SessionEvent::LoadFailed { error }) => {
                assert!(
                    !contains_secret(&error, secret),
                    "load failure included the credential"
                );
                panic!("load failed: {error}");
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("event channel closed"),
        }
    }
}
