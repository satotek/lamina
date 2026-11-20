//! Integration tests for discarding staged and worktree changes in one step.
//!
//! Every repository lives in a `TempDir` and runs with an empty global/system
//! config plus a fixed identity, so the machine's git settings cannot leak in.

use std::fs;
use std::path::{Path, PathBuf};

use lamina_core::git_cli::{CommitOptions, Git};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let config = root.path().join("gitconfig");
        fs::write(&config, b"").expect("empty git config");
        Self { root, config }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn git(&self) -> Git {
        let dir = self.path().join("repo");
        fs::create_dir(&dir).expect("repo dir");
        let git = self.git_in(&dir);
        git.run(&["init", "-b", "main"], None).expect("git init");
        git
    }

    fn git_in(&self, workdir: impl AsRef<Path>) -> Git {
        Git::new(workdir.as_ref().to_path_buf())
            .with_env("GIT_CONFIG_GLOBAL", self.config.clone().into_os_string())
            .with_env("GIT_CONFIG_SYSTEM", self.config.clone().into_os_string())
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_AUTHOR_NAME", "Test Author")
            .with_env("GIT_AUTHOR_EMAIL", "test@example.com")
            .with_env("GIT_COMMITTER_NAME", "Test Author")
            .with_env("GIT_COMMITTER_EMAIL", "test@example.com")
    }
}

fn write_file(git: &Git, name: &str, contents: &str) {
    let path = git.workdir().join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dir");
    }
    fs::write(path, contents).expect("write file");
}

fn read_file(git: &Git, name: &str) -> String {
    fs::read_to_string(git.workdir().join(name)).expect("read file")
}

fn stdout(git: &Git, args: &[&str]) -> String {
    String::from_utf8(git.run(args, None).expect("git command").stdout).expect("utf-8 output")
}

fn status(git: &Git) -> String {
    stdout(git, &["status", "--porcelain"])
}

fn commit_file(git: &Git, name: &str, contents: &str, message: &str) {
    write_file(git, name, contents);
    git.stage_paths(&[name]).expect("stage");
    git.commit(message, CommitOptions::default())
        .expect("commit");
}

#[test]
fn discard_staged_restores_a_modified_and_staged_file() {
    let fixture = Fixture::new();
    let git = fixture.git();
    commit_file(&git, "tracked.txt", "one\n", "initial");

    write_file(&git, "tracked.txt", "two\n");
    git.stage_paths(&["tracked.txt"]).expect("stage");
    assert_eq!(status(&git).trim(), "M  tracked.txt");

    git.discard_staged_paths(&["tracked.txt"])
        .expect("discard staged");

    assert_eq!(read_file(&git, "tracked.txt"), "one\n");
    assert!(status(&git).trim().is_empty(), "{}", status(&git));
}

#[test]
fn discard_staged_removes_a_file_added_to_the_index() {
    let fixture = Fixture::new();
    let git = fixture.git();
    commit_file(&git, "tracked.txt", "one\n", "initial");

    write_file(&git, "nested/new.txt", "new\n");
    git.stage_paths(&["nested/new.txt"]).expect("stage");
    assert_eq!(status(&git).trim(), "A  nested/new.txt");

    // HEAD has no content for the file, so restoring cannot apply: it must be
    // dropped from the index and deleted from the worktree instead.
    git.discard_staged_paths(&["nested/new.txt"])
        .expect("discard staged");

    assert!(
        !git.workdir().join("nested/new.txt").exists(),
        "the added file must be deleted"
    );
    assert!(status(&git).trim().is_empty(), "{}", status(&git));
}

#[test]
fn discard_staged_restores_staged_and_unstaged_edits_together() {
    let fixture = Fixture::new();
    let git = fixture.git();
    commit_file(&git, "tracked.txt", "one\n", "initial");

    write_file(&git, "tracked.txt", "two\n");
    git.stage_paths(&["tracked.txt"]).expect("stage");
    write_file(&git, "tracked.txt", "three\n");
    assert_eq!(status(&git).trim(), "MM tracked.txt");

    git.discard_staged_paths(&["tracked.txt"])
        .expect("discard staged");

    assert_eq!(read_file(&git, "tracked.txt"), "one\n");
    assert!(status(&git).trim().is_empty(), "{}", status(&git));
}

#[test]
fn discard_staged_batches_long_path_lists() {
    let fixture = Fixture::new();
    let git = fixture.git();

    let mut names = Vec::new();
    for index in 0..120 {
        let name = format!("files/{index:03}.txt");
        write_file(&git, &name, "base\n");
        names.push(name);
    }
    git.stage_paths(&names).expect("stage");
    git.commit("initial", CommitOptions::default())
        .expect("commit");

    // Half the files get a staged edit, half are staged additions: a long list
    // crosses the inline-pathspec limit and exercises both branches per chunk.
    let mut changed = Vec::new();
    for (index, name) in names.iter().enumerate() {
        if index % 2 == 0 {
            write_file(&git, name, "changed\n");
            changed.push(name.clone());
        } else {
            let added = format!("added/{index:03}.txt");
            write_file(&git, &added, "added\n");
            changed.push(added);
        }
    }
    git.stage_paths(&changed).expect("stage changes");
    assert_eq!(status(&git).lines().count(), 120);

    git.discard_staged_paths(&changed).expect("discard staged");

    assert!(status(&git).trim().is_empty(), "{}", status(&git));
    for name in names.iter().step_by(2) {
        assert_eq!(read_file(&git, name), "base\n", "{name}");
    }
    for index in (1..120).step_by(2) {
        assert!(
            !git.workdir().join(format!("added/{index:03}.txt")).exists(),
            "added file {index} must be deleted"
        );
    }
}

#[test]
fn discard_staged_on_an_unborn_branch_deletes_the_staged_files() {
    let fixture = Fixture::new();
    let git = fixture.git();

    write_file(&git, "first.txt", "one\n");
    git.stage_paths(&["first.txt"]).expect("stage");
    assert_eq!(status(&git).trim(), "A  first.txt");

    // There is no HEAD to restore from, so every staged path is treated as an
    // addition: the index is emptied and the worktree copies are deleted.
    git.discard_staged_paths(&["first.txt"])
        .expect("discard staged");

    assert!(!git.workdir().join("first.txt").exists());
    assert!(status(&git).trim().is_empty(), "{}", status(&git));
}
