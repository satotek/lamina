//! Integration tests for stashing a subset of paths and for the stash metadata
//! the snapshot carries.

use std::fs;

use lamina_core::git_cli::{CommitOptions, Git};
use lamina_core::test_support::TestRepo;

/// The timestamp every fixture command runs with, so a stash's creation time
/// is a fixed value instead of the machine's clock.
const BASE_TIME: i64 = 1_577_836_800;

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
fn stash_paths_stashes_only_the_given_files() {
    let fixture = TestRepo::with_time(BASE_TIME);
    let git = fixture.git_handle();
    commit_file(&git, "a.txt", "base\n", "initial");
    commit_file(&git, "b.txt", "base\n", "second");

    write_file(&git, "a.txt", "changed\n");
    write_file(&git, "b.txt", "changed\n");
    git.stage_paths(&["a.txt"]).expect("stage a");

    git.stash_push_paths(Some("only a"), false, &["a.txt"])
        .expect("stash a");

    // The stashed path is back at HEAD in both index and worktree; the other
    // worktree change is untouched.
    assert_eq!(read_file(&git, "a.txt"), "base\n");
    assert_eq!(read_file(&git, "b.txt"), "changed\n");
    assert_eq!(status(&git).trim_end(), " M b.txt");

    let entries = git.stash_list().expect("stash list");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].message.contains("only a"), "{:?}", entries[0]);
    assert_eq!(
        stdout(&git, &["stash", "show", "--name-only", "stash@{0}"])
            .lines()
            .collect::<Vec<_>>(),
        vec!["a.txt"]
    );
}

#[test]
fn stash_paths_handles_long_pathspec_lists() {
    let fixture = TestRepo::with_time(BASE_TIME);
    let git = fixture.git_handle();

    let mut names = Vec::new();
    for index in 0..120 {
        let name = format!("files/{index:03}.txt");
        write_file(&git, &name, "base\n");
        names.push(name);
    }
    git.stage_paths(&names).expect("stage");
    git.commit("initial", CommitOptions::default())
        .expect("commit");

    for name in &names {
        write_file(&git, name, "changed\n");
    }
    assert_eq!(status(&git).lines().count(), 120);

    // The list crosses the inline-pathspec limit, so it goes through a
    // pathspec file.
    git.stash_push_paths(Some("all files"), false, &names)
        .expect("stash many");

    assert!(status(&git).trim().is_empty(), "{}", status(&git));
    assert_eq!(
        stdout(&git, &["stash", "show", "--name-only", "stash@{0}"])
            .lines()
            .count(),
        120
    );
}

#[test]
fn stash_info_reports_the_creation_time_and_branch() {
    let fixture = TestRepo::with_time(BASE_TIME);
    let git = fixture.git_handle();
    commit_file(&git, "a.txt", "base\n", "initial");
    git.create_branch("feature", Some("main"), true)
        .expect("checkout feature");

    write_file(&git, "a.txt", "changed\n");
    git.stash_push(Some("work in progress"), false)
        .expect("stash");

    let stashes = fixture.open().stashes().expect("stashes");
    assert_eq!(stashes.len(), 1);
    assert_eq!(stashes[0].created_time, BASE_TIME);
    assert_eq!(stashes[0].branch.as_deref(), Some("feature"));
    assert!(stashes[0].message.contains("work in progress"));
}

#[test]
fn stash_info_parses_the_branch_of_a_plain_stash_message() {
    let fixture = TestRepo::with_time(BASE_TIME);
    let git = fixture.git_handle();
    commit_file(&git, "a.txt", "base\n", "initial");

    write_file(&git, "a.txt", "changed\n");
    git.stash_push(None, false).expect("stash");

    let stashes = fixture.open().stashes().expect("stashes");
    assert_eq!(stashes[0].branch.as_deref(), Some("main"));
    assert!(
        stashes[0].message.starts_with("WIP on main:"),
        "{:?}",
        stashes[0]
    );
}

#[test]
fn stash_info_reports_no_branch_when_head_is_detached() {
    let fixture = TestRepo::with_time(BASE_TIME);
    let git = fixture.git_handle();
    commit_file(&git, "a.txt", "base\n", "initial");
    git.run(&["checkout", "--detach"], None)
        .expect("detach HEAD");

    write_file(&git, "a.txt", "changed\n");
    git.stash_push(None, false).expect("stash");

    let stashes = fixture.open().stashes().expect("stashes");
    assert_eq!(stashes.len(), 1);
    assert_eq!(stashes[0].branch, None);
    assert_eq!(stashes[0].created_time, BASE_TIME);
}
