//! Mutations over the in-process helper. No SSH: the duplex pipe is the
//! connection, and the helper runs the same session the local app uses.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use strata_core::HeadInfo;
use strata_core::error::OPERATION_CANCELLED;
use strata_core::session::{Operation, OperationId, RepositorySession, SessionEvent};
use strata_core::test_support::TestRepo;
use strata_remote_client::{RemoteSession, RemoteTarget, UNSUPPORTED};

const TIMEOUT: Duration = Duration::from_secs(30);

fn identity(repo: &TestRepo) {
    repo.git(&["config", "user.name", "Strata Test"]);
    repo.git(&["config", "user.email", "strata@example.invalid"]);
    repo.git(&["config", "commit.gpgsign", "false"]);
    repo.git(&["config", "tag.gpgsign", "false"]);
}

fn serve(
    repo: &TestRepo,
) -> (
    RemoteSession,
    Receiver<SessionEvent>,
    std::thread::JoinHandle<()>,
) {
    let (client_read, client_write, server_read, server_write) = duplex();
    let root = repo.path().to_path_buf();
    let server = std::thread::spawn(move || {
        strata_remote::serve(server_read, server_write, root).expect("serve");
    });
    let (tx, rx) = mpsc::channel();
    let remote = RemoteSession::connect(
        client_read,
        client_write,
        RemoteTarget::new("operations"),
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("connect");
    let _ = wait_until(&remote, &rx, |snapshot| {
        snapshot.commits.complete && snapshot.head.is_some()
    });
    (remote, rx, server)
}

fn wait_until(
    session: &RemoteSession,
    events: &Receiver<SessionEvent>,
    mut predicate: impl FnMut(&strata_core::session::RepoSnapshot) -> bool,
) -> std::sync::Arc<strata_core::session::RepoSnapshot> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let snapshot = session.snapshot();
        if predicate(&snapshot) {
            return snapshot;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("snapshot predicate was not met: {error}"),
        }
    }
}

fn wait_finished(
    events: &Receiver<SessionEvent>,
    id: OperationId,
) -> (Option<String>, Vec<String>) {
    let deadline = Instant::now() + TIMEOUT;
    let mut progress = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::OperationProgress {
                id: event_id, text, ..
            }) if event_id == id => progress.push(text),
            Ok(SessionEvent::OperationFinished {
                id: event_id,
                error,
            }) if event_id == id => return (error, progress),
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("operation {id:?} did not finish: {error}"),
        }
    }
}

fn head_target(snapshot: &strata_core::session::RepoSnapshot) -> strata_core::Oid {
    match snapshot.head.as_ref().expect("head") {
        HeadInfo::Branch {
            target: Some(id), ..
        }
        | HeadInfo::Detached { target: id } => *id,
        HeadInfo::Branch { target: None, .. } => panic!("unborn"),
    }
}

#[test]
fn a_commit_branch_stash_and_reset_round_trip() {
    let repo = TestRepo::new();
    identity(&repo);
    repo.write("README.md", "one\n");
    repo.commit("initial");
    let (remote, events, server) = serve(&repo);

    repo.write("README.md", "two\n");
    let stage = remote.run(Operation::StagePaths(vec!["README.md".into()]));
    let (error, _) = wait_finished(&events, stage);
    assert!(error.is_none(), "{error:?}");

    let commit = remote.run(Operation::Commit {
        message: "second".into(),
        amend: false,
    });
    let (error, _) = wait_finished(&events, commit);
    assert!(error.is_none(), "{error:?}");
    let snapshot = wait_until(&remote, &events, |snapshot| {
        snapshot.busy.is_none() && head_target(snapshot) == repo.head()
    });
    assert_eq!(head_target(&snapshot), repo.head());

    let branch = remote.run(Operation::CreateBranch {
        name: "feature".into(),
        start_point: None,
        checkout: true,
    });
    let (error, _) = wait_finished(&events, branch);
    assert!(error.is_none(), "{error:?}");
    wait_until(&remote, &events, |snapshot| {
        matches!(
            snapshot.head.as_ref(),
            Some(HeadInfo::Branch { short_name, .. }) if short_name == "feature"
        )
    });

    let base = repo.rev("main");
    let tip = repo.head();
    let between = remote.reader().commits_between(base, tip).expect("range");
    assert!(between.is_empty(), "feature and main are the same commit");
    assert!(
        remote.reader().is_ancestor(base, tip).expect("ancestor"),
        "main is an ancestor of feature"
    );
    assert!(
        remote
            .reader()
            .merge_message()
            .expect("merge message")
            .is_none(),
        "nothing is merging"
    );

    repo.write("notes.txt", "wip\n");
    let stash = remote.run(Operation::StashPush {
        message: Some("notes".into()),
        include_untracked: true,
    });
    let (error, _) = wait_finished(&events, stash);
    assert!(error.is_none(), "{error:?}");
    wait_until(&remote, &events, |snapshot| snapshot.stashes.len() == 1);
    let drop_stash = remote.run(Operation::StashDrop { index: 0 });
    let (error, _) = wait_finished(&events, drop_stash);
    assert!(error.is_none(), "{error:?}");
    wait_until(&remote, &events, |snapshot| snapshot.stashes.is_empty());

    let reset = remote.run(Operation::Reset {
        target: base,
        mode: strata_core::session::ResetMode::Mixed,
    });
    let (error, _) = wait_finished(&events, reset);
    assert!(error.is_none(), "{error:?}");
    wait_until(&remote, &events, |snapshot| head_target(snapshot) == base);
    assert_eq!(repo.head(), base);

    let unsupported = remote.run(Operation::CreateTag {
        name: "v1".into(),
        target: base,
        message: None,
    });
    let (error, _) = wait_finished(&events, unsupported);
    assert_eq!(error.as_deref(), Some(UNSUPPORTED));
    assert!(
        repo.git_output(&["rev-parse", "--verify", "refs/tags/v1"])
            .status
            .code()
            != Some(0)
    );

    drop(remote);
    server.join().expect("server thread");
}

#[test]
fn a_fetch_reports_progress_and_finishes() {
    let origin = TestRepo::new();
    identity(&origin);
    origin.write("lib.rs", "pub fn origin() {}\n");
    origin.commit("origin");

    let repo = TestRepo::new();
    identity(&repo);
    repo.write("README.md", "local\n");
    repo.commit("local");
    repo.git(&[
        "remote",
        "add",
        "origin",
        &format!("file://{}", origin.path().display()),
    ]);

    let (remote, events, server) = serve(&repo);
    let fetch = remote.run(Operation::Fetch {
        remote: Some("origin".into()),
        prune: false,
    });
    let (error, progress) = wait_finished(&events, fetch);
    assert!(error.is_none(), "{error:?}");
    assert!(
        progress.iter().any(|line| !line.is_empty()),
        "fetch should stream at least one progress line, got {progress:?}"
    );
    wait_until(&remote, &events, |snapshot| {
        snapshot
            .refs
            .iter()
            .any(|reference| reference.short_name == "origin/main")
    });

    drop(remote);
    server.join().expect("server thread");
}

#[cfg(unix)]
#[test]
fn cancelling_a_commit_kills_the_git_child() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new();
    identity(&repo);
    repo.write("README.md", "one\n");
    let first = repo.commit("initial");

    let hooks = repo.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let marker = repo.path().join(".git/hook-started");
    let script = format!(
        "#!/bin/sh\ntouch {}\nsleep 30\n",
        sh_quote(&marker.display().to_string())
    );
    let hook = hooks.join("pre-commit");
    std::fs::write(&hook, script).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    repo.git(&["config", "core.hooksPath", &hooks.display().to_string()]);

    repo.write("README.md", "two\n");
    let (remote, events, server) = serve(&repo);
    let id = remote.run(Operation::Commit {
        message: "should not land".into(),
        amend: false,
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        if Instant::now() >= deadline {
            panic!("the pre-commit hook did not start");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    remote.cancel_operation(id);
    let (error, _) = wait_finished(&events, id);
    assert_eq!(error.as_deref(), Some(OPERATION_CANCELLED));
    assert_eq!(repo.head(), first, "the cancelled commit must not land");

    drop(remote);
    server.join().expect("server thread");
}

#[cfg(unix)]
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct DuplexRead {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    offset: usize,
}

impl Read for DuplexRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.offset == self.pending.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.pending = chunk;
                    self.offset = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let available = &self.pending[self.offset..];
        let take = available.len().min(buf.len());
        buf[..take].copy_from_slice(&available[..take]);
        self.offset += take;
        Ok(take)
    }
}

struct DuplexWrite {
    tx: mpsc::Sender<Vec<u8>>,
}

impl Write for DuplexWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.tx.send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn duplex() -> (DuplexRead, DuplexWrite, DuplexRead, DuplexWrite) {
    let (client_tx, server_rx) = mpsc::channel();
    let (server_tx, client_rx) = mpsc::channel();
    (
        DuplexRead {
            rx: client_rx,
            pending: Vec::new(),
            offset: 0,
        },
        DuplexWrite { tx: client_tx },
        DuplexRead {
            rx: server_rx,
            pending: Vec::new(),
            offset: 0,
        },
        DuplexWrite { tx: server_tx },
    )
}
