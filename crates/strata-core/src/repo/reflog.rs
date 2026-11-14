//! Reflog reading and the ancestry checks undo needs.
//!
//! A reflog is the only record of what moved a ref, so an undo reads it to
//! learn where a branch was before the last action. Entries are returned newest
//! first: the reverse iterator reads the log from the end, so a limit never
//! touches the oldest entries of a huge reflog.
//!
//! HEAD's reflog and every local branch's reflog are separate files: HEAD
//! records checkouts as well, a branch only the commits it received. Both are
//! read, tagged with the ref they belong to, so the view can show a branch's
//! own history next to HEAD's.

use std::collections::HashSet;

use crate::types::{Oid, ReflogEntry, ReflogKind};
use crate::{Error, Result};

use super::Repo;

/// Space kept for one reflog line while reading backwards. git writes one line
/// per action and its message is a single line, so only a pathological entry
/// needs more than this.
const LINE_BUFFER: usize = 16 * 1024;

impl Repo {
    /// Up to `limit` entries of HEAD's reflog, newest first, followed by each
    /// local branch's reflog in name order, also newest first.
    ///
    /// `limit` applies to each reflog rather than to the whole answer: a
    /// repository with many branches must not hide HEAD's own entries behind
    /// them.
    pub fn reflog(&self, limit: usize) -> Result<Vec<ReflogEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let repo = self.local();
        let head: &gix::refs::FullNameRef = "HEAD"
            .try_into()
            .map_err(|error| Error::other(format!("HEAD is not a valid ref name: {error}")))?;

        let mut entries = read_log(&repo.refs, head, "HEAD", limit)?;
        let mut branches: Vec<(String, Oid)> = self
            .refs()?
            .into_iter()
            .filter(|reference| reference.kind == crate::types::RefKind::LocalBranch)
            .map(|reference| (reference.short_name, reference.target))
            .collect();
        branches.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, _) in branches {
            let Some(full_name) = self.local_branch_ref(&name) else {
                continue;
            };
            entries.extend(read_log(&repo.refs, full_name.as_ref(), &name, limit)?);
        }
        Ok(entries)
    }

    /// Remote-tracking branches whose history already contains `commit`.
    ///
    /// This is the check that keeps undo from rewriting published history: if
    /// any remote still has the commit, moving the branch off it needs a force
    /// push. It is the same ancestry question the history-edit warning asks,
    /// through [`is_ancestor`](Self::is_ancestor).
    pub fn pushed_remotes(&self, commit: Oid) -> Result<Vec<String>> {
        if commit.is_null() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for reference in self.refs()? {
            if reference.kind != crate::types::RefKind::RemoteBranch {
                continue;
            }
            if self.is_ancestor(commit, reference.target)? {
                names.push(reference.short_name);
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Paths that differ between the trees of `old` and `new`, for deciding
    /// which uncommitted changes `git reset --keep` would refuse to overwrite.
    ///
    /// Renames contribute both their old and new path, because git writes and
    /// deletes both sides.
    pub fn changed_paths_between(&self, old: Oid, new: Oid) -> Result<Vec<String>> {
        if old == new || old.is_null() || new.is_null() {
            return Ok(Vec::new());
        }
        let repo = self.local();
        let old_tree = repo
            .find_commit(old)
            .map_err(Error::gix)?
            .tree()
            .map_err(Error::gix)?;
        let new_tree = repo
            .find_commit(new)
            .map_err(Error::gix)?
            .tree()
            .map_err(Error::gix)?;
        let changes = repo
            .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
            .map_err(Error::gix)?;

        let mut paths = HashSet::new();
        for change in changes {
            paths.insert(change.location().to_string());
            paths.insert(change.source_location().to_string());
        }
        let mut paths: Vec<String> = paths.into_iter().collect();
        paths.sort();
        Ok(paths)
    }

    /// Full names of the local branches, for resolving a checkout's source.
    pub fn local_branch_names(&self) -> Result<Vec<String>> {
        let mut names: Vec<String> = self
            .refs()?
            .into_iter()
            .filter(|reference| reference.kind == crate::types::RefKind::LocalBranch)
            .map(|reference| reference.short_name)
            .collect();
        names.sort();
        Ok(names)
    }

    fn local_branch_ref(&self, short_name: &str) -> Option<gix::refs::FullName> {
        format!("refs/heads/{short_name}").try_into().ok()
    }
}

/// Reads one reflog, newest first, stopping after `limit` entries.
fn read_log(
    store: &gix::RefStore,
    name: &gix::refs::FullNameRef,
    reference: &str,
    limit: usize,
) -> Result<Vec<ReflogEntry>> {
    let mut buffer = vec![0u8; LINE_BUFFER];
    let Some(lines) = store
        .reflog_iter_rev(name, &mut buffer)
        .map_err(Error::gix)?
    else {
        return Ok(Vec::new());
    };

    let mut entries = Vec::new();
    for line in lines.take(limit) {
        let line = line.map_err(|error| Error::other(error.to_string()))?;
        let message = line.message.to_string();
        entries.push(ReflogEntry {
            reference: reference.to_owned(),
            old_id: line.previous_oid,
            new_id: line.new_oid,
            committer_name: line.signature.name.to_string(),
            committer_email: line.signature.email.to_string(),
            time: line.signature.time.seconds,
            kind: ReflogKind::parse(&message),
            message,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;

    #[test]
    fn kinds_cover_gits_own_wording_and_fall_back_to_other() {
        let cases = [
            ("commit (initial): one", ReflogKind::Commit),
            ("commit: two", ReflogKind::Commit),
            ("commit (amend): two", ReflogKind::Amend),
            ("checkout: moving from main to topic", ReflogKind::Checkout),
            ("reset: moving to HEAD~1", ReflogKind::Reset),
            ("rebase (start): checkout main", ReflogKind::RebaseStart),
            (
                "rebase (finish): returning to refs/heads/topic",
                ReflogKind::RebaseFinish,
            ),
            (
                "merge topic: Merge made by the 'ort' strategy.",
                ReflogKind::Merge,
            ),
            ("cherry-pick: pick 1234567", ReflogKind::CherryPick),
            ("pull: Fast-forward", ReflogKind::Pull),
            ("pull --rebase: Fast-forward", ReflogKind::Pull),
            ("branch: Created from HEAD", ReflogKind::BranchCreate),
            (
                "Branch: renamed refs/heads/old to refs/heads/new",
                ReflogKind::BranchRename,
            ),
            ("clone: from /tmp/source", ReflogKind::Other),
            ("filter-branch: rewrite", ReflogKind::Other),
        ];
        for (message, expected) in cases {
            assert_eq!(ReflogKind::parse(message), expected, "{message}");
        }
    }

    #[test]
    fn reflog_lists_head_then_each_branch_newest_first() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        let first = repo.commit("one");
        repo.write("b.txt", "two\n");
        let second = repo.commit("two");
        repo.git(&["branch", "topic"]);
        repo.git(&["checkout", "-q", "topic"]);
        repo.write("c.txt", "three\n");
        let third = repo.commit("three");
        repo.git(&["checkout", "-q", "main"]);

        let entries = repo.open().reflog(10).expect("reflog");

        let head: Vec<&ReflogEntry> = entries
            .iter()
            .filter(|entry| entry.reference == "HEAD")
            .collect();
        assert_eq!(
            head.iter()
                .map(|entry| entry.message.as_str())
                .collect::<Vec<_>>(),
            [
                "checkout: moving from topic to main",
                "commit: three",
                "checkout: moving from main to topic",
                "commit: two",
                "commit (initial): one",
            ],
            "HEAD's reflog is newest first and records checkouts",
        );
        assert_eq!(head[0].kind, ReflogKind::Checkout);
        assert_eq!(head[0].old_id, third);
        assert_eq!(head[0].new_id, second);
        assert_eq!(head[1].kind, ReflogKind::Commit);
        assert_eq!(head[1].old_id, second);
        assert_eq!(head[1].new_id, third);
        assert_eq!(head[2].kind, ReflogKind::Checkout);
        assert_eq!(head[4].old_id, Oid::null(gix::hash::Kind::Sha1));
        assert_eq!(head[4].new_id, first);
        assert_eq!(head[0].committer_name, "Strata Test");
        assert_eq!(head[0].committer_email, "strata@example.invalid");
        assert_eq!(head[0].time, 1_700_000_000);

        let topic = entries
            .iter()
            .find(|entry| entry.reference == "topic" && entry.new_id == third)
            .expect("the branch reflog lists its own commit");
        assert_eq!(topic.kind, ReflogKind::Commit);
        assert_eq!(topic.old_id, second);

        let limited = repo.open().reflog(1).expect("limited reflog");
        assert_eq!(
            limited
                .iter()
                .filter(|entry| entry.reference == "HEAD")
                .count(),
            1,
            "the limit applies to each reflog"
        );
        assert!(
            limited.iter().any(|entry| entry.reference == "topic"),
            "a branch reflog is still read"
        );
    }

    #[test]
    fn only_actions_with_a_previous_state_are_restorable() {
        assert!(ReflogKind::Commit.can_restore());
        assert!(ReflogKind::Amend.can_restore());
        assert!(ReflogKind::Checkout.can_restore());
        assert!(ReflogKind::Reset.can_restore());
        assert!(ReflogKind::RebaseFinish.can_restore());
        assert!(ReflogKind::Merge.can_restore());
        assert!(ReflogKind::CherryPick.can_restore());
        assert!(ReflogKind::Pull.can_restore());
        assert!(!ReflogKind::RebaseStart.can_restore());
        assert!(!ReflogKind::BranchCreate.can_restore());
        assert!(!ReflogKind::BranchRename.can_restore());
        assert!(!ReflogKind::Other.can_restore());
        assert!(ReflogKind::Commit.moves_branch());
        assert!(!ReflogKind::Checkout.moves_branch());
    }
}
