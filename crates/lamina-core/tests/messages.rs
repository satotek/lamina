//! Integration tests for `Repo::commit_message` and `Repo::merge_message`.
//!
//! Every expectation is checked against git itself: `git log -1 --format=%B`
//! for stored messages, `git stripspace --strip-comments` for the cleanup, and
//! real commits made from the prepared message.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use lamina_core::repo::Repo;
use lamina_core::test_support::TestRepo;

/// Git queries the message tests compare against, built on the shared fixture.
trait Messages {
    fn git_bytes(&self, args: &[&str]) -> Vec<u8>;
    /// For commands whose failure is the point.
    fn git_try(&self, args: &[&str]) -> Output;
    fn git_try_in(&self, cwd: &Path, args: &[&str]) -> Output;
    /// A scratch file for `-F`; outside the worktree so it never gets staged.
    fn message_file(&self, name: &str) -> PathBuf;
    /// The stored message of `commit`, as git reports it minus the one newline
    /// `--format=%B` always appends (`%B` is the raw body plus `strbuf_complete_line`).
    fn stored_message(&self, commit: &str) -> String;
    /// [`stored_message`](Self::stored_message) for another repository, such as a
    /// linked worktree.
    fn stored_message_in(&self, cwd: &Path, commit: &str) -> String;
    /// The raw bytes of the commit message in the object, with no decoding.
    fn raw_object_message(&self, commit: &str) -> Vec<u8>;
    /// git's own `strip` cleanup of `input`, as `git stripspace` implements it.
    fn stripspace(&self, input: &[u8]) -> String;
    /// The path git uses for `name` in `cwd`, absolute.
    fn git_path(&self, cwd: &Path, name: &str) -> PathBuf;
}

impl Messages for TestRepo {
    fn git_bytes(&self, args: &[&str]) -> Vec<u8> {
        self.git_output(args).stdout
    }

    fn git_try(&self, args: &[&str]) -> Output {
        self.git_output(args)
    }

    fn git_try_in(&self, cwd: &Path, args: &[&str]) -> Output {
        self.git_output_in(cwd, args)
    }

    fn message_file(&self, name: &str) -> PathBuf {
        self.temp_path(name)
    }

    fn stored_message(&self, commit: &str) -> String {
        self.stored_message_in(self.path(), commit)
    }

    fn stored_message_in(&self, cwd: &Path, commit: &str) -> String {
        let raw = self.git_in(cwd, &["log", "-1", "--format=%B", commit]);
        raw.strip_suffix('\n')
            .expect("%B output always ends with a newline")
            .to_owned()
    }

    fn raw_object_message(&self, commit: &str) -> Vec<u8> {
        let object = self.git_bytes(&["cat-file", "commit", commit]);
        let start = object
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .expect("commit object has a header/message separator")
            + 2;
        object[start..].to_vec()
    }

    fn stripspace(&self, input: &[u8]) -> String {
        let mut child = self
            .command("git")
            .arg("stripspace")
            .arg("--strip-comments")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn git stripspace");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(input)
            .expect("write stripspace input");
        let output = child.wait_with_output().expect("wait for stripspace");
        assert!(output.status.success(), "git stripspace failed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn git_path(&self, cwd: &Path, name: &str) -> PathBuf {
        let reported = self.git_in(cwd, &["rev-parse", "--git-path", name]);
        let path = PathBuf::from(reported.trim());
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    }
}

/// `main` and `side` both change `conflict.txt`, so merging conflicts.
fn conflict_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("conflict.txt", "base\n");
    repo.commit("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("conflict.txt", "theirs\n");
    repo.commit("theirs");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("conflict.txt", "ours\n");
    repo.commit("ours");
    repo
}

fn start_conflicting_merge(repo: &TestRepo, branch: &str) {
    let merge = repo.git_try(&["merge", branch]);
    assert!(
        !merge.status.success(),
        "the merge should conflict: {}",
        String::from_utf8_lossy(&merge.stdout)
    );
}

#[test]
fn commit_message_keeps_every_paragraph() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.git(&["add", "-A"]);
    repo.git(&[
        "commit",
        "-q",
        "-m",
        "subject line",
        "-m",
        "first body paragraph",
        "-m",
        "second body paragraph",
    ]);
    repo.write("a.txt", "b\n");
    repo.commit("later commit");

    let open = repo.open();
    let head = repo.git(&["rev-parse", "HEAD~1"]).trim().to_owned();
    let message = open
        .commit_message(head.parse().expect("parse commit id"))
        .expect("commit message");

    assert_eq!(
        message,
        "subject line\n\nfirst body paragraph\n\nsecond body paragraph\n"
    );
    assert_eq!(message, repo.stored_message(&head));
    // `%B` is the stored message plus one newline, which is what the tests above
    // strip; spelling out the extra newline keeps the normalisation honest.
    assert_eq!(
        repo.git(&["log", "-1", "--format=%B", &head]),
        format!("{message}\n")
    );
}

#[test]
fn commit_message_of_a_commit_without_a_body() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("subject only");

    let message = repo
        .open()
        .commit_message(
            repo.git(&["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("head"),
        )
        .expect("commit message");

    assert_eq!(message, "subject only\n");
    assert_eq!(message, repo.stored_message("HEAD"));
}

#[test]
fn commit_message_keeps_trailing_whitespace_lines() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.git(&["add", "-A"]);
    // `verbatim` stores the file as it is, whitespace-only lines and all; normal
    // commits would clean them away before the object is written.
    let file = repo.message_file("verbatim-message");
    fs::write(&file, "subject\n\nbody\n\n   \n\t\n").expect("write message");
    repo.git(&[
        "commit",
        "-q",
        "--cleanup=verbatim",
        "-F",
        file.to_str().expect("utf-8 path"),
    ]);

    let message = repo
        .open()
        .commit_message(
            repo.git(&["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("head"),
        )
        .expect("commit message");

    assert_eq!(message, "subject\n\nbody\n\n   \n\t\n");
    assert!(
        message.ends_with("\t\n"),
        "the read must not trim anything: {message:?}"
    );
    assert_eq!(message, repo.stored_message("HEAD"));
}

#[test]
fn commit_message_decodes_non_utf8_encodings_lossily() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.git(&["add", "-A"]);
    let file = repo.message_file("latin1-message");
    fs::write(&file, b"caf\xe9 latin1\n").expect("write message");
    repo.git(&[
        "-c",
        "i18n.commitEncoding=ISO-8859-1",
        "commit",
        "-q",
        "--cleanup=verbatim",
        "-F",
        file.to_str().expect("utf-8 path"),
    ]);

    let raw = repo.raw_object_message("HEAD");
    assert!(
        raw.contains(&0xe9),
        "the object should keep the declared encoding's bytes: {raw:?}"
    );
    let message = repo
        .open()
        .commit_message(
            repo.git(&["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("head"),
        )
        .expect("commit message");

    // Documented limitation: the `encoding` header is not honoured, so the
    // message is the lossy UTF-8 decoding of the stored bytes. `git log`
    // transcodes it to UTF-8 instead and would report `café latin1`.
    assert_eq!(message, String::from_utf8_lossy(&raw));
    assert!(message.contains('\u{fffd}'), "{message:?}");
}

#[test]
fn merge_message_is_the_message_of_the_finished_merge() {
    let repo = conflict_repo();
    start_conflicting_merge(&repo, "side");

    let merge_msg_path = repo.git_path(repo.path(), "MERGE_MSG");
    let prepared = fs::read(&merge_msg_path).expect("read MERGE_MSG");
    let message = repo
        .open()
        .merge_message()
        .expect("merge message")
        .expect("a merge is in progress");
    assert_eq!(message, "Merge branch 'side'\n");
    assert_eq!(message, repo.stripspace(&prepared));

    repo.git(&["add", "conflict.txt"]);
    // `--no-edit` alone keeps the comment block (its cleanup mode is
    // `whitespace`), so the commit that matches the read is the one git makes
    // with the cleanup mode an edited message gets.
    repo.git(&["commit", "-q", "--no-edit", "--cleanup=strip"]);
    assert_eq!(message, repo.stored_message("HEAD"));

    assert_eq!(
        repo.open().merge_message().expect("merge message"),
        None,
        "the message file is gone once the merge is committed"
    );
}

#[test]
fn merge_message_removes_the_conflict_block_that_no_edit_keeps() {
    let repo = conflict_repo();
    start_conflicting_merge(&repo, "side");
    let message = repo
        .open()
        .merge_message()
        .expect("merge message")
        .expect("a merge is in progress");

    repo.git(&["add", "conflict.txt"]);
    repo.git(&["commit", "-q", "--no-edit"]);
    let committed = repo.stored_message("HEAD");
    assert!(
        committed.contains("# Conflicts:"),
        "git's whitespace cleanup keeps comments: {committed:?}"
    );
    assert_eq!(message, repo.stripspace(committed.as_bytes()));
}

#[test]
fn merge_message_cleans_whitespace_and_comments_the_way_git_does() {
    let repo = conflict_repo();
    repo.git(&["config", "core.commentChar", ";"]);
    start_conflicting_merge(&repo, "side");

    // A message shaped like git's, with the awkward lines a user edit can leave:
    // leading blank lines, repeated blanks, whitespace-only lines, trailing
    // whitespace, comments and non-comment lines starting with the other
    // character.
    let custom = "\n\nMerge branch 'side'   \n\n\n; Conflicts:\n; \tconflict.txt\n# not a comment\nbody\t\n   \n\t\n\n\n";
    let merge_msg_path = repo.git_path(repo.path(), "MERGE_MSG");
    fs::write(&merge_msg_path, custom).expect("write MERGE_MSG");

    let message = repo
        .open()
        .merge_message()
        .expect("merge message")
        .expect("a merge is in progress");
    assert_eq!(
        message, "Merge branch 'side'\n\n# not a comment\nbody\n",
        "comments use core.commentChar and everything else is cleaned like git"
    );
    assert_eq!(message, repo.stripspace(custom.as_bytes()));
}

#[test]
fn auto_comment_char_is_treated_as_hash() {
    let repo = conflict_repo();
    repo.git(&["config", "core.commentChar", "auto"]);
    start_conflicting_merge(&repo, "side");

    let merge_msg_path = repo.git_path(repo.path(), "MERGE_MSG");
    fs::write(&merge_msg_path, "subject\n# gone\n\nbody\n").expect("write MERGE_MSG");

    // Our documented simplification: `auto` is read as `#` rather than resolved
    // the way git resolves it while writing the message.
    assert_eq!(
        repo.open()
            .merge_message()
            .expect("merge message")
            .as_deref(),
        Some("subject\n\nbody\n")
    );
}

#[test]
fn squash_message_falls_back_to_squash_msg() {
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    repo.commit("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("side.txt", "side\n");
    repo.commit("side work");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("main.txt", "main\n");
    repo.commit("main work");
    // A squash without conflicts leaves SQUASH_MSG only.
    repo.git(&["merge", "--squash", "side"]);

    let squash_path = repo.git_path(repo.path(), "SQUASH_MSG");
    assert!(
        !repo.git_path(repo.path(), "MERGE_MSG").exists(),
        "a clean squash has no MERGE_MSG"
    );
    let prepared = fs::read(&squash_path).expect("read SQUASH_MSG");
    let message = repo
        .open()
        .merge_message()
        .expect("merge message")
        .expect("a squash is in progress");

    assert!(message.starts_with("Squashed commit of the following:\n"));
    assert_eq!(message, repo.stripspace(&prepared));

    repo.git(&["commit", "-q", "--no-edit"]);
    assert_eq!(message, repo.stored_message("HEAD"));
    assert_eq!(repo.open().merge_message().expect("merge message"), None);
}

#[test]
fn squash_message_wins_when_merge_msg_only_holds_comments() {
    let repo = conflict_repo();
    let squash = repo.git_try(&["merge", "--squash", "side"]);
    assert!(!squash.status.success(), "the squash should conflict");

    // git leaves both files: MERGE_MSG describing the conflict, SQUASH_MSG with
    // the log that becomes the message.
    let merge_msg_path = repo.git_path(repo.path(), "MERGE_MSG");
    let squash_path = repo.git_path(repo.path(), "SQUASH_MSG");
    fs::write(&merge_msg_path, "\n# Conflicts:\n#\tconflict.txt\n").expect("write MERGE_MSG");
    assert!(squash_path.exists());

    let message = repo
        .open()
        .merge_message()
        .expect("merge message")
        .expect("a squash is in progress");
    assert!(
        message.starts_with("Squashed commit of the following:\n"),
        "{message:?}"
    );
    assert_eq!(
        message,
        repo.stripspace(&fs::read(&squash_path).expect("read SQUASH_MSG"))
    );
}

#[test]
fn merge_message_reads_the_linked_worktrees_own_file() {
    let repo = conflict_repo();
    let linked_path = repo.temp_path("linked-worktree");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        linked_path.to_str().expect("utf-8 path"),
        "side",
    ]);
    // The merge conflicts inside the linked worktree, on `side`.
    let merge = repo.git_try_in(&linked_path, &["merge", "main"]);
    assert!(!merge.status.success(), "the merge should conflict");

    let private_dir = repo.git_in(&linked_path, &["rev-parse", "--git-dir"]);
    let common_dir = repo.git_in(&linked_path, &["rev-parse", "--git-common-dir"]);
    assert_ne!(
        private_dir.trim(),
        common_dir.trim(),
        "a linked worktree has its own git dir"
    );
    let merge_msg_path = repo.git_path(&linked_path, "MERGE_MSG");
    assert!(
        merge_msg_path.exists(),
        "MERGE_MSG should be in the private git dir: {}",
        merge_msg_path.display()
    );
    assert!(
        !repo.path().join(".git/MERGE_MSG").exists(),
        "the common dir must not hold the worktree's merge message"
    );

    let prepared = fs::read(&merge_msg_path).expect("read MERGE_MSG");
    let message = Repo::open(&linked_path)
        .expect("open linked worktree")
        .merge_message()
        .expect("merge message")
        .expect("a merge is in progress");
    assert_eq!(message, "Merge branch 'main' into side\n");
    assert_eq!(message, repo.stripspace(&prepared));

    repo.git_in(&linked_path, &["add", "conflict.txt"]);
    repo.git_in(
        &linked_path,
        &["commit", "-q", "--no-edit", "--cleanup=strip"],
    );
    assert_eq!(message, repo.stored_message_in(&linked_path, "HEAD"));

    assert_eq!(
        repo.open().merge_message().expect("merge message"),
        None,
        "the main worktree never had a merge in progress"
    );
}

#[test]
fn merge_message_is_none_without_an_operation() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    assert_eq!(repo.open().merge_message().expect("merge message"), None);

    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("b.txt", "b\n");
    repo.commit("side");
    repo.git(&["checkout", "-q", "main"]);
    // A fast-forward merge writes no message, because there is nothing to commit.
    repo.git(&["merge", "--ff-only", "side"]);
    assert_eq!(repo.open().merge_message().expect("merge message"), None);
}
