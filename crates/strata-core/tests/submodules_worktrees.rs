//! Integration tests for submodule and worktree reads and writes.
//!
//! Every repository lives in a `TempDir` and runs with an empty global/system
//! config plus a fixed identity, so the machine's git settings cannot leak in.
//! The config allows the file protocol, which `git submodule update` needs to
//! clone from another local repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use strata_core::git_cli::Git;
use strata_core::repo::{Repo, SubmoduleInfo, SubmoduleStatus, WorktreeInfo};
use strata_core::session::{Operation, OperationId, Session, SessionEvent};
use strata_core::{HeadInfo, Oid};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(60);

struct Fixture {
    root: TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().expect("root temp dir");
        let config = root.path().join("gitconfig");
        fs::write(
            &config,
            concat!(
                "[protocol \"file\"]\n\tallow = always\n",
                "[init]\n\tdefaultBranch = main\n",
                "[user]\n\tname = Strata Test\n\temail = test@example.com\n",
                "[commit]\n\tgpgsign = false\n",
                "[core]\n\tautocrlf = false\n",
            ),
        )
        .expect("write test git config");
        Self { root, config }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn command(&self, workdir: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(workdir)
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_SYSTEM", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Strata Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Strata Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00 +0000")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00 +0000")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        command
    }

    fn git_output(&self, workdir: &Path, args: &[&str]) -> Output {
        self.command(workdir).args(args).output().expect("run git")
    }

    fn git(&self, workdir: &Path, args: &[&str]) -> String {
        let output = self.git_output(workdir, args);
        assert!(
            output.status.success(),
            "git {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git stdout is UTF-8")
    }

    /// The `Git` handle the session would build: same isolated environment.
    ///
    /// `GIT_CONFIG_*` is the environment form of `git -c`; unlike a repository
    /// or global setting, git passes it on to the `git clone` it spawns for a
    /// submodule, which is what allows the file transport there.
    fn git_handle(&self, workdir: &Path) -> Git {
        Git::new(workdir.to_path_buf())
            .with_env("GIT_CONFIG_GLOBAL", self.config.clone().into_os_string())
            .with_env("GIT_CONFIG_SYSTEM", self.config.clone().into_os_string())
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_CONFIG_COUNT", "1")
            .with_env("GIT_CONFIG_KEY_0", "protocol.file.allow")
            .with_env("GIT_CONFIG_VALUE_0", "always")
            .with_env("GIT_AUTHOR_NAME", "Strata Test")
            .with_env("GIT_AUTHOR_EMAIL", "test@example.com")
            .with_env("GIT_COMMITTER_NAME", "Strata Test")
            .with_env("GIT_COMMITTER_EMAIL", "test@example.com")
    }

    fn init_repo(&self, name: &str) -> PathBuf {
        let dir = self.path().join(name);
        fs::create_dir_all(&dir).expect("repo dir");
        self.git(&dir, &["init", "-q"]);
        dir
    }

    fn write(&self, workdir: &Path, relative: &str, contents: &str) {
        let path = workdir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write file");
    }

    fn commit_all(&self, workdir: &Path, message: &str) {
        self.git(workdir, &["add", "-A"]);
        self.git(workdir, &["commit", "-q", "-m", message]);
    }

    fn head(&self, workdir: &Path) -> Oid {
        self.git(workdir, &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("parse HEAD")
    }

    /// A superproject whose HEAD records one cloned (`initialized`) and one
    /// never-cloned (`uninitialised`) submodule, both pointing at the same
    /// commit of `source`.
    fn repo_with_submodules(&self) -> (PathBuf, Oid) {
        let source = self.init_repo("source");
        self.write(&source, "file.txt", "source\n");
        self.commit_all(&source, "source");
        let source_head = self.head(&source);
        let source_url = source.to_str().expect("source path is UTF-8").to_owned();

        let superproject = self.init_repo("super");
        self.write(&superproject, "base.txt", "base\n");
        self.commit_all(&superproject, "base");
        self.git(
            &superproject,
            &["submodule", "add", "-q", &source_url, "initialized"],
        );
        self.commit_all(&superproject, "add an initialised submodule");

        // A gitlink plus a `.gitmodules` entry without a clone is exactly what an
        // uninitialised submodule looks like on disk.
        let modules = superproject.join(".gitmodules");
        let mut text = fs::read_to_string(&modules).expect("read .gitmodules");
        text.push_str(&format!(
            "\n[submodule \"uninitialised\"]\n\tpath = uninitialised\n\turl = {source_url}\n"
        ));
        fs::write(&modules, text).expect("append .gitmodules entry");
        self.git(&superproject, &["add", ".gitmodules"]);
        self.git(
            &superproject,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{source_head},uninitialised"),
            ],
        );
        // Not `commit_all`: its `git add -A` would stage the absent checkout as a
        // deletion and drop the gitlink again.
        self.git(
            &superproject,
            &["commit", "-q", "-m", "record an uninitialised submodule"],
        );

        (superproject, source_head)
    }
}

fn submodule<'a>(submodules: &'a [SubmoduleInfo], path: &str) -> &'a SubmoduleInfo {
    submodules
        .iter()
        .find(|submodule| submodule.path == path)
        .unwrap_or_else(|| panic!("no submodule at {path}"))
}

/// Resolve symlinked temp roots (`/var` vs `/private/var` on macOS) and paths
/// whose own directory is gone, which is what a prunable worktree has left.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(path) {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => match fs::canonicalize(parent) {
            Ok(parent) => parent.join(name),
            Err(_) => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

fn worktree<'a>(worktrees: &'a [WorktreeInfo], path: &Path) -> &'a WorktreeInfo {
    let path = canonical(path);
    worktrees
        .iter()
        .find(|worktree| canonical(&worktree.path) == path)
        .unwrap_or_else(|| panic!("no worktree at {}", path.display()))
}

/// Waits for the first event `pick` accepts, ignoring everything else.
fn wait_for_event<T>(
    events: &Receiver<SessionEvent>,
    timeout: Duration,
    mut pick: impl FnMut(SessionEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for a session event");
        match events.recv_timeout(deadline - now) {
            Ok(event) => {
                if let Some(value) = pick(event) {
                    return value;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("session event channel closed"),
        }
    }
}

fn wait_for_operation(events: &Receiver<SessionEvent>, id: OperationId) {
    let error = wait_for_event(events, TIMEOUT, |event| match event {
        SessionEvent::OperationFinished {
            id: finished,
            error,
        } if finished == id => Some(error),
        _ => None,
    });
    assert_eq!(error, None);
}

#[test]
fn lists_initialised_and_uninitialised_submodules() {
    let fixture = Fixture::new();
    let (superproject, source_head) = fixture.repo_with_submodules();

    let submodules = Repo::open(&superproject)
        .expect("open repository")
        .submodules()
        .expect("submodules");
    assert_eq!(submodules.len(), 2, "{submodules:#?}");
    assert_eq!(submodules[0].path, "initialized", "sorted by path");
    assert_eq!(submodules[1].path, "uninitialised");

    let initialised = submodule(&submodules, "initialized");
    assert_eq!(initialised.name, "initialized");
    assert_eq!(initialised.status, SubmoduleStatus::UpToDate);
    assert_eq!(initialised.recorded_commit, Some(source_head));
    assert_eq!(initialised.checked_out_commit, Some(source_head));
    assert!(
        initialised
            .url
            .as_deref()
            .is_some_and(|url| url.contains("source")),
        "url: {:?}",
        initialised.url
    );

    let uninitialised = submodule(&submodules, "uninitialised");
    assert_eq!(uninitialised.name, "uninitialised");
    assert_eq!(uninitialised.status, SubmoduleStatus::Uninitialised);
    assert_eq!(uninitialised.recorded_commit, Some(source_head));
    assert_eq!(uninitialised.checked_out_commit, None);
    assert!(uninitialised.url.is_some());
}

#[test]
fn detects_modified_and_moved_submodule_checkouts() {
    let fixture = Fixture::new();
    let (superproject, source_head) = fixture.repo_with_submodules();
    let checkout = superproject.join("initialized");

    fixture.write(&checkout, "file.txt", "locally modified\n");
    let submodules = Repo::open(&superproject)
        .expect("open repository")
        .submodules()
        .expect("submodules");
    assert_eq!(
        submodule(&submodules, "initialized").status,
        SubmoduleStatus::Modified
    );

    fixture.git(&checkout, &["checkout", "--", "file.txt"]);
    let submodules = Repo::open(&superproject)
        .expect("open repository")
        .submodules()
        .expect("submodules");
    assert_eq!(
        submodule(&submodules, "initialized").status,
        SubmoduleStatus::UpToDate
    );

    // A commit inside the submodule that the superproject does not record.
    fixture.git(&checkout, &["commit", "--allow-empty", "-q", "-m", "local"]);
    let moved = fixture.head(&checkout);
    assert_ne!(moved, source_head);
    let submodules = Repo::open(&superproject)
        .expect("open repository")
        .submodules()
        .expect("submodules");
    let initialised = submodule(&submodules, "initialized");
    assert_eq!(initialised.status, SubmoduleStatus::CommitDiffers);
    assert_eq!(initialised.recorded_commit, Some(source_head));
    assert_eq!(initialised.checked_out_commit, Some(moved));
}

#[test]
fn submodule_update_init_checks_out_the_uninitialised_submodule() {
    let fixture = Fixture::new();
    let (superproject, source_head) = fixture.repo_with_submodules();
    let git = fixture.git_handle(&superproject);

    git.submodule_update(&["uninitialised"], true)
        .expect("submodule update --init");

    assert!(
        superproject.join("uninitialised/file.txt").is_file(),
        "the clone must be checked out"
    );
    let submodules = Repo::open(&superproject)
        .expect("open repository")
        .submodules()
        .expect("submodules");
    let updated = submodule(&submodules, "uninitialised");
    assert_eq!(updated.status, SubmoduleStatus::UpToDate);
    assert_eq!(updated.checked_out_commit, Some(source_head));
}

#[test]
fn lists_main_linked_and_detached_worktrees() {
    let fixture = Fixture::new();
    let root = fixture.init_repo("repo");
    fixture.write(&root, "a.txt", "a\n");
    fixture.commit_all(&root, "init");
    let head = fixture.head(&root);
    let git = fixture.git_handle(&root);

    let linked = fixture.path().join("linked");
    fs::create_dir_all(&linked).expect("linked parent");
    let existing = linked.join("existing");
    let created = linked.join("created");
    let detached = linked.join("detached");

    fixture.git(&root, &["branch", "side"]);
    git.worktree_add(&existing, Some("side"), None, None)
        .expect("add worktree on an existing branch");
    git.worktree_add(&created, None, Some("topic"), Some(head))
        .expect("add worktree on a new branch");
    git.worktree_add(&detached, None, None, Some(head))
        .expect("add a detached worktree");

    let worktrees = Repo::open(&root)
        .expect("open repository")
        .worktrees()
        .expect("worktrees");
    assert_eq!(worktrees.len(), 4, "{worktrees:#?}");

    let main = worktree(&worktrees, &root);
    assert!(main.is_main);
    assert!(!main.locked);
    assert!(!main.prunable);
    assert_eq!(
        main.head,
        Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(head),
        })
    );

    let existing = worktree(&worktrees, &existing);
    assert!(!existing.is_main);
    assert_eq!(
        existing.head,
        Some(HeadInfo::Branch {
            short_name: "side".into(),
            target: Some(head),
        })
    );

    let created = worktree(&worktrees, &created);
    assert_eq!(
        created.head,
        Some(HeadInfo::Branch {
            short_name: "topic".into(),
            target: Some(head),
        })
    );

    let detached = worktree(&worktrees, &detached);
    assert_eq!(detached.head, Some(HeadInfo::Detached { target: head }));
}

#[test]
fn shows_locked_worktrees_with_their_reason() {
    let fixture = Fixture::new();
    let root = fixture.init_repo("repo");
    fixture.write(&root, "a.txt", "a\n");
    fixture.commit_all(&root, "init");
    let head = fixture.head(&root);
    let git = fixture.git_handle(&root);

    let linked = fixture.path().join("locked-worktree");
    git.worktree_add(&linked, None, Some("topic"), Some(head))
        .expect("add worktree");
    fixture.git(
        &root,
        &[
            "worktree",
            "lock",
            "--reason",
            "keep for review",
            linked.to_str().expect("path is UTF-8"),
        ],
    );

    let worktrees = Repo::open(&root)
        .expect("open repository")
        .worktrees()
        .expect("worktrees");
    let locked = worktree(&worktrees, &linked);
    assert!(locked.locked);
    assert_eq!(locked.lock_reason.as_deref(), Some("keep for review"));
    assert!(!locked.prunable);
}

#[test]
fn removes_a_linked_worktree() {
    let fixture = Fixture::new();
    let root = fixture.init_repo("repo");
    fixture.write(&root, "a.txt", "a\n");
    fixture.commit_all(&root, "init");
    let head = fixture.head(&root);
    let git = fixture.git_handle(&root);

    let linked = fixture.path().join("removable");
    git.worktree_add(&linked, None, Some("topic"), Some(head))
        .expect("add worktree");
    assert_eq!(
        Repo::open(&root)
            .expect("open repository")
            .worktrees()
            .expect("worktrees")
            .len(),
        2
    );

    git.worktree_remove(&linked, false)
        .expect("worktree remove");

    assert!(!linked.exists());
    let worktrees = Repo::open(&root)
        .expect("open repository")
        .worktrees()
        .expect("worktrees");
    assert_eq!(worktrees.len(), 1);
    assert!(worktrees[0].is_main);
}

#[test]
fn prunes_a_worktree_whose_directory_is_gone() {
    let fixture = Fixture::new();
    let root = fixture.init_repo("repo");
    fixture.write(&root, "a.txt", "a\n");
    fixture.commit_all(&root, "init");
    let head = fixture.head(&root);
    let git = fixture.git_handle(&root);

    let linked = fixture.path().join("gone");
    git.worktree_add(&linked, None, None, Some(head))
        .expect("add detached worktree");
    fs::remove_dir_all(&linked).expect("delete the checkout");

    let worktrees = Repo::open(&root)
        .expect("open repository")
        .worktrees()
        .expect("worktrees");
    let gone = worktree(&worktrees, &linked);
    assert!(gone.prunable);
    assert!(!gone.locked);

    git.worktree_prune().expect("worktree prune");

    let worktrees = Repo::open(&root)
        .expect("open repository")
        .worktrees()
        .expect("worktrees");
    assert_eq!(worktrees.len(), 1, "{worktrees:#?}");
    assert!(worktrees[0].is_main);
}

#[test]
fn session_answers_submodules_and_worktrees_query() {
    let fixture = Fixture::new();
    let (superproject, source_head) = fixture.repo_with_submodules();

    let (tx, events) = mpsc::channel();
    let session = Session::open(&superproject, move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");

    let ticket = session.request_submodules_and_worktrees();
    let (submodules, worktrees) = wait_for_event(&events, TIMEOUT, |event| match event {
        SessionEvent::SubmodulesAndWorktreesReady {
            ticket: got,
            submodules,
            worktrees,
        } if got == ticket => Some((submodules, worktrees)),
        SessionEvent::QueryFailed { ticket: got, error } if got == ticket => {
            panic!("query {ticket:?} failed: {error}")
        }
        _ => None,
    });
    assert_eq!(submodules.len(), 2);
    assert_eq!(
        submodule(&submodules, "initialized").checked_out_commit,
        Some(source_head)
    );
    assert_eq!(
        submodule(&submodules, "uninitialised").status,
        SubmoduleStatus::Uninitialised
    );
    assert_eq!(worktrees.len(), 1);
    assert!(worktrees[0].is_main);

    // The session's git has none of the test's environment, so only a
    // submodule that is already cloned can be updated through it.
    let id = session.run(Operation::SubmoduleUpdate {
        paths: vec!["initialized".into()],
        init: false,
    });
    wait_for_operation(&events, id);

    let ticket = session.request_submodules_and_worktrees();
    let submodules = wait_for_event(&events, TIMEOUT, |event| match event {
        SessionEvent::SubmodulesAndWorktreesReady {
            ticket: got,
            submodules,
            ..
        } if got == ticket => Some(submodules),
        SessionEvent::QueryFailed { ticket: got, error } if got == ticket => {
            panic!("query {ticket:?} failed: {error}")
        }
        _ => None,
    });
    assert_eq!(
        submodule(&submodules, "initialized").status,
        SubmoduleStatus::UpToDate
    );
    assert_eq!(
        submodule(&submodules, "uninitialised").status,
        SubmoduleStatus::Uninitialised
    );
}
