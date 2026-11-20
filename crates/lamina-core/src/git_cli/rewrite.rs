//! Rewording a commit at or below a merge without replaying its changes.
//!
//! `git rebase -i` refuses a merge in the range, yet a reword only replaces one
//! message: every commit from the target up to HEAD keeps its tree. So the new
//! commits are built bottom-up with `git commit-tree <tree> -p <parents>`,
//! which copies each tree exactly and lets a merge keep its parents. A parent
//! that this rewrite replaced is remapped to its new id; an untouched parent is
//! kept as it is.
//!
//! The branch then moves with `git update-ref` and the old tip as the expected
//! value, so a branch that moved while the rewrite ran fails instead of losing
//! commits. HEAD follows because it is symbolic.
//!
//! The index and working tree are not involved: HEAD's tree is unchanged, so
//! uncommitted changes stay valid. A dirty working tree is therefore allowed,
//! unlike a rebase, which has to replay its changes through it.
//!
//! The author and its date are preserved, while the committer is whoever `git`
//! is configured as now, which is what `git rebase` records for a rewritten
//! commit. `commit-tree -S` signs each new commit when `commit.gpgsign` is set,
//! leaving the key, format and program to git's own configuration.

use std::collections::HashMap;

use crate::repo::{CommitSource, Repo};
use crate::{Error, HeadInfo, Oid, RepoState, Result};

use super::Git;
use super::rebase::state_name;

/// How many first-parent steps a reword walks before it calls the commit off
/// the line. A history deeper than this is not a repository a menu acts on.
const LINE_LIMIT: usize = 100_000;

impl Git {
    /// Replaces `commit`'s message and rebuilds every commit above it on HEAD's
    /// first-parent line, keeping each tree.
    ///
    /// Refused when HEAD is detached, when git has an operation in progress, or
    /// when `commit` is not on the first-parent line of the checked-out branch.
    pub fn reword_across_merges(&self, commit: Oid, message: &str) -> Result<()> {
        if message.trim().is_empty() {
            return Err(Error::other("cannot reword: the commit message is empty"));
        }
        if commit.is_null() {
            return Err(Error::other("cannot reword: no commit was given"));
        }
        let repo = Repo::open(self.workdir())?;
        let state = repo.state()?;
        if state != RepoState::Normal {
            return Err(Error::other(format!(
                "cannot reword: {} is in progress; finish or abort it first",
                state_name(state)
            )));
        }
        let (branch, head) = match repo.head()? {
            HeadInfo::Branch {
                short_name,
                target: Some(head),
            } => (short_name, head),
            HeadInfo::Branch { target: None, .. } => {
                return Err(Error::other("cannot reword: the branch has no commits"));
            }
            HeadInfo::Detached { .. } => {
                return Err(Error::other(
                    "cannot reword: HEAD is detached; check out a branch first",
                ));
            }
        };
        let line = self.first_parent_line(&repo, head, commit)?;
        let sign = self.commit_signing_enabled();

        // Oldest first, so a commit's parents are already rebuilt when it is.
        let mut rewritten: HashMap<Oid, Oid> = HashMap::with_capacity(line.len());
        for old in &line {
            let source = repo.commit_source(*old)?;
            let parents: Vec<Oid> = source
                .parents
                .iter()
                .map(|parent| rewritten.get(parent).copied().unwrap_or(*parent))
                .collect();
            let body = if *old == commit {
                message_bytes(message)
            } else {
                source.message.clone()
            };
            let new = self.commit_tree(&source, &parents, &body, sign)?;
            rewritten.insert(*old, new);
        }

        let Some(new_head) = rewritten.get(&head).copied() else {
            return Err(Error::other("cannot reword: HEAD was not rewritten"));
        };
        if new_head == head {
            return Ok(());
        }
        let reference = format!("refs/heads/{branch}");
        let note = format!("reword: {}", short(commit));
        self.run(
            &[
                "update-ref",
                "-m",
                &note,
                &reference,
                &new_head.to_string(),
                &head.to_string(),
            ],
            None,
        )
        .map(|_| ())
    }

    /// HEAD's first parents from `head` down to `commit`, oldest first.
    fn first_parent_line(&self, repo: &Repo, head: Oid, commit: Oid) -> Result<Vec<Oid>> {
        let mut line = Vec::new();
        let mut current = head;
        while line.len() < LINE_LIMIT {
            line.push(current);
            if current == commit {
                line.reverse();
                return Ok(line);
            }
            let parents = repo.parents(current)?;
            match parents.first() {
                Some(parent) => current = *parent,
                None => break,
            }
        }
        if line.len() < LINE_LIMIT {
            return Err(Error::other(format!(
                "cannot reword {}: it is not on the first-parent line from HEAD",
                short(commit)
            )));
        }
        Err(Error::other(format!(
            "cannot reword {}: the first-parent walk did not reach it within {LINE_LIMIT} commits",
            short(commit)
        )))
    }

    /// Whether git would sign a commit made through this handle. The question
    /// is about the configuration git reads, so it goes through the same
    /// environment as the writes: `GIT_CONFIG_GLOBAL` and a repository config
    /// can differ from the host's.
    fn commit_signing_enabled(&self) -> bool {
        match self.run_internal(&["config", "--bool", "--get", "commit.gpgsign"], None) {
            Ok(output) => String::from_utf8_lossy(&output.stdout).trim() == "true",
            // Unset exits 1, and a config that cannot be read signs nothing.
            Err(_) => false,
        }
    }

    /// Builds one commit from `source` with `parents`, `message` and the current
    /// committer. `-S` makes git sign it with its configured key.
    fn commit_tree(
        &self,
        source: &CommitSource,
        parents: &[Oid],
        message: &[u8],
        sign: bool,
    ) -> Result<Oid> {
        let mut args = vec!["commit-tree".to_owned(), source.tree.to_string()];
        for parent in parents {
            args.push("-p".to_owned());
            args.push(parent.to_string());
        }
        if sign {
            args.push("-S".to_owned());
        }
        let output = self
            .clone()
            .with_env("GIT_AUTHOR_NAME", source.author_name.clone())
            .with_env("GIT_AUTHOR_EMAIL", source.author_email.clone())
            .with_env("GIT_AUTHOR_DATE", source.author_date())
            .run(&args, Some(message))?;
        let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        id.parse().map_err(|error| {
            Error::other(format!(
                "git commit-tree returned an invalid commit id: {error}"
            ))
        })
    }
}

/// The message as a commit object stores it: one final newline, no trailing
/// blank lines. `commit-tree` writes what it is given, so the normalization
/// `git commit` would apply is done here.
fn message_bytes(message: &str) -> Vec<u8> {
    let mut bytes = message.trim_end().as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
}

fn short(id: Oid) -> String {
    let hex = id.to_hex().to_string();
    hex[..7.min(hex.len())].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestRepo, config_path};

    fn commit_as(repo: &TestRepo, name: &str, date: &str, message: &str) -> Oid {
        repo.git(&["add", "-A"]);
        let output = repo
            .command("git")
            .args(["commit", "-q", "-m", message])
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", "author@example.invalid")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .expect("run git commit");
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        repo.head()
    }

    fn merge(repo: &TestRepo, branch: &str, message: &str, date: &str) {
        let output = repo
            .command("git")
            .args(["merge", "--no-ff", "-m", message, branch])
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .expect("run git merge");
        assert!(
            output.status.success(),
            "git merge failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `base -> target`, then two side branches that started at `target` are
    /// merged into `main`, so `target` sits behind four commits and two merges
    /// while staying on the first-parent line.
    fn merged_history() -> (TestRepo, Oid) {
        let repo = TestRepo::new();
        repo.write("base.txt", "base\n");
        commit_as(&repo, "Author One", "1700000100 +0900", "base");
        repo.write("target.txt", "target\n");
        let target = commit_as(&repo, "Author Two", "1700000200 -0700", "target");

        repo.write("main-1.txt", "main 1\n");
        commit_as(&repo, "Author Three", "1700000300 +0000", "main 1");
        repo.git(&["checkout", "-q", "-b", "side-1"]);
        repo.write("side-1.txt", "side 1\n");
        commit_as(&repo, "Author Four", "1700000400 +0200", "side 1");
        repo.git(&["checkout", "-q", "main"]);
        merge(&repo, "side-1", "merge side 1", "1700000500 +0200");

        repo.write("main-2.txt", "main 2\n");
        commit_as(&repo, "Author Five", "1700000600 +0000", "main 2");
        // A second side branch from the same target keeps the history a graph
        // two rewrites deep, with no chance of a fast-forward.
        repo.git(&["checkout", "-q", "-b", "side-2", &target.to_string()]);
        repo.write("side-2.txt", "side 2\n");
        commit_as(&repo, "Author Six", "1700000700 -0300", "side 2");
        repo.git(&["checkout", "-q", "main"]);
        merge(&repo, "side-2", "merge side 2", "1700000800 -0300");
        (repo, target)
    }

    /// `head`'s first-parent line down to and including `stop`.
    fn first_parent_line(repo: &TestRepo, head: Oid, stop: Oid) -> Vec<Oid> {
        let mut line = Vec::new();
        for id in repo
            .git(&["rev-list", "--first-parent", &head.to_string()])
            .lines()
        {
            let id: Oid = id.trim().parse().expect("valid object id");
            line.push(id);
            if id == stop {
                break;
            }
        }
        line
    }

    /// The first `count` commits of `head`'s first-parent line. A rewritten
    /// commit gets a new id, so its position is what names it afterwards.
    fn first_parent_prefix(repo: &TestRepo, head: Oid, count: usize) -> Vec<Oid> {
        repo.git(&["rev-list", "--first-parent", &head.to_string()])
            .lines()
            .take(count)
            .map(|id| id.trim().parse().expect("valid object id"))
            .collect()
    }

    fn trees(repo: &TestRepo, line: &[Oid]) -> Vec<String> {
        line.iter()
            .map(|commit| {
                repo.git(&["rev-parse", &format!("{commit}^{{tree}}")])
                    .trim()
                    .to_owned()
            })
            .collect()
    }

    fn parent_count(repo: &TestRepo, commit: Oid) -> usize {
        repo.git(&["rev-list", "--parents", "-n", "1", &commit.to_string()])
            .split_whitespace()
            .count()
            - 1
    }

    fn full_message(repo: &TestRepo, commit: Oid) -> String {
        repo.git(&["log", "-1", "--format=%B", &commit.to_string()])
            .trim_end()
            .to_owned()
    }

    fn author(repo: &TestRepo, commit: Oid) -> String {
        repo.git(&["log", "-1", "--format=%an <%ae> %aI", &commit.to_string()])
            .trim_end()
            .to_owned()
    }

    #[test]
    fn message_bytes_normalize_like_a_commit() {
        assert_eq!(message_bytes("subject"), b"subject\n");
        assert_eq!(message_bytes("subject\n\n\n"), b"subject\n");
        assert_eq!(message_bytes("subject\n\nbody  \n"), b"subject\n\nbody\n");
    }

    #[test]
    fn the_author_date_is_gits_raw_format() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        commit_as(&repo, "Author", "1700000200 -0700", "one");
        let source = repo.open().commit_source(repo.head()).expect("source");
        assert_eq!(source.author_date(), "1700000200 -0700");
        assert_eq!(source.author_name, "Author");
    }

    #[test]
    fn reword_behind_two_merges_keeps_every_tree_and_the_topology() {
        let (repo, target) = merged_history();
        let head = repo.head();
        let old_line = first_parent_line(&repo, head, target);
        assert_eq!(old_line.len(), 5, "two merges above the target");
        assert_eq!(parent_count(&repo, old_line[0]), 2);
        assert_eq!(parent_count(&repo, old_line[2]), 2);
        let old_trees = trees(&repo, &old_line);
        let old_messages: Vec<String> =
            old_line.iter().map(|id| full_message(&repo, *id)).collect();
        let old_authors: Vec<String> = old_line.iter().map(|id| author(&repo, *id)).collect();

        repo.git_handle()
            .reword_across_merges(target, "target reworded")
            .expect("the reword runs");

        let new_head = repo.head();
        assert_ne!(new_head, head, "the branch moved");
        assert_eq!(
            repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "main",
            "HEAD stays on the branch"
        );
        let new_line = first_parent_prefix(&repo, new_head, old_line.len());
        assert_eq!(new_line.len(), old_line.len());
        assert_eq!(
            trees(&repo, &new_line),
            old_trees,
            "every commit keeps its tree"
        );
        for (index, commit) in new_line.iter().enumerate() {
            assert_eq!(
                parent_count(&repo, *commit),
                parent_count(&repo, old_line[index]),
                "commit {index} keeps its parent count"
            );
            assert_eq!(
                author(&repo, *commit),
                old_authors[index],
                "commit {index} keeps its author and date"
            );
        }
        let new_messages: Vec<String> =
            new_line.iter().map(|id| full_message(&repo, *id)).collect();
        assert_eq!(
            new_messages.last().map(String::as_str),
            Some("target reworded"),
            "only the target message changed"
        );
        for index in 0..old_line.len() - 1 {
            assert_eq!(
                new_messages[index], old_messages[index],
                "commit {index} keeps its message"
            );
        }
        assert!(repo.path().join("target.txt").is_file());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("target.txt")).expect("worktree file"),
            "target\n",
            "the working tree is untouched"
        );
    }

    #[test]
    fn reword_a_merge_commit_keeps_its_parents() {
        let (repo, _target) = merged_history();
        let merge = repo.head();
        assert_eq!(parent_count(&repo, merge), 2);
        let tree = repo
            .git(&["rev-parse", &format!("{merge}^{{tree}}")])
            .trim()
            .to_owned();
        let parent_branches: Vec<String> = repo
            .git(&["log", "-1", "--format=%P", &merge.to_string()])
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        repo.git_handle()
            .reword_across_merges(merge, "merge side 2 reworded")
            .expect("a merge commit can be reworded");

        let new = repo.head();
        assert_ne!(new, merge);
        assert_eq!(parent_count(&repo, new), 2, "the merge keeps both parents");
        assert_eq!(
            repo.git(&["log", "-1", "--format=%P", &new.to_string()])
                .split_whitespace()
                .collect::<Vec<_>>(),
            parent_branches,
            "a merge with no rewritten ancestor keeps its parents as they are"
        );
        assert_eq!(
            repo.git(&["rev-parse", &format!("{new}^{{tree}}")]).trim(),
            tree,
            "the merge keeps its tree"
        );
        assert_eq!(full_message(&repo, new), "merge side 2 reworded");
    }

    #[test]
    fn reword_refuses_a_detached_head() {
        let (repo, target) = merged_history();
        let head = repo.head();
        repo.git(&["checkout", "-q", "--detach"]);
        let error = repo
            .git_handle()
            .reword_across_merges(target, "nope")
            .expect_err("detached HEAD is refused");
        assert!(error.to_string().contains("detached"), "{error}");
        assert_eq!(
            repo.git(&["rev-parse", "main"]).trim(),
            head.to_string(),
            "nothing moved"
        );
    }

    #[test]
    fn reword_refuses_an_operation_in_progress() {
        let repo = TestRepo::new();
        repo.write("conflict.txt", "base\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.write("conflict.txt", "topic\n");
        let topic = repo.commit("topic");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("conflict.txt", "main\n");
        repo.commit("main");
        let merge = repo.git_output(&["merge", "topic"]);
        assert!(!merge.status.success(), "the merge conflicts");
        assert_eq!(
            repo.open().state().expect("state"),
            RepoState::Merging,
            "the merge is left half-done"
        );

        let error = repo
            .git_handle()
            .reword_across_merges(topic, "nope")
            .expect_err("a merge in progress is refused");
        assert!(error.to_string().contains("in progress"), "{error}");
    }

    #[test]
    fn reword_refuses_a_commit_off_the_first_parent_line() {
        let repo = TestRepo::new();
        repo.write("base.txt", "base\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "side"]);
        repo.write("side.txt", "side\n");
        let side = repo.commit("side work");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("main.txt", "main\n");
        repo.commit("main work");

        let error = repo
            .git_handle()
            .reword_across_merges(side, "nope")
            .expect_err("an unmerged side commit is not on the line");
        assert!(error.to_string().contains("first-parent line"), "{error}");
    }

    #[test]
    fn reword_refuses_an_empty_message() {
        let (repo, target) = merged_history();
        let head = repo.head();
        let error = repo
            .git_handle()
            .reword_across_merges(target, "  \n")
            .expect_err("an empty message is refused");
        assert!(error.to_string().contains("message"), "{error}");
        assert_eq!(repo.head(), head);
    }

    #[test]
    fn undo_restores_the_tip_before_the_reword() {
        let (repo, target) = merged_history();
        let head = repo.head();

        repo.git_handle()
            .reword_across_merges(target, "target reworded")
            .expect("the reword runs");
        assert_ne!(repo.head(), head);
        assert_eq!(
            repo.open().reflog(5).expect("reflog")[0].kind,
            crate::ReflogKind::Reword,
            "the branch move is recorded as a reword"
        );

        repo.git_handle().undo_reflog(None).expect("undo runs");
        assert_eq!(repo.head(), head, "undo brings the old tip back");
        assert_eq!(
            full_message(&repo, target),
            "target",
            "the original message is back"
        );
    }

    #[test]
    fn signing_is_requested_when_commit_gpgsign_is_set() {
        let (repo, target) = merged_history();
        let head = repo.head();
        repo.git(&["config", "commit.gpgsign", "true"]);
        // No signer exists, so the rewrite cannot finish; the point is that
        // `commit-tree` was asked to sign instead of silently writing an
        // unsigned commit.
        let missing = config_path(&repo.path().join("no-such-gpg"));
        repo.git(&["config", "gpg.program", &missing]);

        let git = repo.git_handle();
        let error = git
            .reword_across_merges(target, "signed reword")
            .expect_err("the missing signer stops the rewrite");
        assert!(
            !error.to_string().is_empty(),
            "the failure names the signer: {error}"
        );
        assert!(
            git.log().iter().any(|record| {
                record.args.first().is_some_and(|arg| arg == "commit-tree")
                    && record.args.iter().any(|arg| arg == "-S")
            }),
            "commit-tree is built with -S while commit.gpgsign is set"
        );
        assert_eq!(repo.head(), head, "a failed rewrite moves nothing");
    }
}
