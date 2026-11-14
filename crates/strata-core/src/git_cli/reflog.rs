//! Undo: restore the state before a reflog entry.
//!
//! Undo is a history rewrite, so it never runs blind and never uses
//! `--hard`. A branch move goes through `git reset --keep`, which refuses to
//! overwrite local changes on the paths it touches; the checks here refuse the
//! same situations first, with a reason that names the real problem.
//!
//! A checkout is undone by switching back to the branch the reflog message
//! names, which is the state the user sees rather than a detached commit.
//!
//! Published history is not rewritten: if a remote-tracking branch already
//! contains the commit being moved away from, the undo is refused, the same
//! ancestry check history edits use to warn before a force push.

use crate::repo::Repo;
use crate::types::{HeadInfo, Oid, ReflogEntry, ReflogKind};
use crate::{Error, RepoState, Result};

use super::Git;
use super::rebase::state_name;

/// How many HEAD reflog entries an undo looks through for one it can restore.
/// Anything further down is old enough that "last action" would be misleading.
const UNDO_SCAN: usize = 50;

/// Where a restored checkout goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchTarget {
    /// A local branch that was checked out before.
    Branch(String),
    /// A commit, when the previous checkout was detached.
    Detached(Oid),
}

/// What undoing one reflog entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoStep {
    /// `git reset --keep <to>` on the current branch.
    Reset {
        branch: String,
        from: Oid,
        to: Oid,
    },
    Switch {
        target: SwitchTarget,
    },
}

/// A decided undo, kept so the caller can describe it before running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoPlan {
    pub entry: ReflogEntry,
    pub step: UndoStep,
}

impl UndoPlan {
    /// The branch or commit the undo lands on, for a confirmation or a status
    /// line.
    pub fn destination(&self) -> String {
        match &self.step {
            UndoStep::Reset { to, .. } => short(*to),
            UndoStep::Switch {
                target: SwitchTarget::Branch(name),
            } => name.clone(),
            UndoStep::Switch {
                target: SwitchTarget::Detached(id),
            } => short(*id),
        }
    }
}

impl Git {
    /// Plans the undo of the newest HEAD reflog entry that can be restored.
    pub fn plan_undo_last_action(&self) -> Result<UndoPlan> {
        let repo = Repo::open(self.workdir())?;
        self.ensure_can_undo(&repo)?;
        let entry = repo
            .reflog(UNDO_SCAN)?
            .into_iter()
            .find(|entry| entry.reference == "HEAD" && entry.kind.can_restore())
            .ok_or_else(|| Error::other("nothing to undo: HEAD has no restorable reflog entry"))?;
        // Only an entry at HEAD's current target describes the last action: a
        // newer action that cannot be restored (a branch creation, say) would
        // otherwise let this one discard work made since.
        if head_target(&repo)? != Some(entry.new_id) {
            return Err(Error::other(
                "cannot undo: the branch has moved since the last action",
            ));
        }
        self.plan_entry(&repo, entry)
    }

    /// Restores the state before one reflog row, which is what the view's
    /// "Restore to this point" offers.
    pub fn restore_to(&self, entry: &ReflogEntry) -> Result<UndoPlan> {
        let repo = Repo::open(self.workdir())?;
        self.ensure_can_undo(&repo)?;
        self.plan_entry(&repo, entry.clone())
    }

    /// Runs an undo plan. The repository is checked again so a plan that was
    /// confirmed against an older state cannot run after the state moved.
    pub fn apply_undo(&self, plan: &UndoPlan) -> Result<()> {
        let repo = Repo::open(self.workdir())?;
        self.ensure_can_undo(&repo)?;
        let checked = self.plan_entry(&repo, plan.entry.clone())?;
        if checked.step != plan.step {
            return Err(Error::other(
                "cannot undo: the repository changed while the confirmation was open",
            ));
        }
        match &checked.step {
            UndoStep::Reset { to, .. } => self
                .run(&["reset", "--keep", &to.to_string()], None)
                .map(|_| ()),
            UndoStep::Switch {
                target: SwitchTarget::Branch(name),
            } => self.run(&["switch", name], None).map(|_| ()),
            UndoStep::Switch {
                target: SwitchTarget::Detached(id),
            } => self
                .run(&["switch", "--detach", &id.to_string()], None)
                .map(|_| ()),
        }
    }

    /// Plans and runs one undo, for the session operation.
    pub fn undo_reflog(&self, entry: Option<&ReflogEntry>) -> Result<()> {
        let plan = match entry {
            Some(entry) => self.restore_to(entry)?,
            None => self.plan_undo_last_action()?,
        };
        self.apply_undo(&plan)
    }

    /// Refuses while git has an operation half-done: undoing a history move in
    /// the middle of one would leave both in an inconsistent state.
    fn ensure_can_undo(&self, repo: &Repo) -> Result<()> {
        let state = repo.state()?;
        if state != RepoState::Normal {
            return Err(Error::other(format!(
                "cannot undo: {} is in progress; finish or abort it first",
                state_name(state)
            )));
        }
        Ok(())
    }

    fn plan_entry(&self, repo: &Repo, entry: ReflogEntry) -> Result<UndoPlan> {
        if !entry.kind.can_restore() {
            return Err(Error::other(format!(
                "cannot restore this entry: `{}` is not an action strata knows how to undo",
                entry.message
            )));
        }
        let head = repo.head()?;
        let step = if entry.kind == ReflogKind::Checkout {
            checkout_step(repo, &entry)?
        } else {
            reset_step(repo, &head, &entry)?
        };
        Ok(UndoPlan { entry, step })
    }
}

/// The branch move that undoes a commit, amend, reset, rebase, merge, pick or
/// pull entry.
fn reset_step(repo: &Repo, head: &HeadInfo, entry: &ReflogEntry) -> Result<UndoStep> {
    let HeadInfo::Branch { short_name, target } = head else {
        return Err(Error::other(
            "cannot undo: HEAD is detached; check out the branch first",
        ));
    };
    let Some(from) = *target else {
        return Err(Error::other(
            "cannot undo: the current branch has no commits",
        ));
    };
    // A branch reflog entry can only be restored while that branch is checked
    // out; resetting another branch needs a checkout, which is a different
    // action than the user asked for.
    if entry.reference != "HEAD" && entry.reference != *short_name {
        return Err(Error::other(format!(
            "cannot restore {} while {short_name} is checked out; switch to {} first",
            entry.reference, entry.reference
        )));
    }
    let to = entry.old_id;
    if to.is_null() {
        return Err(Error::other(
            "cannot undo: this entry created the ref, so there is no earlier state",
        ));
    }

    let pushed = repo.pushed_remotes(from)?;
    if !pushed.is_empty() {
        return Err(Error::other(format!(
            "cannot undo: {} is already pushed to {}; rewriting it needs a force push",
            short(from),
            pushed.join(", ")
        )));
    }
    ensure_worktree_keeps(repo, from, to)?;
    Ok(UndoStep::Reset {
        branch: short_name.clone(),
        from,
        to,
    })
}

/// The switch that undoes a checkout.
fn checkout_step(repo: &Repo, entry: &ReflogEntry) -> Result<UndoStep> {
    if entry.reference != "HEAD" {
        return Err(Error::other(
            "cannot undo: only HEAD's reflog records checkouts",
        ));
    }
    let Some(source) = checkout_source(&entry.message) else {
        return Err(Error::other(format!(
            "cannot undo: `{}` does not name the previous branch",
            entry.message
        )));
    };
    // A switch would overwrite the same paths a reset would; the check is
    // shared so both report the same problem the same way.
    let current = head_target(repo)?;
    if let Some(current) = current
        && !entry.old_id.is_null()
        && entry.old_id != current
    {
        ensure_worktree_keeps(repo, current, entry.old_id)?;
    }

    let target = if repo.local_branch_names()?.iter().any(|name| name == source) {
        SwitchTarget::Branch(source.to_owned())
    } else if !entry.old_id.is_null() {
        // The previous checkout was detached (or a remote branch): the entry's
        // old id is the commit it showed.
        SwitchTarget::Detached(entry.old_id)
    } else {
        return Err(Error::other(format!(
            "cannot undo: `{source}` is not a local branch and the entry has no other commit"
        )));
    };
    Ok(UndoStep::Switch { target })
}

/// The ref a checkout moved away from, from git's message
/// (`checkout: moving from <source> to <target>`). `git switch` writes the same
/// message.
fn checkout_source(message: &str) -> Option<&str> {
    for prefix in ["checkout: moving from ", "switch: moving from "] {
        if let Some(rest) = message.strip_prefix(prefix) {
            return rest.split_once(" to ").map(|(source, _)| source);
        }
    }
    None
}

/// Refuses when the worktree or index holds changes on a path the move would
/// rewrite, mirroring what `git reset --keep` treats as a local change.
///
/// The comparison is by content, not by status codes: an index that already
/// matches the target (the state a soft reset leaves) is kept rather than
/// refused, exactly as `--keep` does, while a modified or untracked worktree
/// file at a path the target writes would be lost.
fn ensure_worktree_keeps(repo: &Repo, from: Oid, to: Oid) -> Result<()> {
    let mut blocked = Vec::new();
    for path in repo.changed_paths_between(from, to)? {
        let current = repo.file_at_commit(from, &path)?;
        let target = repo.file_at_commit(to, &path)?;
        let index = repo.file_in_index(&path)?;
        if index == target {
            continue;
        }
        if index != current {
            blocked.push(path);
            continue;
        }
        // A missing worktree file is the local deletion the move restores,
        // which `--keep` allows; a present file that differs from the commit is
        // content the move would overwrite.
        let worktree = repo.file_in_worktree_as_git(&path)?;
        if worktree.is_some() && worktree != current {
            blocked.push(path);
        }
    }
    if blocked.is_empty() {
        return Ok(());
    }
    blocked.sort();
    blocked.dedup();
    let shown = blocked
        .iter()
        .take(3)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let more = blocked.len().saturating_sub(3);
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    Err(Error::other(format!(
        "cannot undo: uncommitted changes to {shown}{more} would be lost; commit or stash them first"
    )))
}

/// The commit HEAD points at, or `None` on an unborn branch.
fn head_target(repo: &Repo) -> Result<Option<Oid>> {
    Ok(match repo.head()? {
        HeadInfo::Branch { target, .. } => target,
        HeadInfo::Detached { target } => Some(target),
    })
}

fn short(id: Oid) -> String {
    let hex = id.to_hex().to_string();
    hex[..7.min(hex.len())].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;

    #[test]
    fn a_checkout_message_names_where_it_came_from() {
        assert_eq!(
            checkout_source("checkout: moving from main to feature/x"),
            Some("main")
        );
        assert_eq!(
            checkout_source("switch: moving from topic to main"),
            Some("topic")
        );
        assert_eq!(checkout_source("commit: no move"), None);
    }

    fn repo_with_two_commits() -> (TestRepo, Oid, Oid) {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        let first = repo.commit("one");
        repo.write("b.txt", "two\n");
        let second = repo.commit("two");
        (repo, first, second)
    }

    #[test]
    fn undoing_a_commit_puts_the_branch_back() {
        let (repo, first, second) = repo_with_two_commits();

        let plan = repo.git_handle().plan_undo_last_action().expect("plan");
        assert_eq!(
            plan.step,
            UndoStep::Reset {
                branch: "main".into(),
                from: second,
                to: first,
            }
        );
        repo.git_handle().apply_undo(&plan).expect("undo");

        assert_eq!(repo.head(), first);
        assert!(!repo.path().join("b.txt").exists(), "the commit is gone");
        assert_eq!(repo.git(&["status", "--porcelain=v2"]), "");
    }

    #[test]
    fn undoing_an_amend_restores_the_original_commit() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        let original = repo.commit("original");
        repo.git(&["commit", "-q", "--amend", "-m", "amended"]);
        assert_ne!(repo.head(), original);
        repo.git_handle().undo_reflog(None).expect("undo");

        assert_eq!(repo.head(), original);
        assert_eq!(repo.git(&["log", "-1", "--format=%s"]).trim(), "original");
    }

    #[test]
    fn undoing_a_reset_brings_the_branch_back() {
        let (repo, first, second) = repo_with_two_commits();
        repo.git(&["reset", "-q", "--hard", &first.to_string()]);

        repo.git_handle().undo_reflog(None).expect("undo");

        assert_eq!(repo.head(), second, "the commit the reset dropped is back");
    }

    #[test]
    fn undoing_a_checkout_switches_back_to_the_previous_branch() {
        let (repo, _first, second) = repo_with_two_commits();
        repo.git(&["branch", "topic"]);
        repo.git(&["checkout", "-q", "topic"]);

        let plan = repo.git_handle().plan_undo_last_action().expect("plan");
        assert_eq!(
            plan.step,
            UndoStep::Switch {
                target: SwitchTarget::Branch("main".into())
            }
        );
        repo.git_handle().apply_undo(&plan).expect("undo");

        assert_eq!(
            repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "main"
        );
        assert_eq!(repo.head(), second, "main is back at its own tip");
    }

    #[test]
    fn a_dirty_file_the_reset_would_clobber_refuses_the_undo() {
        let (repo, first, _second) = repo_with_two_commits();
        repo.write("b.txt", "uncommitted\n");

        let error = repo
            .git_handle()
            .undo_reflog(None)
            .expect_err("a dirty tree the reset would overwrite is refused");
        assert!(error.to_string().contains("uncommitted changes"), "{error}");
        assert!(error.to_string().contains("b.txt"), "{error}");
        assert_ne!(repo.head(), first, "nothing was reset");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("b.txt")).expect("worktree file"),
            "uncommitted\n",
            "the change is untouched"
        );
    }

    #[test]
    fn a_dirty_file_the_reset_leaves_alone_does_not_block_it() {
        let (repo, first, _second) = repo_with_two_commits();
        repo.write("a.txt", "uncommitted\n");

        repo.git_handle().undo_reflog(None).expect("undo");

        assert_eq!(repo.head(), first);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).expect("worktree file"),
            "uncommitted\n",
            "an unrelated change survives the undo"
        );
    }

    #[test]
    fn a_pushed_commit_cannot_be_undone() {
        let (repo, first, second) = repo_with_two_commits();
        let remote = tempfile::TempDir::new().expect("remote temp dir");
        let remote_path = remote.path().join("remote.git");
        repo.git(&[
            "init",
            "-q",
            "--bare",
            remote_path.to_str().expect("utf-8 path"),
        ]);
        repo.git(&[
            "remote",
            "add",
            "origin",
            remote_path.to_str().expect("utf-8 path"),
        ]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);

        let error = repo
            .git_handle()
            .undo_reflog(None)
            .expect_err("pushed history is not rewritten");
        assert!(error.to_string().contains("already pushed"), "{error}");
        assert_eq!(repo.head(), second, "nothing was reset");
        assert_ne!(repo.head(), first);

        // The same check applies to restoring an older row.
        let entry = repo
            .open()
            .reflog(10)
            .expect("reflog")
            .into_iter()
            .find(|entry| entry.new_id == second)
            .expect("the pushed commit's entry");
        let error = repo
            .git_handle()
            .restore_to(&entry)
            .expect_err("pushed history is not rewritten");
        assert!(error.to_string().contains("already pushed"), "{error}");
    }

    #[test]
    fn undoing_while_a_merge_is_in_progress_is_refused() {
        let repo = TestRepo::new();
        repo.write("shared.txt", "base\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "side"]);
        repo.write("shared.txt", "side\n");
        repo.commit("side");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("shared.txt", "main\n");
        repo.commit("main");
        // A conflicting merge stops with the merge state behind it.
        let output = repo.git_output(&["merge", "-q", "side"]);
        assert!(!output.status.success(), "the merge must conflict");

        let error = repo
            .git_handle()
            .undo_reflog(None)
            .expect_err("a merge in progress is refused");
        assert!(error.to_string().contains("merge"), "{error}");
        assert!(error.to_string().contains("in progress"), "{error}");
    }

    #[test]
    fn restoring_a_row_uses_the_state_before_it() {
        let (repo, first, second) = repo_with_two_commits();
        let entry = repo
            .open()
            .reflog(10)
            .expect("reflog")
            .into_iter()
            .find(|entry| entry.reference == "HEAD" && entry.new_id == second)
            .expect("the commit entry");

        let plan = repo.git_handle().restore_to(&entry).expect("plan");
        repo.git_handle().apply_undo(&plan).expect("restore");

        assert_eq!(repo.head(), first);
    }

    #[test]
    fn a_branch_entry_cannot_move_another_branch() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("one");
        repo.git(&["branch", "topic"]);
        repo.write("b.txt", "two\n");
        repo.commit("two");
        // `topic` has its own reflog with the creation entry, which is not
        // restorable; the branch entry check is about which branch is checked
        // out, so build it from a commit on `topic` itself.
        repo.git(&["checkout", "-q", "topic"]);
        repo.write("c.txt", "three\n");
        repo.commit("three");
        repo.git(&["checkout", "-q", "main"]);
        let entry = repo
            .open()
            .reflog(10)
            .expect("reflog")
            .into_iter()
            .find(|entry| entry.reference == "topic" && entry.kind == ReflogKind::Commit)
            .expect("the topic commit entry");

        let error = repo
            .git_handle()
            .restore_to(&entry)
            .expect_err("another branch cannot be moved");
        assert!(error.to_string().contains("switch to topic"), "{error}");
    }
}
