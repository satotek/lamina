//! A local `Session` and a `lamina-remote --stdio` helper must leave the same
//! repository after each write this revision adds.
//!
//! The two repositories are twins: the fixture pins the author and the
//! committer date, so the same commit sequence produces the same object ids.
//! Comparing `HEAD` and the worktree then says whether the helper ran the
//! operation git would have run locally. The pipe is unix-only.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use lamina_core::lfs::LfsPointer;
use lamina_core::session::{Operation, OperationId, RepositorySession, Session, SessionEvent};
use lamina_core::test_support::TestRepo;
use lamina_core::{ReflogEntry, ReflogKind};
use lamina_remote_client::{RemoteSession, RemoteTarget};

const TIMEOUT: Duration = Duration::from_secs(30);

fn two_commits() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("README.md", "one\n");
    repo.commit("first");
    repo.write("README.md", "two\n");
    repo.commit("second");
    repo
}

fn open_local(repo: &TestRepo) -> (Session, Receiver<SessionEvent>) {
    let (tx, rx) = mpsc::channel();
    let session = Session::open(repo.path(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open local session");
    (session, rx)
}

struct Helper {
    child: std::process::Child,
    session: RemoteSession,
    events: Receiver<SessionEvent>,
}

impl Helper {
    fn spawn(repo: &TestRepo) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lamina-remote"))
            .arg("--stdio")
            .arg("--root")
            .arg(repo.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn lamina-remote");
        let stdin = child.stdin.take().expect("helper stdin");
        let stdout = child.stdout.take().expect("helper stdout");
        let (tx, rx) = mpsc::channel();
        let session = RemoteSession::connect(
            stdout,
            stdin,
            RemoteTarget::new("write-parity"),
            move |event| {
                let _ = tx.send(event);
            },
        )
        .expect("connect to the spawned helper");
        Self {
            child,
            session,
            events: rx,
        }
    }

    fn close(mut self) {
        drop(self.session);
        let status = self.child.wait().expect("wait for helper");
        assert!(status.success(), "helper exited with {status}");
    }
}

fn wait_finished(events: &Receiver<SessionEvent>, id: OperationId) -> Option<String> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(SessionEvent::OperationFinished {
                id: event_id,
                error,
            }) if event_id == id => return error,
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(error) => panic!("operation {id:?} did not finish: {error}"),
        }
    }
}

fn run_local(session: &Session, events: &Receiver<SessionEvent>, operation: Operation) {
    let id = session.run(operation);
    let error = wait_finished(events, id);
    assert_eq!(error, None, "local operation failed");
}

fn run_remote(helper: &Helper, operation: Operation) {
    let id = helper.session.run(operation);
    let error = wait_finished(&helper.events, id);
    assert_eq!(error, None, "remote operation failed");
}

/// `HEAD` and a clean or dirty worktree, which is what undo and redo move.
fn repository_state(repo: &TestRepo) -> (String, String) {
    // A file rewritten in the same instant as the index is "racily clean";
    // refreshing first makes every status compare contents.
    let _ = repo.git_output(&["update-index", "-q", "--really-refresh"]);
    let head = repo.git(&["rev-parse", "HEAD"]);
    let status = repo.git(&["status", "--porcelain"]);
    (head, status)
}

fn newest_head_commit(repo: &TestRepo) -> ReflogEntry {
    repo.open()
        .reflog(20)
        .expect("read reflog")
        .into_iter()
        .find(|entry| entry.reference == "HEAD" && entry.kind == ReflogKind::Commit)
        .expect("a HEAD commit entry")
}

#[test]
fn undo_and_redo_of_the_newest_action_match_a_local_session() {
    let local_repo = two_commits();
    let remote_repo = two_commits();
    let first = local_repo.rev("HEAD~1");
    assert_eq!(first, remote_repo.rev("HEAD~1"), "twins share object ids");
    let second = local_repo.head();

    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);

    // Captured before the reset. Redo replays this entry: its old id is where
    // undo leaves HEAD, and its new id is the commit to put back.
    let local_commit = newest_head_commit(&local_repo);
    let remote_commit = newest_head_commit(&remote_repo);
    assert_eq!(local_commit, remote_commit);
    assert_eq!(local_commit.new_id, second);

    run_local(&local, &local_events, Operation::UndoReflog { entry: None });
    run_remote(&helper, Operation::UndoReflog { entry: None });
    assert_eq!(local_repo.head(), first, "local undo moves to the parent");
    assert_eq!(
        repository_state(&local_repo),
        repository_state(&remote_repo),
        "undo leaves the same repository"
    );

    run_local(
        &local,
        &local_events,
        Operation::RedoReflog {
            entry: local_commit,
        },
    );
    run_remote(
        &helper,
        Operation::RedoReflog {
            entry: remote_commit,
        },
    );
    assert_eq!(
        repository_state(&local_repo),
        repository_state(&remote_repo),
        "redo leaves the same repository"
    );
    assert_eq!(local_repo.head(), second);

    helper.close();
}

#[test]
fn undo_of_a_chosen_reflog_entry_matches_a_local_session() {
    let local_repo = two_commits();
    let remote_repo = two_commits();
    let local_entry = newest_head_commit(&local_repo);
    let remote_entry = newest_head_commit(&remote_repo);
    assert_eq!(local_entry, remote_entry, "the commit entry is identical");

    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);
    run_local(
        &local,
        &local_events,
        Operation::UndoReflog {
            entry: Some(local_entry.clone()),
        },
    );
    run_remote(
        &helper,
        Operation::UndoReflog {
            entry: Some(remote_entry),
        },
    );
    assert_eq!(
        repository_state(&local_repo),
        repository_state(&remote_repo)
    );
    assert_eq!(local_repo.head(), local_entry.old_id);
    helper.close();
}

fn sibling(repo: &TestRepo, name: &str) -> std::path::PathBuf {
    repo.path()
        .parent()
        .expect("the repository sits in a temp dir")
        .join(name)
}

/// Worktree list with this machine's temp directory replaced, so two twins
/// can be compared even though their absolute paths differ.
fn worktree_list(repo: &TestRepo) -> String {
    let parent = repo
        .path()
        .parent()
        .expect("parent")
        .to_string_lossy()
        .into_owned();
    repo.git(&["worktree", "list", "--porcelain"])
        .replace(&parent, "$PARENT")
}

#[test]
fn adding_removing_and_pruning_a_worktree_matches_a_local_session() {
    let local_repo = two_commits();
    let remote_repo = two_commits();
    let local_head = local_repo.head();
    let remote_head = remote_repo.head();
    assert_eq!(local_head, remote_head);

    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);

    run_local(
        &local,
        &local_events,
        Operation::WorktreeAdd {
            path: sibling(&local_repo, "topic"),
            branch: None,
            new_branch: Some("topic".into()),
            commit: Some(local_head),
        },
    );
    run_remote(
        &helper,
        Operation::WorktreeAdd {
            path: sibling(&remote_repo, "topic"),
            branch: None,
            new_branch: Some("topic".into()),
            commit: Some(remote_head),
        },
    );
    assert_eq!(worktree_list(&local_repo), worktree_list(&remote_repo));
    assert_eq!(
        local_repo.git(&["rev-parse", "topic"]),
        remote_repo.git(&["rev-parse", "topic"])
    );

    run_local(
        &local,
        &local_events,
        Operation::WorktreeRemove {
            path: sibling(&local_repo, "topic"),
            force: false,
        },
    );
    run_remote(
        &helper,
        Operation::WorktreeRemove {
            path: sibling(&remote_repo, "topic"),
            force: false,
        },
    );
    assert_eq!(worktree_list(&local_repo), worktree_list(&remote_repo));
    assert!(
        !local_repo
            .git(&["worktree", "list", "--porcelain"])
            .contains("topic"),
        "remove drops the linked worktree"
    );

    // Prune drops a registration whose directory is already gone.
    let local_gone = sibling(&local_repo, "gone");
    let remote_gone = sibling(&remote_repo, "gone");
    let local_gone_text = local_gone.to_string_lossy();
    let remote_gone_text = remote_gone.to_string_lossy();
    let local_spec = local_head.to_string();
    let remote_spec = remote_head.to_string();
    local_repo.git(&[
        "worktree",
        "add",
        "--detach",
        "--",
        local_gone_text.as_ref(),
        &local_spec,
    ]);
    remote_repo.git(&[
        "worktree",
        "add",
        "--detach",
        "--",
        remote_gone_text.as_ref(),
        &remote_spec,
    ]);
    std::fs::remove_dir_all(&local_gone).expect("delete local checkout");
    std::fs::remove_dir_all(&remote_gone).expect("delete remote checkout");
    run_local(&local, &local_events, Operation::WorktreePrune);
    run_remote(&helper, Operation::WorktreePrune);
    assert_eq!(worktree_list(&local_repo), worktree_list(&remote_repo));
    assert!(
        !local_repo
            .git(&["worktree", "list", "--porcelain"])
            .contains("gone")
    );

    helper.close();
}

/// A superproject whose submodule checkout is one commit behind the gitlink.
///
/// The objects are already in the submodule, so the update does not have to
/// clone over `file://`. That transport is refused unless the invoking git
/// command opts in, and the helper runs plain git.
fn superproject() -> (TestRepo, TestRepo) {
    let library = TestRepo::new();
    library.write("lib.txt", "lib\n");
    let first = library.commit("lib");
    let repo = TestRepo::new();
    repo.write("README.md", "super\n");
    repo.commit("super");
    let url = format!("file://{}", library.path().display());
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "--",
        &url,
        "libs/demo",
    ]);
    repo.commit("add demo");
    library.write("lib.txt", "lib2\n");
    library.commit("lib2");
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "update",
        "--remote",
        "--",
        "libs/demo",
    ]);
    repo.commit("bump demo");
    let first = first.to_string();
    repo.git(&["-C", "libs/demo", "checkout", "-q", &first]);
    (repo, library)
}

fn submodule_checkout(repo: &TestRepo) -> String {
    format!(
        "{}{}",
        repo.git(&["-C", "libs/demo", "rev-parse", "HEAD"]),
        repo.git(&["submodule", "status"])
    )
}

#[test]
fn updating_a_submodule_matches_a_local_session() {
    let (local_repo, _local_library) = superproject();
    let (remote_repo, _remote_library) = superproject();
    assert_eq!(
        submodule_checkout(&local_repo),
        submodule_checkout(&remote_repo),
        "twins start one commit behind the same gitlink"
    );

    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);
    let update = Operation::SubmoduleUpdate {
        paths: vec!["libs/demo".into()],
        init: true,
    };
    run_local(&local, &local_events, update.clone());
    run_remote(&helper, update);
    assert_eq!(
        submodule_checkout(&local_repo),
        submodule_checkout(&remote_repo)
    );
    assert_eq!(
        local_repo.git(&["-C", "libs/demo", "rev-parse", "HEAD"]),
        local_repo.git(&["rev-parse", "HEAD:libs/demo"]),
        "update checks out the recorded commit"
    );
    helper.close();
}

const LFS_HEX: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// Same pointer commit in two twins. The object is planted by hand so the
/// read does not depend on `git-lfs` being installed.
fn pointer_repo() -> TestRepo {
    let repo = TestRepo::new();
    let pointer =
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{LFS_HEX}\nsize 6\n");
    repo.write("data.bin", &pointer);
    repo.write(".gitattributes", "*.bin filter=lfs\n");
    repo.commit("pointer");
    // The sessions' git reads the system config, the test's own git does not
    // (GIT_CONFIG_NOSYSTEM). A runner whose system config sets the LFS filter
    // left the sessions cleaning data.bin while the test's status compared
    // raw bytes, so the two sides disagreed at random. Configuring the filter
    // in the repository gives both the same view.
    if repo.git_output(&["lfs", "version"]).status.success() {
        for (key, value) in [
            ("filter.lfs.clean", "git-lfs clean -- %f"),
            ("filter.lfs.smudge", "git-lfs smudge -- %f"),
            ("filter.lfs.process", "git-lfs filter-process"),
            ("filter.lfs.required", "true"),
        ] {
            repo.git(&["config", key, value]);
        }
    }
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
    repo
}

/// Only for a failure message: what git-lfs did to one side.
fn lfs_details(repo: &TestRepo) -> String {
    let output = |args: &[&str]| {
        let out = repo.git_output(args);
        format!(
            "$ git {}\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };
    [
        output(&["config", "--local", "--get-regexp", "^(filter|lfs)\\."]),
        output(&["diff", "--cached", "--stat", "--", "data.bin"]),
        output(&["diff", "--stat", "--", "data.bin"]),
        output(&["cat-file", "-p", ":data.bin"]),
        format!(
            "worktree data.bin: {:?}\n",
            std::fs::read(repo.path().join("data.bin"))
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        ),
    ]
    .concat()
}

fn lfs_version(repo: &TestRepo) -> String {
    String::from_utf8_lossy(&repo.git_output(&["lfs", "version"]).stdout)
        .trim()
        .to_owned()
}

fn run_outcome_local(
    session: &Session,
    events: &Receiver<SessionEvent>,
    operation: Operation,
) -> Option<String> {
    let id = session.run(operation);
    wait_finished(events, id)
}

fn run_outcome_remote(helper: &Helper, operation: Operation) -> Option<String> {
    let id = helper.session.run(operation);
    wait_finished(&helper.events, id)
}

#[test]
fn lfs_reads_and_writes_match_a_local_session() {
    let local_repo = pointer_repo();
    let remote_repo = pointer_repo();
    assert_eq!(local_repo.head(), remote_repo.head());
    let commit = local_repo.head();

    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);
    let local_reader = local.reader();
    let remote_reader = helper.session.reader();

    assert_eq!(
        local_reader
            .lfs_info_at(commit, "data.bin")
            .expect("local info"),
        remote_reader
            .lfs_info_at(commit, "data.bin")
            .expect("remote info")
    );
    assert_eq!(
        local_reader.uses_lfs(),
        remote_reader.uses_lfs(),
        "both see the attributes pattern"
    );
    let pointer = LfsPointer {
        oid: format!("sha256:{LFS_HEX}"),
        size: 6,
    };
    assert_eq!(
        local_reader
            .read_lfs_object(&pointer, 1024)
            .expect("local object"),
        remote_reader
            .read_lfs_object(&pointer, 1024)
            .expect("remote object")
    );
    assert_eq!(
        local.git_extras().lfs_available(),
        helper.session.git_extras().lfs_available()
    );

    // Track rewrites `.gitattributes` when git-lfs is installed and fails the
    // same way on both sides when it is not. Lock, unlock and pull need an
    // LFS server, so a missing command or a missing remote still has to leave
    // both repositories identical.
    let writes = [
        Operation::LfsTrack {
            path: "*.bin".into(),
        },
        Operation::LfsLock {
            path: "data.bin".into(),
        },
        Operation::LfsUnlock {
            path: "data.bin".into(),
        },
        Operation::LfsPull {
            path: "data.bin".into(),
        },
    ];
    for operation in writes {
        let label = format!("{operation:?}");
        let local_error = run_outcome_local(&local, &local_events, operation.clone());
        let remote_error = run_outcome_remote(&helper, operation);
        assert_eq!(
            local_error.is_none(),
            remote_error.is_none(),
            "after {label}: local {local_error:?} remote {remote_error:?}"
        );
        let local_state = repository_state(&local_repo);
        let remote_state = repository_state(&remote_repo);
        assert_eq!(
            local_state,
            remote_state,
            "after {label} (local {local_error:?}, remote {remote_error:?})\n\
             git-lfs: {}\nlocal:\n{}\nremote:\n{}",
            lfs_version(&local_repo),
            lfs_details(&local_repo),
            lfs_details(&remote_repo),
        );
    }
    helper.close();
}

/// The local side's configuration access, the same shape the app uses for a
/// local tab: `git config` runs in this process against the checkout.
struct LocalAccess {
    repository: std::path::PathBuf,
}

impl lamina_core::git_cli::GitConfigAccess for LocalAccess {
    fn load_config(&self) -> lamina_core::Result<lamina_core::repo::GitConfig> {
        lamina_core::repo::GitConfig::load(&self.repository)
    }

    fn preview_config(
        &self,
        write: &lamina_core::git_cli::ConfigWrite,
    ) -> lamina_core::Result<lamina_core::git_cli::ConfigPreview> {
        lamina_core::git_cli::Git::new(self.repository.clone()).config_preview(write)
    }

    fn apply_config(
        &self,
        preview: &lamina_core::git_cli::ConfigPreview,
    ) -> lamina_core::Result<()> {
        lamina_core::git_cli::Git::new(self.repository.clone()).config_apply(preview)
    }
}

fn local_value(repo: &TestRepo, key: &str) -> String {
    repo.git(&["config", "--local", "--get", key])
        .trim()
        .to_owned()
}

#[test]
fn a_config_read_preview_and_write_match_a_local_access() {
    use lamina_core::git_cli::{ConfigChange, ConfigTarget, ConfigWrite, GitConfigAccess};
    use lamina_core::repo::ConfigLayer;

    let local_repo = two_commits();
    let remote_repo = two_commits();
    let helper = Helper::spawn(&remote_repo);
    let local_access = LocalAccess {
        repository: local_repo.path().to_path_buf(),
    };
    let remote_access = helper
        .session
        .git_config_access()
        .expect("a remote session reaches the host's config");

    // Both reads describe their own checkout, including the file list a write
    // has to name.
    let local_config = local_access.load_config().expect("local config");
    let remote_config = remote_access.load_config().expect("remote config");
    assert_eq!(
        local_config.files.local,
        local_repo.path().join(".git/config")
    );
    assert_eq!(
        remote_config.files.local,
        remote_repo.path().join(".git/config")
    );
    assert_eq!(
        local_config
            .entry("user.name")
            .and_then(|entry| entry.effective())
            .map(|value| value.value.clone()),
        remote_config
            .entry("user.name")
            .and_then(|entry| entry.effective())
            .map(|value| value.value.clone()),
        "twins start with the same identity"
    );

    let write_for = |config: &lamina_core::repo::GitConfig| {
        ConfigWrite::new(
            ConfigTarget::new(config.files.local.clone(), ConfigLayer::Local),
            "user.name",
            ConfigChange::Set("Ada".into()),
        )
    };
    let local_preview = local_access
        .preview_config(&write_for(&local_config))
        .expect("local preview");
    let remote_preview = remote_access
        .preview_config(&write_for(&remote_config))
        .expect("remote preview");
    assert_eq!(local_preview.after, remote_preview.after);
    assert!(local_preview.changed());

    local_access
        .apply_config(&local_preview)
        .expect("local save");
    remote_access
        .apply_config(&remote_preview)
        .expect("remote save");
    assert_eq!(local_value(&local_repo, "user.name"), "Ada");
    assert_eq!(
        local_value(&remote_repo, "user.name"),
        "Ada",
        "the helper wrote the host's file through git config"
    );

    // A file that moved since the preview is refused on both sides, so a
    // concurrent edit is never overwritten.
    assert!(local_access.apply_config(&local_preview).is_err());
    assert!(remote_access.apply_config(&remote_preview).is_err());
    assert_eq!(local_value(&local_repo, "user.name"), "Ada");
    assert_eq!(local_value(&remote_repo, "user.name"), "Ada");

    let loaded = remote_access.load_config().expect("reread");
    let value = loaded
        .entry("user.name")
        .and_then(|entry| entry.effective())
        .expect("the written value reads back");
    assert_eq!(value.value, "Ada");
    assert_eq!(value.layer, ConfigLayer::Local);
    helper.close();
}

/// A patch that adds one file, so applying it does not depend on the twins'
/// working trees.
const NEW_FILE_PATCH: &[u8] = b"diff --git a/added.txt b/added.txt
new file mode 100644
index 0000000..ce01362
--- /dev/null
+++ b/added.txt
@@ -0,0 +1 @@
+hello
";

#[test]
fn applying_a_patch_matches_a_local_session() {
    let local_repo = two_commits();
    let remote_repo = two_commits();
    let (local, local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);

    run_local(
        &local,
        &local_events,
        Operation::ApplyPatch {
            patch: NEW_FILE_PATCH.to_vec(),
        },
    );
    run_remote(
        &helper,
        Operation::ApplyPatch {
            patch: NEW_FILE_PATCH.to_vec(),
        },
    );
    let read = |repo: &TestRepo| {
        std::fs::read_to_string(repo.path().join("added.txt")).expect("the patched file")
    };
    assert_eq!(
        read(&local_repo),
        read(&remote_repo),
        "both working trees got the patched file"
    );
    assert_eq!(
        repository_state(&local_repo),
        repository_state(&remote_repo)
    );

    // Garbage is refused on both sides.
    let local_error = run_outcome_local(
        &local,
        &local_events,
        Operation::ApplyPatch {
            patch: b"not a patch\n".to_vec(),
        },
    );
    let remote_error = run_outcome_remote(
        &helper,
        Operation::ApplyPatch {
            patch: b"not a patch\n".to_vec(),
        },
    );
    assert!(local_error.is_some(), "local git refuses the bytes");
    assert!(remote_error.is_some(), "the helper refuses them too");
    helper.close();
}

#[test]
fn saving_a_commit_as_a_patch_returns_the_same_bytes() {
    let local_repo = two_commits();
    let remote_repo = two_commits();
    let commit = local_repo.head();
    assert_eq!(commit, remote_repo.head());

    let (local, _local_events) = open_local(&local_repo);
    let helper = Helper::spawn(&remote_repo);
    let local_patch = local
        .git_extras()
        .format_patch(commit)
        .expect("local format-patch");
    let remote_patch = helper
        .session
        .git_extras()
        .format_patch(commit)
        .expect("remote format-patch");
    assert_eq!(local_patch, remote_patch);
    assert!(
        String::from_utf8_lossy(&remote_patch).contains("README.md"),
        "the patch names the file the commit changed"
    );
    helper.close();
}
