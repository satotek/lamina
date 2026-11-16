//! Mutations over the in-process helper. No SSH: the duplex pipe is the
//! connection, and the helper runs the same session the local app uses.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use strata_core::HeadInfo;
use strata_core::error::OPERATION_CANCELLED;
use strata_core::lfs::LfsPointer;
use strata_core::merge::{ConflictStyle, ContentHash};
use strata_core::session::{
    ConflictSide, Operation, OperationId, RepositorySession, Session, SessionEvent,
};
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
fn reload_sees_a_file_written_outside_the_helper() {
    let repo = TestRepo::new();
    identity(&repo);
    repo.write("README.md", "one\n");
    repo.commit("initial");
    let (remote, events, server) = serve(&repo);

    // The helper only republishes what it is asked for. A file created after
    // connect stays invisible until reload, which is what the app's refresh
    // sends.
    repo.write("extra.txt", "outside\n");
    remote.reload();
    wait_until(&remote, &events, |snapshot| {
        snapshot
            .status
            .unstaged
            .iter()
            .any(|change| change.path == "extra.txt")
    });

    drop(remote);
    server.join().expect("server thread");
}

#[test]
fn checkout_commit_detaches_at_an_older_commit() {
    let repo = TestRepo::new();
    identity(&repo);
    repo.write("README.md", "one\n");
    let first = repo.commit("initial");
    repo.write("README.md", "two\n");
    let second = repo.commit("second");
    let (remote, events, server) = serve(&repo);

    // `Checkout` ends with `--`, so an object id would be a pathspec. This
    // operation is `git checkout --detach` and must leave `main` where it is.
    let id = remote.run(Operation::CheckoutCommit { commit: first });
    let (error, _) = wait_finished(&events, id);
    assert!(error.is_none(), "{error:?}");
    wait_until(&remote, &events, |snapshot| {
        matches!(
            snapshot.head.as_ref(),
            Some(HeadInfo::Detached { target }) if *target == first
        )
    });
    assert_eq!(repo.head(), first);
    assert_eq!(repo.rev("main"), second);

    drop(remote);
    server.join().expect("server thread");
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

fn conflict_repo() -> TestRepo {
    let repo = TestRepo::new();
    identity(&repo);
    repo.write("conflict.txt", "base\n");
    repo.commit("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("conflict.txt", "theirs\n");
    repo.commit("theirs");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("conflict.txt", "ours\n");
    repo.commit("ours");
    let merge = repo.git_output(&["merge", "--no-edit", "side"]);
    assert!(
        !merge.status.success(),
        "the fixture merge should stop on conflict.txt: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    repo
}

/// Index, status and worktree bytes, so a remote resolution can be compared
/// with the same resolution run by a local session.
fn checkout_state(repo: &TestRepo) -> String {
    let file = std::fs::read(repo.path().join("conflict.txt")).unwrap_or_default();
    format!(
        "index:\n{}\nunmerged:\n{}\nstatus:\n{}\nfile:{file:?}",
        repo.git(&["ls-files", "-s"]),
        repo.git(&["ls-files", "-u"]),
        repo.git(&["status", "--porcelain"]),
    )
}

fn run_local(repo: &TestRepo, operation: Operation) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    let session = Session::open(repo.path(), move |event| {
        let _ = tx.send(event);
    })
    .expect("local session");
    let id = session.run(operation);
    let (error, _) = wait_finished(&rx, id);
    drop(session);
    error
}

fn run_remote(repo: &TestRepo, operation: Operation) -> Option<String> {
    let (remote, events, server) = serve(repo);
    let id = remote.run(operation);
    let (error, _) = wait_finished(&events, id);
    drop(remote);
    server.join().expect("server thread");
    error
}

fn resolved(content: &str) -> Operation {
    Operation::ResolveWithContent {
        path: "conflict.txt".into(),
        content: content.as_bytes().to_vec(),
        expected: None,
    }
}

#[test]
fn resolving_a_conflict_matches_a_local_session() {
    let local_repo = conflict_repo();
    let remote_repo = conflict_repo();
    assert_eq!(run_local(&local_repo, resolved("resolved\n")), None);
    assert_eq!(run_remote(&remote_repo, resolved("resolved\n")), None);
    assert_eq!(
        checkout_state(&local_repo),
        checkout_state(&remote_repo),
        "the resolved index and worktree must match"
    );
}

#[test]
fn taking_ours_or_theirs_matches_a_local_session() {
    for side in [ConflictSide::Ours, ConflictSide::Theirs] {
        let operation = Operation::ResolveConflict {
            path: "conflict.txt".into(),
            side,
        };
        let local_repo = conflict_repo();
        let remote_repo = conflict_repo();
        assert_eq!(run_local(&local_repo, operation.clone()), None, "{side:?}");
        assert_eq!(
            run_remote(&remote_repo, operation),
            None,
            "{side:?} over the helper"
        );
        assert_eq!(
            checkout_state(&local_repo),
            checkout_state(&remote_repo),
            "{side:?}"
        );
    }
}

#[test]
fn conflict_reads_match_a_local_repository() {
    let repo = conflict_repo();
    repo.git(&["config", "merge.conflictStyle", "diff3"]);
    let local = repo.open();
    let (remote, _events, server) = serve(&repo);
    let reader = remote.reader();
    assert_eq!(
        local.conflict_sides("conflict.txt").expect("local sides"),
        reader.conflict_sides("conflict.txt").expect("remote sides"),
    );
    assert_eq!(reader.conflict_style(), ConflictStyle::Diff3);
    assert_eq!(local.conflict_style(), reader.conflict_style());
    assert_eq!(
        local
            .file_in_worktree("conflict.txt")
            .expect("local worktree"),
        reader
            .file_in_worktree("conflict.txt")
            .expect("remote worktree"),
    );
    let escaped = reader
        .file_in_worktree("../outside.txt")
        .expect_err("a worktree read must not leave the repository");
    assert!(
        escaped.to_string().contains("worktree"),
        "unexpected error: {escaped}"
    );
    drop(reader);
    drop(remote);
    server.join().expect("server thread");
}

#[test]
fn resolve_with_content_refuses_a_file_that_changed() {
    let repo = conflict_repo();
    let path = repo.path().join("conflict.txt");
    let loaded = std::fs::read(&path).expect("conflicted file");
    let expected = ContentHash::of(&loaded);
    std::fs::write(&path, b"sneaky\n").expect("change the file underneath");

    let (remote, events, server) = serve(&repo);
    let id = remote.run(Operation::ResolveWithContent {
        path: "conflict.txt".into(),
        content: b"resolved\n".to_vec(),
        expected: Some(expected),
    });
    let (error, _) = wait_finished(&events, id);
    let error = error.expect("a changed file must be refused");
    assert!(
        error.contains("changed on disk"),
        "unexpected error: {error}"
    );
    assert_eq!(std::fs::read(&path).expect("file"), b"sneaky\n");
    assert!(
        !repo.git(&["ls-files", "-u"]).trim().is_empty(),
        "the path must stay unmerged"
    );
    drop(remote);
    server.join().expect("server thread");
}

#[test]
fn resolve_with_content_rejects_paths_that_leave_the_worktree() {
    let repo = conflict_repo();
    let git_config = std::fs::read(repo.path().join(".git/config")).expect("config");
    let parent_escape = repo
        .path()
        .parent()
        .expect("worktree parent")
        .join("strata-resolve-escape.txt");
    let absolute = repo.path().join("strata-resolve-absolute.txt");
    let (remote, events, server) = serve(&repo);
    for path in [
        "../strata-resolve-escape.txt".to_owned(),
        absolute.to_string_lossy().into_owned(),
        ".git/config".to_owned(),
    ] {
        let id = remote.run(Operation::ResolveWithContent {
            path: path.clone(),
            content: b"x\n".to_vec(),
            expected: None,
        });
        let (error, _) = wait_finished(&events, id);
        let error = error.unwrap_or_else(|| panic!("{path} must be rejected"));
        assert!(
            error.contains("worktree") || error.contains(".git"),
            "unexpected error for {path}: {error}"
        );
    }
    drop(remote);
    server.join().expect("server thread");
    assert!(!parent_escape.exists(), "a `..` path must not escape");
    assert!(!absolute.exists(), "an absolute path must not be written");
    assert_eq!(
        std::fs::read(repo.path().join(".git/config")).expect("config"),
        git_config
    );
    assert!(
        !repo.git(&["ls-files", "-u"]).trim().is_empty(),
        "a rejected write must not stage the conflict"
    );
}

#[cfg(unix)]
#[test]
fn resolve_with_content_rejects_a_symlink_that_leaves_the_worktree() {
    use std::os::unix::fs::symlink;

    let repo = conflict_repo();
    let outside = repo
        .path()
        .parent()
        .expect("worktree parent")
        .join("strata-symlink-outside.txt");
    std::fs::write(&outside, b"secret\n").expect("outside file");
    symlink(&outside, repo.path().join("linked.txt")).expect("symlink");

    let outside_dir = repo
        .path()
        .parent()
        .expect("worktree parent")
        .join("strata-symlink-dir");
    std::fs::create_dir(&outside_dir).expect("outside dir");
    symlink(&outside_dir, repo.path().join("escape-dir")).expect("dir symlink");

    let (remote, events, server) = serve(&repo);
    let link = remote.run(Operation::ResolveWithContent {
        path: "linked.txt".into(),
        content: b"pwned\n".to_vec(),
        expected: None,
    });
    let (error, _) = wait_finished(&events, link);
    let error = error.expect("a symlink must not be written through");
    assert!(error.contains("symlink"), "unexpected error: {error}");
    assert_eq!(std::fs::read(&outside).expect("outside"), b"secret\n");

    let through_dir = remote.run(Operation::ResolveWithContent {
        path: "escape-dir/file.txt".into(),
        content: b"pwned\n".to_vec(),
        expected: None,
    });
    let (error, _) = wait_finished(&events, through_dir);
    let error = error.expect("a directory symlink must not carry the write out");
    assert!(
        error.contains("worktree") || error.contains("symlink"),
        "unexpected error: {error}"
    );
    assert!(!outside_dir.join("file.txt").exists());
    drop(remote);
    server.join().expect("server thread");
}

#[test]
fn worktree_and_submodule_paths_that_escape_are_rejected() {
    let repo = TestRepo::new();
    identity(&repo);
    repo.write("README.md", "one\n");
    let head = repo.commit("base");
    let before = repo.git(&["worktree", "list", "--porcelain"]);
    let (remote, events, server) = serve(&repo);

    let escape = if cfg!(windows) {
        std::path::PathBuf::from(r"C:\tmp\..\Windows")
    } else {
        std::path::PathBuf::from("/tmp/../etc")
    };
    let control = if cfg!(windows) {
        std::path::PathBuf::from("C:\\temp\\a\nb")
    } else {
        std::path::PathBuf::from("/tmp/a\nb")
    };
    let cases = [
        (
            Operation::WorktreeAdd {
                path: std::path::PathBuf::from("relative-worktree"),
                branch: None,
                new_branch: Some("topic".into()),
                commit: Some(head),
            },
            "absolute",
        ),
        (
            Operation::WorktreeAdd {
                path: escape,
                branch: None,
                new_branch: Some("topic".into()),
                commit: Some(head),
            },
            "..",
        ),
        (
            Operation::WorktreeAdd {
                path: std::path::PathBuf::from(""),
                branch: None,
                new_branch: Some("topic".into()),
                commit: Some(head),
            },
            "empty",
        ),
        (
            Operation::WorktreeAdd {
                path: control.clone(),
                branch: None,
                new_branch: Some("topic".into()),
                commit: Some(head),
            },
            "control",
        ),
        (
            Operation::WorktreeRemove {
                path: std::path::PathBuf::from("relative-worktree"),
                force: false,
            },
            "absolute",
        ),
        (
            Operation::WorktreeRemove {
                path: control,
                force: true,
            },
            "control",
        ),
        (
            Operation::SubmoduleUpdate {
                paths: vec!["..".into()],
                init: false,
            },
            "worktree",
        ),
        (
            Operation::SubmoduleUpdate {
                paths: vec!["../outside".into()],
                init: false,
            },
            "worktree",
        ),
        (
            Operation::SubmoduleUpdate {
                paths: vec![".git/config".into()],
                init: true,
            },
            ".git",
        ),
        (
            Operation::SubmoduleUpdate {
                paths: vec![String::new()],
                init: false,
            },
            "empty",
        ),
        (
            Operation::SubmoduleUpdate {
                paths: vec!["libs/../secret".into()],
                init: false,
            },
            "worktree",
        ),
        (
            Operation::SubmoduleUpdate {
                paths: vec!["libs/a\nb".into()],
                init: false,
            },
            "control",
        ),
    ];
    for (operation, needle) in cases {
        let id = remote.run(operation);
        let (error, _) = wait_finished(&events, id);
        let error = error.expect("the helper must reject the path");
        assert!(error.contains(needle), "expected {needle:?} in {error}");
    }
    drop(remote);
    server.join().expect("server thread");
    assert_eq!(
        repo.git(&["worktree", "list", "--porcelain"]),
        before,
        "a rejected path must not create a worktree"
    );
}

const LFS_HEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A commit whose blob is an LFS pointer, plus the object the pointer names.
///
/// No `git-lfs` binary is required: the pointer is ordinary text, and the
/// object file is placed where the helper derives the path from the id.
fn commit_lfs_pointer(repo: &TestRepo) -> strata_core::Oid {
    let pointer =
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{LFS_HEX}\nsize 6\n");
    repo.write("data.bin", &pointer);
    repo.write(".gitattributes", "*.bin filter=lfs\n");
    let commit = repo.commit("pointer");
    let object = repo
        .path()
        .join(".git")
        .join("lfs")
        .join("objects")
        .join(&LFS_HEX[..2])
        .join(&LFS_HEX[2..4])
        .join(LFS_HEX);
    std::fs::create_dir_all(object.parent().expect("object parent")).expect("lfs dir");
    std::fs::write(object, b"hello\n").expect("lfs object");
    commit
}

#[test]
fn lfs_paths_and_object_ids_that_escape_are_rejected() {
    let repo = TestRepo::new();
    identity(&repo);
    let commit = commit_lfs_pointer(&repo);
    let (remote, events, server) = serve(&repo);
    let reader = remote.reader();

    let info = reader
        .lfs_info_at(commit, "data.bin")
        .expect("lfs info")
        .expect("the blob is a pointer");
    assert_eq!(info.oid, format!("sha256:{LFS_HEX}"));
    assert!(info.present, "the planted object is readable");
    assert!(reader.uses_lfs(), "the attributes file marks *.bin");
    let pointer = LfsPointer {
        oid: info.oid.clone(),
        size: info.size,
    };
    let bytes = reader
        .read_lfs_object(&pointer, 1024)
        .expect("object read")
        .expect("object bytes");
    assert_eq!(bytes, b"hello\n");
    assert!(
        reader
            .read_lfs_object(&pointer, 1)
            .expect("capped read")
            .is_none(),
        "an object larger than the limit is not sent"
    );

    let outside = reader.lfs_info_at(commit, "../secret");
    let outside = outside.expect_err(".. must be refused").to_string();
    assert!(outside.contains("worktree"), "{outside}");
    let git_dir = reader
        .lfs_info_in_index(".git/config")
        .expect_err(".git must be refused")
        .to_string();
    assert!(git_dir.contains(".git"), "{git_dir}");
    let empty = reader
        .lfs_info_in_worktree("")
        .expect_err("an empty path must be refused")
        .to_string();
    assert!(empty.contains("empty"), "{empty}");
    let control = reader
        .read_lfs_object(
            &LfsPointer {
                oid: "sha256:abcd\n".into(),
                size: 1,
            },
            32,
        )
        .expect_err("a control character must be refused")
        .to_string();
    assert!(control.contains("control"), "{control}");
    let traversal = reader
        .read_lfs_object(
            &LfsPointer {
                oid: "../etc/passwd".into(),
                size: 1,
            },
            32,
        )
        .expect_err("a path is not an object id")
        .to_string();
    assert!(traversal.contains("sha256"), "{traversal}");

    let cases = [
        (Operation::LfsTrack { path: "..".into() }, "worktree"),
        (
            Operation::LfsLock {
                path: "../outside".into(),
            },
            "worktree",
        ),
        (
            Operation::LfsUnlock {
                path: ".git/config".into(),
            },
            ".git",
        ),
        (
            Operation::LfsPull {
                path: String::new(),
            },
            "empty",
        ),
        (
            Operation::LfsTrack {
                path: "libs/../secret".into(),
            },
            "worktree",
        ),
        (
            Operation::LfsPull {
                path: "data\n.bin".into(),
            },
            "control",
        ),
    ];
    for (operation, needle) in cases {
        let id = remote.run(operation);
        let (error, _) = wait_finished(&events, id);
        let error = error.expect("the helper must reject the path");
        assert!(error.contains(needle), "expected {needle:?} in {error}");
    }

    // `*` is a normal path component. A pattern must reach git, not the
    // worktree-path check, even when git-lfs itself is missing.
    let tracked = remote.run(Operation::LfsTrack {
        path: "*.bin".into(),
    });
    let (error, _) = wait_finished(&events, tracked);
    if let Some(error) = error {
        assert!(
            !error.contains("worktree") && !error.contains("control"),
            "a pattern was treated as an escape: {error}"
        );
    }
    drop(remote);
    server.join().expect("server thread");
}

#[cfg(unix)]
#[test]
fn a_stdio_helper_resolves_a_conflict_like_a_local_session() {
    let local_repo = conflict_repo();
    let remote_repo = conflict_repo();
    assert_eq!(run_local(&local_repo, resolved("resolved\n")), None);
    let (remote, events, mut child) = spawn_helper(&remote_repo);
    let id = remote.run(resolved("resolved\n"));
    let (error, _) = wait_finished(&events, id);
    assert!(error.is_none(), "{error:?}");
    drop(remote);
    let status = child.wait().expect("wait for helper");
    assert!(status.success(), "helper exited with {status}");
    assert_eq!(checkout_state(&local_repo), checkout_state(&remote_repo));
}

#[cfg(unix)]
fn spawn_helper(repo: &TestRepo) -> (RemoteSession, Receiver<SessionEvent>, std::process::Child) {
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_strata-remote"))
        .arg("--stdio")
        .arg("--root")
        .arg(repo.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn strata-remote");
    let stdin = child.stdin.take().expect("helper stdin");
    let stdout = child.stdout.take().expect("helper stdout");
    let (tx, rx) = mpsc::channel();
    let remote = RemoteSession::connect(stdout, stdin, RemoteTarget::new("stdio"), move |event| {
        let _ = tx.send(event);
    })
    .expect("connect to the spawned helper");
    let _ = wait_until(&remote, &rx, |snapshot| {
        snapshot.commits.complete && snapshot.head.is_some()
    });
    (remote, rx, child)
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

#[test]
fn a_config_write_only_names_a_file_the_helper_lists() {
    use strata_core::git_cli::{
        ConfigChange, ConfigPreview, ConfigStamp, ConfigTarget, ConfigWrite,
    };
    use strata_core::repo::ConfigLayer;

    let repo = TestRepo::new();
    identity(&repo);
    repo.write("README.md", "one\n");
    repo.commit("base");
    let (remote, _events, server) = serve(&repo);
    let access = remote.git_config_access().expect("config access");
    let config = access.load_config().expect("load config");
    let local = config.files.local.clone();

    let write_for = |path: std::path::PathBuf| {
        ConfigWrite::new(
            ConfigTarget::new(path, ConfigLayer::Local),
            "user.name",
            ConfigChange::Set("Mallory".into()),
        )
    };
    assert!(
        access.preview_config(&write_for(local.clone())).is_ok(),
        "the repository's own config file is writable"
    );

    let cases = [
        (
            repo.path().join("README.md"),
            "not one this repository writes",
        ),
        (std::path::PathBuf::from("config"), "absolute"),
        (std::path::PathBuf::from(""), "no target file"),
        (std::path::PathBuf::from("/tmp/a\nb"), "control"),
        (
            std::path::PathBuf::from(format!(
                "{}/../config",
                local.parent().expect("config parent").display()
            )),
            "..",
        ),
    ];
    for (path, needle) in cases {
        let error = access
            .preview_config(&write_for(path))
            .expect_err("the path must be refused")
            .to_string();
        assert!(error.contains(needle), "expected {needle:?} in {error}");
    }

    // The save takes a separate path through `Run`, so it has to refuse an
    // outside file as well. A hand-built preview is enough: the path check
    // runs before the stamp is compared.
    let preview = ConfigPreview::from_displayed(
        write_for(repo.path().join("README.md")),
        "git config".into(),
        String::new(),
        String::new(),
        ConfigStamp {
            len: 0,
            hash: 0,
            modified_secs: None,
            modified_nanos: None,
        },
    );
    let error = access
        .apply_config(&preview)
        .expect_err("the save must refuse the path")
        .to_string();
    assert!(error.contains("config file"), "{error}");
    assert_eq!(
        repo.git(&["config", "--local", "--get", "user.name"])
            .trim(),
        "Strata Test",
        "a refused path leaves the config alone"
    );

    drop(remote);
    server.join().expect("server thread");
}
