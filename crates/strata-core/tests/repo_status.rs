//! Integration tests for `Repo::status`, `Repo::commit_changes` and file content.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use strata_core::diff::{DiffOptions, LineKind, diff_text};
use strata_core::repo::Repo;
use strata_core::{ChangeKind, FileChange, Oid, WorktreeStatus};
use tempfile::TempDir;

/// A repository in a temp dir plus an isolated git environment.
struct TestRepo {
    dir: TempDir,
    /// Holds the empty config file; kept alive for the lifetime of the test.
    _env: TempDir,
    config: PathBuf,
}

impl TestRepo {
    fn new() -> Self {
        let dir = TempDir::new().expect("repo temp dir");
        let env = TempDir::new().expect("env temp dir");
        let config = env.path().join("gitconfig");
        fs::write(&config, "").expect("write empty config");

        let repo = TestRepo {
            dir,
            _env: env,
            config,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn open(&self) -> Repo {
        Repo::open(self.path()).expect("open repository")
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, contents).expect("write file");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(self.path())
            .args(args)
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

    fn git(&self, args: &[&str]) -> String {
        let output = self.git_output(args);
        assert!(
            output.status.success(),
            "git {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git stdout is UTF-8")
    }

    fn git_output(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run git")
    }

    fn head(&self) -> Oid {
        self.git(&["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("parse HEAD")
    }
}

fn kind_for(code: char) -> Option<ChangeKind> {
    match code {
        'A' => Some(ChangeKind::Added),
        'M' => Some(ChangeKind::Modified),
        'D' => Some(ChangeKind::Deleted),
        'R' => Some(ChangeKind::Renamed),
        'C' => Some(ChangeKind::Copied),
        'T' => Some(ChangeKind::TypeChanged),
        'U' => Some(ChangeKind::Conflicted),
        _ => None,
    }
}

fn push_tracked(status: &mut WorktreeStatus, xy: &str, path: &str, old_path: Option<&str>) {
    let mut codes = xy.chars();
    let staged_code = codes.next().unwrap_or('.');
    let unstaged_code = codes.next().unwrap_or('.');
    if let Some(kind) = kind_for(staged_code) {
        status.staged.push(FileChange {
            path: path.to_owned(),
            old_path: old_path.map(str::to_owned),
            kind,
        });
    }
    if let Some(kind) = kind_for(unstaged_code) {
        status.unstaged.push(FileChange {
            path: path.to_owned(),
            old_path: old_path.map(str::to_owned),
            kind,
        });
    }
}

/// Parse `git status --porcelain=v2 -uall` into the shape our `status()` returns.
fn parse_porcelain(output: &str) -> WorktreeStatus {
    let mut status = WorktreeStatus::default();

    for line in output.lines() {
        if let Some(path) = line.strip_prefix("? ") {
            status.unstaged.push(FileChange {
                path: path.to_owned(),
                old_path: None,
                kind: ChangeKind::Untracked,
            });
        } else if let Some(rest) = line.strip_prefix("u ") {
            let fields: Vec<&str> = rest.splitn(10, ' ').collect();
            let path = fields.last().expect("unmerged path");
            status.unstaged.push(FileChange {
                path: (*path).to_owned(),
                old_path: None,
                kind: ChangeKind::Conflicted,
            });
        } else if let Some(rest) = line.strip_prefix("1 ") {
            let fields: Vec<&str> = rest.splitn(8, ' ').collect();
            push_tracked(&mut status, fields[0], fields[7], None);
        } else if let Some(rest) = line.strip_prefix("2 ") {
            let fields: Vec<&str> = rest.splitn(9, ' ').collect();
            let mut paths = fields[8].splitn(2, '\t');
            let path = paths.next().expect("rename destination");
            let old_path = paths.next().expect("rename source");
            push_tracked(&mut status, fields[0], path, Some(old_path));
        }
        // `!` (ignored) lines are not requested and would not be part of status.
    }

    status.staged.sort_by(|a, b| a.path.cmp(&b.path));
    status.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    status
}

fn porcelain_status(repo: &TestRepo) -> WorktreeStatus {
    let output = repo.git(&["status", "--porcelain=v2", "-uall"]);
    parse_porcelain(&output)
}

/// Both sides of the status must agree with git, which also checks ordering.
fn assert_matches_git(repo: &TestRepo) {
    let expected = porcelain_status(repo);
    let actual = repo.open().status().expect("status");
    assert_eq!(actual.staged, expected.staged, "staged changes");
    assert_eq!(actual.unstaged, expected.unstaged, "unstaged changes");
}

fn added(path: &str) -> FileChange {
    FileChange {
        path: path.to_owned(),
        old_path: None,
        kind: ChangeKind::Added,
    }
}

fn modified(path: &str) -> FileChange {
    FileChange {
        path: path.to_owned(),
        old_path: None,
        kind: ChangeKind::Modified,
    }
}

fn deleted(path: &str) -> FileChange {
    FileChange {
        path: path.to_owned(),
        old_path: None,
        kind: ChangeKind::Deleted,
    }
}

fn untracked(path: &str) -> FileChange {
    FileChange {
        path: path.to_owned(),
        old_path: None,
        kind: ChangeKind::Untracked,
    }
}

fn type_changed(path: &str) -> FileChange {
    FileChange {
        path: path.to_owned(),
        old_path: None,
        kind: ChangeKind::TypeChanged,
    }
}

/// The reference for `commit_changes`: `git diff-tree -r --no-renames
/// --name-status` against the first parent, or the empty tree for a root commit.
fn git_commit_changes(repo: &TestRepo, commit: &str) -> Vec<FileChange> {
    let parent = format!("{commit}^1");
    let has_parent = repo
        .command(&["rev-parse", "--verify", "--quiet", &parent])
        .status()
        .expect("run git")
        .success();
    let mut args = vec![
        "diff-tree",
        "-r",
        "--no-renames",
        "--no-commit-id",
        "--name-status",
    ];
    if has_parent {
        args.push(&parent);
    } else {
        args.push("--root");
    }
    args.push(commit);

    repo.git(&args)
        .lines()
        .map(|line| {
            let (code, path) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("name-status line without a tab: {line:?}"));
            FileChange {
                path: path.to_owned(),
                old_path: None,
                kind: kind_for(code.chars().next().expect("status code"))
                    .unwrap_or_else(|| panic!("unexpected status code {code:?}")),
            }
        })
        .collect()
}

fn commit_changes(repo: &TestRepo, commit: &str) -> Vec<FileChange> {
    repo.open()
        .commit_changes(commit.parse().expect("commit oid"))
        .expect("commit changes")
}

/// Replace the worktree copy of `path` with what `git checkout` writes, then
/// backdate it so a content comparison is required. This is the state a Windows
/// checkout with CRLF conversion leaves behind.
fn checkout_crlf(repo: &TestRepo, path: &str) {
    let file = repo.path().join(path);
    fs::remove_file(&file).expect("remove worktree file");
    repo.git(&["checkout", "--", path]);

    let file = fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .expect("open worktree file");
    let stale = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    file.set_times(fs::FileTimes::new().set_modified(stale))
        .expect("backdate worktree file");
}

#[test]
fn clean_repository_has_no_changes() {
    let repo = TestRepo::new();
    repo.write("a.txt", "hello\n");
    repo.git(&["add", "--", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let status = repo.open().status().expect("status");
    assert_eq!(status, WorktreeStatus::default());
    assert_matches_git(&repo);
}

#[test]
fn unstaged_modification() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "--", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);
    repo.write("a.txt", "two\n");

    let status = repo.open().status().expect("status");
    assert!(status.staged.is_empty());
    assert_eq!(status.unstaged, vec![modified("a.txt")]);
    assert_matches_git(&repo);
}

#[test]
fn staged_add_modify_and_delete() {
    let repo = TestRepo::new();
    repo.write("keep.txt", "keep\n");
    repo.write("remove.txt", "remove\n");
    repo.git(&["add", "--", "keep.txt", "remove.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    repo.write("keep.txt", "changed\n");
    repo.git(&["add", "--", "keep.txt"]);
    repo.git(&["rm", "-q", "remove.txt"]);
    repo.write("new.txt", "new\n");
    repo.git(&["add", "--", "new.txt"]);

    let status = repo.open().status().expect("status");
    assert_eq!(
        status.staged,
        vec![
            modified("keep.txt"),
            added("new.txt"),
            deleted("remove.txt")
        ]
    );
    assert!(status.unstaged.is_empty());
    assert_matches_git(&repo);
}

#[test]
fn partially_staged_file_appears_in_both_lists() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "--", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    repo.write("a.txt", "two\n");
    repo.git(&["add", "--", "a.txt"]);
    repo.write("a.txt", "three\n");

    let status = repo.open().status().expect("status");
    assert_eq!(status.staged, vec![modified("a.txt")]);
    assert_eq!(status.unstaged, vec![modified("a.txt")]);
    assert_matches_git(&repo);
}

#[test]
fn untracked_files_are_listed_individually_and_ignored_ones_skipped() {
    let repo = TestRepo::new();
    repo.write(".gitignore", "*.log\nbuild/\n");
    repo.write("tracked.txt", "tracked\n");
    repo.git(&["add", "--", ".gitignore", "tracked.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    repo.write("uno.txt", "uno\n");
    repo.write("dir/a.txt", "a\n");
    repo.write("dir/sub/b.txt", "b\n");
    repo.write("ignored.log", "log\n");
    repo.write("build/out.bin", "bin\n");

    let status = repo.open().status().expect("status");
    assert!(status.staged.is_empty());
    assert_eq!(
        status.unstaged,
        vec![
            untracked("dir/a.txt"),
            untracked("dir/sub/b.txt"),
            untracked("uno.txt"),
        ]
    );
    assert_matches_git(&repo);
}

#[test]
fn unborn_head_reports_index_entries_as_added() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("nested/b.txt", "b\n");
    repo.git(&["add", "--", "a.txt", "nested/b.txt"]);

    let status = repo.open().status().expect("status");
    assert_eq!(status.staged, vec![added("a.txt"), added("nested/b.txt")]);
    assert!(status.unstaged.is_empty());
    assert_matches_git(&repo);
}

#[test]
fn staged_rename_is_detected() {
    let repo = TestRepo::new();
    repo.write("old.txt", "content that is identical after the rename\n");
    repo.git(&["add", "--", "old.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);
    repo.git(&["mv", "old.txt", "new.txt"]);

    let status = repo.open().status().expect("status");
    assert_eq!(
        status.staged,
        vec![FileChange {
            path: "new.txt".to_owned(),
            old_path: Some("old.txt".to_owned()),
            kind: ChangeKind::Renamed,
        }]
    );
    assert!(status.unstaged.is_empty());
    assert_matches_git(&repo);
}

#[test]
fn commit_changes_detects_rename_and_modification() {
    let repo = TestRepo::new();
    repo.write("old.txt", "content that survives a rename intact\n");
    repo.write("modify.txt", "one\n");
    repo.git(&["add", "--", "old.txt", "modify.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    repo.git(&["mv", "old.txt", "new.txt"]);
    repo.write("modify.txt", "two\n");
    repo.git(&["add", "--", "modify.txt"]);
    repo.git(&["commit", "-q", "-m", "second"]);

    let changes = repo
        .open()
        .commit_changes(repo.head())
        .expect("commit changes");
    assert_eq!(
        changes,
        vec![
            modified("modify.txt"),
            FileChange {
                path: "new.txt".to_owned(),
                old_path: Some("old.txt".to_owned()),
                kind: ChangeKind::Renamed,
            },
        ]
    );
}

#[test]
fn root_commit_changes_are_additions() {
    let repo = TestRepo::new();
    repo.write("root.txt", "root\n");
    repo.write("nested/child.txt", "child\n");
    repo.git(&["add", "--", "root.txt", "nested/child.txt"]);
    repo.git(&["commit", "-q", "-m", "root"]);

    let head = repo.head().to_string();
    let changes = commit_changes(&repo, &head);
    assert_eq!(changes, vec![added("nested/child.txt"), added("root.txt")]);
    assert_eq!(changes, git_commit_changes(&repo, &head));
}

#[test]
fn commit_changes_in_a_subdirectory_lists_only_the_file() {
    let repo = TestRepo::new();
    repo.write("src/lib.rs", "one\n");
    repo.write("src/main.rs", "main\n");
    repo.git(&["add", "--", "src/lib.rs", "src/main.rs"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    repo.write("src/lib.rs", "two\n");
    repo.git(&["add", "--", "src/lib.rs"]);
    repo.git(&["commit", "-q", "-m", "modify"]);

    let head = repo.head().to_string();
    let changes = commit_changes(&repo, &head);
    assert_eq!(
        changes,
        vec![modified("src/lib.rs")],
        "the changed `src` tree is not a file and must not be reported"
    );
    assert_eq!(changes, git_commit_changes(&repo, &head));
}

#[test]
fn commit_changes_for_added_and_removed_directories_lists_files() {
    let repo = TestRepo::new();
    repo.write("keep.txt", "keep\n");
    repo.git(&["add", "--", "keep.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    repo.write("dir/one.txt", "one\n");
    repo.write("dir/sub/two.txt", "two\n");
    repo.git(&["add", "--", "dir"]);
    repo.git(&["commit", "-q", "-m", "add a directory"]);
    let added_dir = repo.head().to_string();
    let changes = commit_changes(&repo, &added_dir);
    assert_eq!(
        changes,
        vec![added("dir/one.txt"), added("dir/sub/two.txt")]
    );
    assert_eq!(changes, git_commit_changes(&repo, &added_dir));

    repo.git(&["rm", "-r", "-q", "dir"]);
    repo.git(&["commit", "-q", "-m", "remove a directory"]);
    let removed_dir = repo.head().to_string();
    let changes = commit_changes(&repo, &removed_dir);
    assert_eq!(
        changes,
        vec![deleted("dir/one.txt"), deleted("dir/sub/two.txt")]
    );
    assert_eq!(changes, git_commit_changes(&repo, &removed_dir));
}

#[test]
fn commit_changes_for_file_directory_replacements() {
    let repo = TestRepo::new();
    repo.write("thing", "a file, before the change\n");
    repo.git(&["add", "--", "thing"]);
    repo.git(&["commit", "-q", "-m", "file"]);

    // The content is unrelated, so rename detection cannot pair the two sides.
    fs::remove_file(repo.path().join("thing")).expect("remove the file");
    repo.write("thing/inside.txt", "unrelated contents entirely\n");
    repo.git(&["add", "-A", "--", "thing"]);
    repo.git(&["commit", "-q", "-m", "file becomes directory"]);
    let to_directory = repo.head().to_string();
    let changes = commit_changes(&repo, &to_directory);
    assert_eq!(
        changes,
        vec![deleted("thing"), added("thing/inside.txt")],
        "git reports the file as deleted and the new file as added"
    );
    assert_eq!(changes, git_commit_changes(&repo, &to_directory));

    repo.git(&["rm", "-r", "-q", "thing"]);
    repo.write("thing", "now a plain file again\n");
    repo.git(&["add", "--", "thing"]);
    repo.git(&["commit", "-q", "-m", "directory becomes file"]);
    let to_file = repo.head().to_string();
    let changes = commit_changes(&repo, &to_file);
    assert_eq!(changes, vec![added("thing"), deleted("thing/inside.txt")]);
    assert_eq!(changes, git_commit_changes(&repo, &to_file));
}

#[test]
fn commit_changes_for_gitlink_changes() {
    let repo = TestRepo::new();
    repo.write("file.txt", "file\n");
    repo.git(&["add", "--", "file.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    // A gitlink is a tree entry of its own; it is not a directory to recurse
    // into, and the object it names does not have to exist here.
    let first = "1111111111111111111111111111111111111111";
    let second = "2222222222222222222222222222222222222222";
    repo.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{first},sub"),
    ]);
    repo.git(&["commit", "-q", "-m", "add a gitlink"]);
    let added_link = repo.head().to_string();
    assert_eq!(commit_changes(&repo, &added_link), vec![added("sub")]);
    assert_eq!(
        commit_changes(&repo, &added_link),
        git_commit_changes(&repo, &added_link)
    );

    repo.git(&[
        "update-index",
        "--cacheinfo",
        &format!("160000,{second},sub"),
    ]);
    repo.git(&["commit", "-q", "-m", "move the gitlink"]);
    let moved_link = repo.head().to_string();
    assert_eq!(commit_changes(&repo, &moved_link), vec![modified("sub")]);
    assert_eq!(
        commit_changes(&repo, &moved_link),
        git_commit_changes(&repo, &moved_link)
    );

    repo.git(&["update-index", "--force-remove", "sub"]);
    repo.write("sub", "a regular file where the gitlink was\n");
    repo.git(&["add", "--", "sub"]);
    repo.git(&["commit", "-q", "-m", "gitlink becomes a file"]);
    let replaced = repo.head().to_string();
    assert_eq!(commit_changes(&repo, &replaced), vec![type_changed("sub")]);
    assert_eq!(
        commit_changes(&repo, &replaced),
        git_commit_changes(&repo, &replaced)
    );
}

#[cfg(unix)]
#[test]
fn commit_changes_for_a_mode_change_is_a_modification() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new();
    repo.write("run.sh", "#!/bin/sh\necho hi\n");
    repo.git(&["add", "--", "run.sh"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let file = repo.path().join("run.sh");
    let mut permissions = fs::metadata(&file).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&file, permissions).expect("chmod");
    repo.git(&["add", "--", "run.sh"]);
    repo.git(&["commit", "-q", "-m", "make executable"]);

    let head = repo.head().to_string();
    let changes = commit_changes(&repo, &head);
    assert_eq!(changes, vec![modified("run.sh")]);
    assert_eq!(changes, git_commit_changes(&repo, &head));
}

#[cfg(unix)]
#[test]
fn commit_changes_for_symlinks() {
    let repo = TestRepo::new();
    repo.write("target.txt", "content\n");
    repo.write("other.txt", "other\n");
    repo.write("link", "a plain file\n");
    repo.git(&["add", "--", "target.txt", "other.txt", "link"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    // A regular file replaced by a symlink is a type change, as is the reverse.
    fs::remove_file(repo.path().join("link")).expect("remove the file");
    std::os::unix::fs::symlink("target.txt", repo.path().join("link")).expect("symlink");
    repo.git(&["add", "--", "link"]);
    repo.git(&["commit", "-q", "-m", "link becomes a symlink"]);
    let to_link = repo.head().to_string();
    assert_eq!(commit_changes(&repo, &to_link), vec![type_changed("link")]);
    assert_eq!(
        commit_changes(&repo, &to_link),
        git_commit_changes(&repo, &to_link)
    );

    // Retargeting a symlink keeps its type, so it is a content modification.
    fs::remove_file(repo.path().join("link")).expect("remove the symlink");
    std::os::unix::fs::symlink("other.txt", repo.path().join("link")).expect("symlink");
    repo.git(&["add", "--", "link"]);
    repo.git(&["commit", "-q", "-m", "retarget the symlink"]);
    let retargeted = repo.head().to_string();
    assert_eq!(commit_changes(&repo, &retargeted), vec![modified("link")]);
    assert_eq!(
        commit_changes(&repo, &retargeted),
        git_commit_changes(&repo, &retargeted)
    );

    fs::remove_file(repo.path().join("link")).expect("remove the symlink");
    repo.write("link", "back to a plain file\n");
    repo.git(&["add", "--", "link"]);
    repo.git(&["commit", "-q", "-m", "link is a file again"]);
    let to_file = repo.head().to_string();
    assert_eq!(commit_changes(&repo, &to_file), vec![type_changed("link")]);
    assert_eq!(
        commit_changes(&repo, &to_file),
        git_commit_changes(&repo, &to_file)
    );
}

#[test]
fn commit_changes_for_a_merge_is_the_first_parent_diff() {
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    repo.git(&["add", "--", "base.txt"]);
    repo.git(&["commit", "-q", "-m", "base"]);

    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("side.txt", "side\n");
    repo.git(&["add", "--", "side.txt"]);
    repo.git(&["commit", "-q", "-m", "side"]);

    repo.git(&["checkout", "-q", "main"]);
    repo.write("main.txt", "main\n");
    repo.git(&["add", "--", "main.txt"]);
    repo.git(&["commit", "-q", "-m", "main"]);
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);

    let merge = repo.head().to_string();
    let changes = commit_changes(&repo, &merge);
    assert_eq!(
        changes,
        vec![added("side.txt")],
        "a merge is diffed against its first parent only"
    );
    assert_eq!(changes, git_commit_changes(&repo, &merge));
}

#[test]
fn merge_conflict_is_reported_as_conflicted() {
    let repo = TestRepo::new();
    repo.write("conflict.txt", "base\n");
    repo.git(&["add", "--", "conflict.txt"]);
    repo.git(&["commit", "-q", "-m", "base"]);

    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("conflict.txt", "theirs\n");
    repo.git(&["commit", "-q", "-a", "-m", "theirs"]);

    repo.git(&["checkout", "-q", "main"]);
    repo.write("conflict.txt", "ours\n");
    repo.git(&["commit", "-q", "-a", "-m", "ours"]);

    let merge = repo.git_output(&["merge", "side"]);
    assert!(!merge.status.success(), "merge should conflict");

    let status = repo.open().status().expect("status");
    assert!(status.staged.is_empty());
    assert_eq!(
        status.unstaged,
        vec![FileChange {
            path: "conflict.txt".to_owned(),
            old_path: None,
            kind: ChangeKind::Conflicted,
        }]
    );
    // `git status --porcelain=v2` reports unmerged paths with `u` lines.
    assert_matches_git(&repo);
}

#[test]
fn file_contents_from_commit_index_and_worktree() {
    let repo = TestRepo::new();
    repo.write("dir/file.txt", "committed\n");
    repo.git(&["add", "--", "dir/file.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);
    let commit = repo.head();

    repo.write("dir/file.txt", "staged\n");
    repo.git(&["add", "--", "dir/file.txt"]);
    repo.write("dir/file.txt", "worktree\n");

    let repo = repo.open();
    assert_eq!(
        repo.file_at_commit(commit, "dir/file.txt")
            .expect("commit contents")
            .as_deref(),
        Some(b"committed\n".as_slice())
    );
    assert_eq!(
        repo.file_in_index("dir/file.txt")
            .expect("index contents")
            .as_deref(),
        Some(b"staged\n".as_slice())
    );
    assert_eq!(
        repo.file_in_worktree("dir/file.txt")
            .expect("worktree contents")
            .as_deref(),
        Some(b"worktree\n".as_slice())
    );
    // Without any eol configuration the git form is the raw content.
    assert_eq!(
        repo.file_in_worktree_as_git("dir/file.txt")
            .expect("worktree contents")
            .as_deref(),
        Some(b"worktree\n".as_slice())
    );

    assert_eq!(
        repo.file_at_commit(commit, "missing.txt").expect("missing"),
        None
    );
    assert_eq!(repo.file_in_index("missing.txt").expect("missing"), None);
    assert_eq!(repo.file_in_worktree("missing.txt").expect("missing"), None);
    assert_eq!(
        repo.file_in_worktree_as_git("missing.txt")
            .expect("missing"),
        None
    );
    // Directories are not files.
    assert_eq!(repo.file_in_worktree("dir").expect("directory"), None);
    assert_eq!(
        repo.file_in_worktree_as_git("dir").expect("directory"),
        None
    );
}

/// A checkout with `core.autocrlf=true` holds CRLF while the index holds LF.
/// Reading the raw bytes must stay possible (a discard writes them back), and
/// both git and `status()` must see the file as unchanged.
#[test]
fn autocrlf_checkout_with_crlf_matches_lf_index() {
    let repo = TestRepo::new();
    repo.git(&["config", "core.autocrlf", "true"]);
    repo.write("a.txt", "one\ntwo\n");
    repo.git(&["add", "--", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    // `git checkout` applies the smudge filter, which is what puts CRLF on disk
    // on Windows. Backdating the result forces git (and gix) to compare content
    // instead of trusting stat data.
    checkout_crlf(&repo, "a.txt");

    let open = repo.open();
    assert_eq!(
        open.file_in_worktree("a.txt")
            .expect("raw contents")
            .as_deref(),
        Some(b"one\r\ntwo\r\n".as_slice()),
        "file_in_worktree keeps the bytes on disk"
    );
    let index = open.file_in_index("a.txt").expect("index").expect("entry");
    assert_eq!(index, b"one\ntwo\n");
    assert_eq!(
        open.file_in_worktree_as_git("a.txt")
            .expect("git form")
            .expect("entry"),
        index
    );
    assert_eq!(open.status().expect("status"), WorktreeStatus::default());
    assert_matches_git(&repo);
}

#[test]
fn autocrlf_one_changed_crlf_line_is_one_diff_pair() {
    let repo = TestRepo::new();
    repo.git(&["config", "core.autocrlf", "true"]);
    let base: String = (1..=5).map(|i| format!("line {i}\n")).collect();
    repo.write("a.txt", &base);
    repo.git(&["add", "--", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);
    checkout_crlf(&repo, "a.txt");

    // Same length as before, so only a content comparison can notice it.
    let edited = base.replace("line 3", "line X").replace('\n', "\r\n");
    repo.write("a.txt", &edited);

    let open = repo.open();
    let index = open.file_in_index("a.txt").expect("index").expect("entry");
    let git_form = open
        .file_in_worktree_as_git("a.txt")
        .expect("git form")
        .expect("entry");

    let diff = diff_text(&index, &git_form, &DiffOptions::default());
    assert_eq!(diff.hunks.len(), 1, "hunks: {diff:#?}");
    let changed: Vec<&str> = diff
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind != LineKind::Context)
        .map(|line| line.content.as_str())
        .collect();
    assert_eq!(changed, vec!["line 3", "line X"], "{diff:#?}");

    assert_eq!(
        open.status().expect("status").unstaged,
        vec![modified("a.txt")]
    );
    assert_matches_git(&repo);
}

/// A `text eol=crlf` attribute gives the same LF index / CRLF checkout split as
/// `core.autocrlf`, without any configuration.
#[test]
fn gitattributes_text_eol_crlf_matches_lf_index() {
    let repo = TestRepo::new();
    repo.write(".gitattributes", "*.txt text eol=crlf\n");
    repo.write("a.txt", "one\ntwo\n");
    repo.git(&["add", "--", ".gitattributes", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);
    checkout_crlf(&repo, "a.txt");

    let open = repo.open();
    assert_eq!(
        open.file_in_worktree("a.txt")
            .expect("raw contents")
            .as_deref(),
        Some(b"one\r\ntwo\r\n".as_slice())
    );
    assert_eq!(
        open.file_in_worktree_as_git("a.txt")
            .expect("git form")
            .expect("entry"),
        b"one\ntwo\n"
    );
    assert_eq!(open.status().expect("status"), WorktreeStatus::default());
    assert_matches_git(&repo);
}

/// `binary` turns off every conversion, so the CRLF bytes on disk are the bytes
/// in the index.
#[test]
fn gitattributes_binary_keeps_bytes_unchanged() {
    let repo = TestRepo::new();
    repo.write(".gitattributes", "*.bin binary\n");
    let content: &[u8] = b"\x00\x01\r\nbinary\r\n";
    fs::write(repo.path().join("data.bin"), content).expect("write binary");
    repo.git(&["add", "--", ".gitattributes", "data.bin"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let open = repo.open();
    assert_eq!(
        open.file_in_index("data.bin")
            .expect("index")
            .expect("entry"),
        content
    );
    assert_eq!(
        open.file_in_worktree_as_git("data.bin")
            .expect("git form")
            .expect("entry"),
        content
    );
    assert_eq!(open.status().expect("status"), WorktreeStatus::default());
    assert_matches_git(&repo);
}

/// A `-text` attribute also disables eol conversion, for files that are text but
/// must keep their bytes.
#[test]
fn gitattributes_minus_text_keeps_bytes_unchanged() {
    let repo = TestRepo::new();
    repo.write(".gitattributes", "*.txt -text\n");
    repo.write("a.txt", "one\r\ntwo\r\n");
    repo.git(&["add", "--", ".gitattributes", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let open = repo.open();
    assert_eq!(
        open.file_in_worktree_as_git("a.txt")
            .expect("git form")
            .expect("entry"),
        b"one\r\ntwo\r\n"
    );
    assert_eq!(open.status().expect("status"), WorktreeStatus::default());
    assert_matches_git(&repo);
}

/// A conversion gix refuses (here `core.safecrlf=true` on a mixed-ending file)
/// must not fail the read: the raw bytes come back so the diff still renders.
#[test]
fn failed_conversion_falls_back_to_raw_bytes() {
    let repo = TestRepo::new();
    repo.git(&["config", "core.autocrlf", "true"]);
    repo.write("a.txt", "one\ntwo\n");
    repo.git(&["add", "--", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);
    // A lone LF would come back as CRLF on checkout, which `safecrlf` rejects.
    repo.git(&["config", "core.safecrlf", "true"]);
    let raw = b"one\r\ntwo\n";
    fs::write(repo.path().join("a.txt"), raw).expect("write mixed endings");

    let open = repo.open();
    assert_eq!(
        open.file_in_worktree_as_git("a.txt")
            .expect("fallback")
            .as_deref(),
        Some(raw.as_slice())
    );
}

#[test]
fn incremental_edit_and_delete_tracked_file_and_revert_clean() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.write("b.txt", "keep\n");
    repo.git(&["add", "--", "a.txt", "b.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let open = repo.open();
    let mut prev = open.status().expect("baseline");

    repo.write("a.txt", "two\n");
    let next = open
        .status_incremental(&prev, &[repo.path().join("a.txt")])
        .expect("incremental edit")
        .expect("must not fall back");
    assert_eq!(next.unstaged, vec![modified("a.txt")]);
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    assert_matches_git(&repo);
    prev = next;

    // Reverting to original bytes makes the file clean again.
    repo.write("a.txt", "one\n");
    let next = open
        .status_incremental(&prev, &[repo.path().join("a.txt")])
        .expect("incremental revert")
        .expect("must not fall back");
    assert_eq!(next, WorktreeStatus::default());
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    prev = next;

    // Deleting a tracked file and restoring it.
    fs::remove_file(repo.path().join("b.txt")).expect("remove b.txt");
    let next = open
        .status_incremental(&prev, &[repo.path().join("b.txt")])
        .expect("incremental delete")
        .expect("must not fall back");
    assert_eq!(next.unstaged, vec![deleted("b.txt")]);
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    prev = next;

    repo.write("b.txt", "keep\n");
    let next = open
        .status_incremental(&prev, &[repo.path().join("b.txt")])
        .expect("incremental restore")
        .expect("must not fall back");
    assert_eq!(next, WorktreeStatus::default());
    open.assert_incremental_matches_full(&next)
        .expect("matches");
}

#[test]
fn incremental_create_delete_and_rename_untracked_file() {
    let repo = TestRepo::new();
    repo.write("tracked.txt", "base\n");
    repo.git(&["add", "--", "tracked.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let open = repo.open();
    let mut prev = open.status().expect("baseline");

    repo.write("uno.txt", "uno\n");
    let next = open
        .status_incremental(&prev, &[repo.path().join("uno.txt")])
        .expect("incremental create")
        .expect("must not fall back");
    assert_eq!(next.unstaged, vec![untracked("uno.txt")]);
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    prev = next;

    fs::rename(repo.path().join("uno.txt"), repo.path().join("due.txt")).expect("rename");
    let next = open
        .status_incremental(
            &prev,
            &[repo.path().join("uno.txt"), repo.path().join("due.txt")],
        )
        .expect("incremental rename")
        .expect("must not fall back");
    assert_eq!(next.unstaged, vec![untracked("due.txt")]);
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    prev = next;

    fs::remove_file(repo.path().join("due.txt")).expect("remove");
    let next = open
        .status_incremental(&prev, &[repo.path().join("due.txt")])
        .expect("incremental delete")
        .expect("must not fall back");
    assert_eq!(next, WorktreeStatus::default());
    open.assert_incremental_matches_full(&next)
        .expect("matches");
}

#[test]
fn incremental_untracked_directory_with_files() {
    let repo = TestRepo::new();
    repo.write("tracked.txt", "base\n");
    repo.git(&["add", "--", "tracked.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let open = repo.open();
    let mut prev = open.status().expect("baseline");

    // Reporting only the newly created directory must still discover all files inside.
    repo.write("dir/a.txt", "a\n");
    repo.write("dir/sub/b.txt", "b\n");
    let next = open
        .status_incremental(&prev, &[repo.path().join("dir")])
        .expect("incremental dir create")
        .expect("must not fall back");
    assert_eq!(
        next.unstaged,
        vec![untracked("dir/a.txt"), untracked("dir/sub/b.txt")]
    );
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    prev = next;

    // Renaming the whole directory.
    fs::rename(repo.path().join("dir"), repo.path().join("dir2")).expect("rename dir");
    let next = open
        .status_incremental(&prev, &[repo.path().join("dir"), repo.path().join("dir2")])
        .expect("incremental dir rename")
        .expect("must not fall back");
    assert_eq!(
        next.unstaged,
        vec![untracked("dir2/a.txt"), untracked("dir2/sub/b.txt")]
    );
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    prev = next;

    // Removing the directory clears all child untracked entries.
    fs::remove_dir_all(repo.path().join("dir2")).expect("remove dir2");
    let next = open
        .status_incremental(&prev, &[repo.path().join("dir2")])
        .expect("incremental dir remove")
        .expect("must not fall back");
    assert_eq!(next, WorktreeStatus::default());
    open.assert_incremental_matches_full(&next)
        .expect("matches");
}

#[test]
fn incremental_ignored_files_and_rule_fallbacks() {
    let repo = TestRepo::new();
    repo.write(".gitignore", "*.log\nignored_dir/\n");
    repo.write("tracked.txt", "base\n");
    repo.git(&["add", "--", ".gitignore", "tracked.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let open = repo.open();
    let prev = open.status().expect("baseline");

    repo.write("app.log", "ignored\n");
    repo.write("ignored_dir/out.bin", "ignored\n");
    let next = open
        .status_incremental(
            &prev,
            &[
                repo.path().join("app.log"),
                repo.path().join("ignored_dir/out.bin"),
            ],
        )
        .expect("incremental ignored")
        .expect("must not fall back");
    assert_eq!(next, WorktreeStatus::default());
    open.assert_incremental_matches_full(&next)
        .expect("matches");

    // Any rules / exclude / git-internal / root path forces a full scan (`None`).
    repo.write(".gitignore", "*.log\nignored_dir/\n*.tmp\n");
    assert!(
        open.status_incremental(&prev, &[repo.path().join(".gitignore")])
            .expect("gitignore")
            .is_none()
    );
    assert!(
        open.status_incremental(&prev, &[repo.path().join("sub/.gitignore")])
            .expect("nested gitignore")
            .is_none()
    );
    assert!(
        open.status_incremental(&prev, &[repo.path().join(".gitattributes")])
            .expect("gitattributes")
            .is_none()
    );
    assert!(
        open.status_incremental(&prev, &[repo.path().join(".gitmodules")])
            .expect("gitmodules")
            .is_none()
    );
    assert!(
        open.status_incremental(&prev, &[repo.path().join(".git/info/exclude")])
            .expect("info/exclude")
            .is_none()
    );
    assert!(
        open.status_incremental(&prev, &[repo.path().join(".git/index")])
            .expect("git index")
            .is_none()
    );
    assert!(
        open.status_incremental(&prev, &[repo.path().to_path_buf()])
            .expect("workdir root")
            .is_none()
    );
}

#[test]
fn incremental_tracked_dir_delete_and_file_dir_replacements() {
    let repo = TestRepo::new();
    repo.write("swap_f", "file\n");
    repo.write("swap_d/nested.txt", "nested\n");
    repo.write("gone_d/a.txt", "a\n");
    repo.write("gone_d/sub/b.txt", "b\n");
    repo.git(&["add", "--", "swap_f", "swap_d/nested.txt", "gone_d"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let open = repo.open();
    let mut prev = open.status().expect("baseline");

    // Replacing a tracked file with a directory (reported only via child path).
    fs::remove_file(repo.path().join("swap_f")).expect("remove swap_f");
    repo.write("swap_f/inside.txt", "inside\n");
    let next = open
        .status_incremental(&prev, &[repo.path().join("swap_f/inside.txt")])
        .expect("file to dir")
        .expect("must not fall back");
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    assert_matches_git(&repo);
    prev = next;

    // Replacing a tracked directory with a plain file.
    fs::remove_dir_all(repo.path().join("swap_d")).expect("remove swap_d");
    repo.write("swap_d", "now a file\n");
    let next = open
        .status_incremental(&prev, &[repo.path().join("swap_d")])
        .expect("dir to file")
        .expect("must not fall back");
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    assert_matches_git(&repo);
    prev = next;

    // Removing a whole tracked directory.
    fs::remove_dir_all(repo.path().join("gone_d")).expect("remove gone_d");
    let next = open
        .status_incremental(&prev, &[repo.path().join("gone_d")])
        .expect("remove tracked dir")
        .expect("must not fall back");
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    assert_matches_git(&repo);
}

#[test]
fn incremental_mixed_burst_and_special_characters() {
    let repo = TestRepo::new();
    repo.write(".gitignore", "*.log\n");
    repo.write("staged.txt", "v1\n");
    repo.write("edited.txt", "v1\n");
    repo.write("deleted.txt", "bye\n");
    repo.write("clean.txt", "same\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "init"]);

    // Partially staged change must be preserved across incremental updates.
    repo.write("staged.txt", "v2\n");
    repo.git(&["add", "--", "staged.txt"]);
    repo.write("temp.txt", "temp\n");

    let open = repo.open();
    let prev = open.status().expect("baseline");

    // Mixed burst: edit tracked, delete tracked, edit staged worktree, remove untracked,
    // add untracked with special glob chars, add untracked dir, add ignored file.
    repo.write("edited.txt", "v2\n");
    fs::remove_file(repo.path().join("deleted.txt")).expect("remove deleted.txt");
    repo.write("staged.txt", "v3\n");
    fs::remove_file(repo.path().join("temp.txt")).expect("remove temp.txt");
    repo.write("weird[1].txt", "glob-like\n");
    repo.write("newdir/one.txt", "one\n");
    repo.write("newdir/sub/two.txt", "two\n");
    repo.write("debug.log", "ignored\n");

    let changed = vec![
        repo.path().join("edited.txt"),
        repo.path().join("deleted.txt"),
        repo.path().join("staged.txt"),
        repo.path().join("temp.txt"),
        repo.path().join("weird[1].txt"),
        repo.path().join("newdir"),
        repo.path().join("newdir/one.txt"),
        repo.path().join("newdir/sub/two.txt"),
        repo.path().join("debug.log"),
    ];
    let next = open
        .status_incremental(&prev, &changed)
        .expect("mixed burst")
        .expect("must not fall back");
    assert_eq!(next.staged, vec![modified("staged.txt")]);
    open.assert_incremental_matches_full(&next)
        .expect("matches");
    assert_matches_git(&repo);
}

#[test]
fn incremental_falls_back_when_change_set_or_directory_exceeds_limit() {
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    repo.git(&["add", "--", "base.txt"]);
    repo.git(&["commit", "-q", "-m", "init"]);
    let open = repo.open();
    let prev = open.status().expect("baseline");

    // Over MAX_INCREMENTAL_PATHS distinct input paths.
    let many: Vec<PathBuf> = (0..=strata_core::repo::MAX_INCREMENTAL_PATHS)
        .map(|i| repo.path().join(format!("f{i}.txt")))
        .collect();
    assert!(
        open.status_incremental(&prev, &many)
            .expect("many paths")
            .is_none()
    );

    // Directory with over MAX_INCREMENTAL_PATHS untracked files inside.
    let big_dir = repo.path().join("big_untracked");
    fs::create_dir_all(&big_dir).expect("create big_untracked");
    for i in 0..=strata_core::repo::MAX_INCREMENTAL_PATHS {
        fs::write(big_dir.join(format!("u{i}.txt")), "u").expect("write untracked");
    }
    assert!(
        open.status_incremental(&prev, &[big_dir])
            .expect("big untracked dir")
            .is_none()
    );
}

#[test]
#[ignore = "perf: reads STRATA_BENCH_REPO"]
fn bench_status_incremental_on_repo() {
    use std::time::Instant;

    let Some(repo_path) = std::env::var_os("STRATA_BENCH_REPO")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    else {
        eprintln!("STRATA_BENCH_REPO is unset; skipping");
        return;
    };
    let target = std::env::var("STRATA_BENCH_PATH").unwrap_or_else(|_| "README".into());
    let targets: Vec<&str> = target.split(',').filter(|s| !s.is_empty()).collect();
    let repo = Repo::open(&repo_path).expect("open benchmark repo");

    // Warm up OS file cache and verify baseline.
    let baseline = repo.status().expect("warmup status");
    let mut full_times = Vec::with_capacity(5);
    let mut full_status = baseline.clone();
    for _ in 0..5 {
        let start = Instant::now();
        full_status = repo.status().expect("full status");
        full_times.push(start.elapsed());
    }

    // Simulate the pre-edit snapshot where the target path was still clean.
    let mut pre_edit = baseline.clone();
    pre_edit
        .unstaged
        .retain(|change| !targets.contains(&change.path.as_str()));

    let target_paths: Vec<PathBuf> = targets.iter().map(|p| repo_path.join(p)).collect();
    let mut inc_times = Vec::with_capacity(10);
    let mut inc_status = None;
    for _ in 0..10 {
        let start = Instant::now();
        inc_status = repo
            .status_incremental(&pre_edit, &target_paths)
            .expect("incremental status");
        inc_times.push(start.elapsed());
    }
    let inc_status = inc_status.expect("incremental path should not fall back");
    assert_eq!(inc_status, full_status, "incremental must match full scan");

    full_times.sort();
    inc_times.sort();
    eprintln!(
        "repo={} target={} changes={} | full_status median={:.2} ms (min={:.2} ms) | incremental median={:.2} ms (min={:.2} ms)",
        repo_path.display(),
        target,
        full_status.unstaged.len(),
        full_times[full_times.len() / 2].as_secs_f64() * 1000.0,
        full_times[0].as_secs_f64() * 1000.0,
        inc_times[inc_times.len() / 2].as_secs_f64() * 1000.0,
        inc_times[0].as_secs_f64() * 1000.0,
    );
}
