//! Scripted interactive rebase: one `git rebase -i` with a prepared todo.
//!
//! The todo is written by Strata and handed to git through
//! `GIT_SEQUENCE_EDITOR="cp <prepared>"`, which works on every platform git
//! supports because git runs the editor through its own `sh`. Reword messages
//! are not fed through that editor: each reworded commit is a `pick` followed by
//! `exec git commit --amend -F <file>`. An editor script would have to guess
//! which invocation belongs to which reword, while squash also opens the
//! message editor; a file per reword keeps the two apart and carries newlines,
//! quotes and non-ASCII text without shell quoting.
//!
//! `GIT_EDITOR=true` accepts git's prepared message for a squash, which is the
//! same combination git produces when the editor leaves the buffer untouched.
//!
//! Uncommitted changes are refused rather than autostashed: a conflict during
//! the rebase would then be indistinguishable from an autostash pop conflict,
//! and the user's work would move between commands. Untracked files are
//! allowed because they do not block a rebase.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, HeadInfo, Oid, RepoState, Result};

use super::Git;

/// One commit in the rebase todo, in the order git should apply it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseStep {
    pub commit: Oid,
    pub action: RebaseAction,
}

/// What to do with a step's commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseAction {
    /// Keep the commit as it is.
    Pick,
    /// Keep the changes but replace the message.
    Reword { message: String },
    /// Combine into the previous commit, using git's combined message (both
    /// messages, as an interactive squash produces with the editor untouched).
    Squash,
    /// Combine into the previous commit and keep the previous message.
    Fixup,
    /// Leave the commit out entirely.
    Drop,
}

/// Which way [`move_commit`] moves a commit in the todo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDirection {
    /// One step later in history, toward `HEAD`.
    Up,
    /// One step earlier in history, toward the base of the range.
    Down,
}

impl Git {
    /// Replay the commits of `onto..HEAD` in the order of `steps`, editing them
    /// as the actions say. A conflict stops the rebase and leaves
    /// [`RepoState::Rebasing`] in place, exactly like a merge that conflicts;
    /// [`Git::continue_in_progress`] and [`Git::abort_in_progress`] finish it.
    ///
    /// The range must be linear and `steps` must name every commit in it
    /// exactly once: a missing step would silently drop a commit, which is not
    /// something a UI action should do by accident. Use [`RebaseStep`]'s
    /// `Drop` action to drop a commit on purpose.
    pub fn rebase_edit(&self, onto: Oid, steps: &[RebaseStep]) -> Result<()> {
        self.ensure_can_rebase(onto, steps)?;

        // An empty range is a no-op; running git would only rewrite the branch
        // to the commit it already points at.
        if steps.is_empty() {
            return Ok(());
        }

        let scratch = Scratch::new()?;
        let todo = scratch.path("todo");
        fs::write(&todo, todo_text(steps, &scratch)?)?;

        let args = [
            "rebase",
            "-i",
            // An empty pick must stop instead of being dropped silently: the
            // `exec` that rewords it would otherwise amend an unrelated commit.
            "--empty=stop",
            &onto.to_string(),
        ];
        let result = self
            .clone()
            .with_env("GIT_SEQUENCE_EDITOR", format!("cp {}", shell_quote(&todo)))
            .with_env("GIT_EDITOR", "true")
            .run(&args, None);

        match result {
            Ok(_) => Ok(()),
            // A stop on a conflict or an empty pick leaves the rebase state
            // behind, which is a normal, resumable outcome rather than failure.
            Err(error @ Error::GitCommand { code: Some(1), .. }) => {
                if self.state()? == RepoState::Rebasing {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Refuse every situation in which `git rebase -i` would either fail with a
    /// confusing message or edit the wrong thing.
    fn ensure_can_rebase(&self, onto: Oid, steps: &[RebaseStep]) -> Result<()> {
        let state = self.state()?;
        if state != RepoState::Normal {
            return Err(Error::other(format!(
                "cannot edit commits: {} is in progress; finish or abort it first",
                state_name(state)
            )));
        }

        let repo = crate::repo::Repo::open(self.workdir())?;
        match repo.head()? {
            HeadInfo::Detached { .. } => {
                return Err(Error::other(
                    "cannot edit commits: HEAD is detached; check out a branch first",
                ));
            }
            HeadInfo::Branch { target: None, .. } => {
                return Err(Error::other(
                    "cannot edit commits: the branch has no commits",
                ));
            }
            HeadInfo::Branch { .. } => {}
        }

        // Only tracked changes matter: untracked files never block a rebase.
        let status = self.run(&["status", "--porcelain", "--untracked-files=no"], None)?;
        if !status.stdout.iter().all(u8::is_ascii_whitespace) {
            return Err(Error::other(
                "cannot edit commits: the working tree has uncommitted changes; \
                 commit or stash them first",
            ));
        }

        let range = format!("{onto}..HEAD");
        let merges = self.run(&["rev-list", "--merges", &range], None)?;
        if !merges.stdout.iter().all(u8::is_ascii_whitespace) {
            return Err(Error::other(
                "cannot edit commits: the range contains merge commits; \
                 only linear history can be edited",
            ));
        }

        let commits = self.rev_list(&range)?;
        validate_steps(&commits, steps)?;
        Ok(())
    }

    /// Commit ids of `range`, oldest first, i.e. the todo order.
    fn rev_list(&self, range: &str) -> Result<Vec<Oid>> {
        let output = self.run(&["rev-list", "--reverse", range], None)?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim().parse())
            .collect::<std::result::Result<Vec<Oid>, _>>()
            .map_err(|error| Error::other(format!("git listed an invalid commit id: {error}")))
    }
}

/// Build the todo for the common "reword one commit" case.
///
/// `commits` is the first-parent list from `onto` to `HEAD`, oldest first, as
/// [`Git::rebase_edit`] expects it.
pub fn reword_commit(
    commits: &[Oid],
    commit: Oid,
    message: impl Into<String>,
) -> Result<Vec<RebaseStep>> {
    let mut steps = picks(commits);
    steps[position(commits, commit)?].action = RebaseAction::Reword {
        message: message.into(),
    };
    Ok(steps)
}

/// Build the todo that squashes `commit` into the commit before it in
/// `commits`, keeping git's combined message.
pub fn squash_commit_into_parent(commits: &[Oid], commit: Oid) -> Result<Vec<RebaseStep>> {
    let index = position(commits, commit)?;
    if index == 0 {
        return Err(Error::other(
            "cannot squash the first commit in the range: it has no parent to squash into",
        ));
    }
    let mut steps = picks(commits);
    steps[index].action = RebaseAction::Squash;
    Ok(steps)
}

/// Build the todo that drops `commit` from history.
pub fn drop_commit(commits: &[Oid], commit: Oid) -> Result<Vec<RebaseStep>> {
    let mut steps = picks(commits);
    steps[position(commits, commit)?].action = RebaseAction::Drop;
    Ok(steps)
}

/// Build the todo that moves `commit` one place later ([`MoveDirection::Up`])
/// or earlier ([`MoveDirection::Down`]) in history.
pub fn move_commit(
    commits: &[Oid],
    commit: Oid,
    direction: MoveDirection,
) -> Result<Vec<RebaseStep>> {
    let index = position(commits, commit)?;
    let target = match direction {
        MoveDirection::Up if index + 1 < commits.len() => index + 1,
        MoveDirection::Down if index > 0 => index - 1,
        MoveDirection::Up => {
            return Err(Error::other(
                "that commit is already the newest in the range",
            ));
        }
        MoveDirection::Down => {
            return Err(Error::other(
                "that commit is already the oldest in the range",
            ));
        }
    };
    let mut steps = picks(commits);
    steps.swap(index, target);
    Ok(steps)
}

fn picks(commits: &[Oid]) -> Vec<RebaseStep> {
    commits
        .iter()
        .map(|commit| RebaseStep {
            commit: *commit,
            action: RebaseAction::Pick,
        })
        .collect()
}

fn position(commits: &[Oid], commit: Oid) -> Result<usize> {
    commits
        .iter()
        .position(|candidate| *candidate == commit)
        .ok_or_else(|| Error::other(format!("commit {commit} is not in the rebase range")))
}

/// Whether `steps` is a permutation of `commits` that git can execute.
fn validate_steps(commits: &[Oid], steps: &[RebaseStep]) -> Result<()> {
    let mut seen = HashSet::with_capacity(steps.len());
    let mut can_squash = false;
    for step in steps {
        if !commits.contains(&step.commit) {
            return Err(Error::other(format!(
                "commit {} is not in the rebase range",
                step.commit
            )));
        }
        if !seen.insert(step.commit) {
            return Err(Error::other(format!(
                "commit {} appears twice in the rebase steps",
                step.commit
            )));
        }
        match step.action {
            RebaseAction::Squash | RebaseAction::Fixup if !can_squash => {
                return Err(Error::other(
                    "a squash or fixup needs a preceding commit to combine into",
                ));
            }
            RebaseAction::Drop => {}
            _ => can_squash = true,
        }
    }
    if seen.len() != commits.len() {
        return Err(Error::other(format!(
            "the rebase steps must name each of the {} commits in the range exactly once",
            commits.len()
        )));
    }
    Ok(())
}

fn todo_text(steps: &[RebaseStep], scratch: &Scratch) -> Result<String> {
    let mut todo = String::new();
    for (index, step) in steps.iter().enumerate() {
        let commit = step.commit;
        match &step.action {
            RebaseAction::Pick => todo.push_str(&format!("pick {commit}\n")),
            RebaseAction::Squash => todo.push_str(&format!("squash {commit}\n")),
            RebaseAction::Fixup => todo.push_str(&format!("fixup {commit}\n")),
            RebaseAction::Drop => todo.push_str(&format!("drop {commit}\n")),
            RebaseAction::Reword { message } => {
                let path = scratch.path(&format!("message-{index}"));
                fs::write(&path, message)?;
                // `--cleanup=strip` is what an interactive reword applies to
                // the edited message, so the result matches the manual flow.
                todo.push_str(&format!("pick {commit}\n"));
                todo.push_str(&format!(
                    "exec git commit --amend --cleanup=strip -F {}\n",
                    shell_quote(&path)
                ));
            }
        }
    }
    Ok(todo)
}

fn state_name(state: RepoState) -> &'static str {
    match state {
        RepoState::Normal => "nothing",
        RepoState::Merging => "a merge",
        RepoState::Rebasing => "a rebase",
        RepoState::CherryPicking => "a cherry-pick",
        RepoState::Reverting => "a revert",
        RepoState::Bisecting => "a bisect",
        RepoState::ApplyingPatches => "`git am`",
    }
}

/// Quote one argument for the POSIX shell git uses to run editors and `exec`
/// lines. A Windows path is written with forward slashes, which its `sh`
/// understands and which avoids backslash escaping.
fn shell_quote(path: &Path) -> String {
    #[cfg(windows)]
    let text = path.to_string_lossy().replace('\\', "/");
    #[cfg(not(windows))]
    let text = path.to_string_lossy().into_owned();
    format!("'{}'", text.replace('\'', "'\\''"))
}

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Todo and reword-message files, removed when the rebase command returns.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Result<Self> {
        let base = std::env::temp_dir();
        for _ in 0..32 {
            let id = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = base.join(format!("strata-rebase-{}-{id}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => return Ok(Self { dir }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(Error::Io(error)),
            }
        }
        Err(Error::other(
            "could not create a temporary directory for the rebase todo",
        ))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repo;
    use crate::test_support::TestRepo;

    /// Base commit plus `count` commits that each add their own file, so a
    /// reorder never conflicts by itself.
    fn linear_repo(count: usize) -> (TestRepo, Oid) {
        let repo = TestRepo::new();
        repo.write("base.txt", "base\n");
        let base = repo.commit("base");
        for index in 0..count {
            repo.write(&format!("file-{index}.txt"), &format!("{index}\n"));
            repo.commit(&format!("commit {index}"));
        }
        (repo, base)
    }

    fn picks(commits: &[Oid]) -> Vec<RebaseStep> {
        commits
            .iter()
            .map(|commit| RebaseStep {
                commit: *commit,
                action: RebaseAction::Pick,
            })
            .collect()
    }

    /// The range a commit id names, as git revision syntax.
    fn range(base: Oid) -> String {
        format!("{base}..HEAD")
    }

    fn messages(repo: &TestRepo) -> Vec<String> {
        repo.git(&["log", "--format=%s"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn full_message(repo: &TestRepo) -> String {
        repo.git(&["log", "-1", "--format=%B"])
            .trim_end()
            .to_owned()
    }

    fn rebase(repo: &TestRepo, base: Oid, steps: &[RebaseStep]) {
        repo.git_handle()
            .rebase_edit(base, steps)
            .expect("rebase succeeds");
    }

    #[test]
    fn reorders_two_commits() {
        let (repo, base) = linear_repo(2);
        let commits = repo.rev_list(&range(base));
        let steps = picks(&commits);
        let swapped = vec![steps[1].clone(), steps[0].clone()];

        rebase(&repo, base, &swapped);

        assert_eq!(
            messages(&repo),
            ["commit 0", "commit 1", "base"],
            "the second commit is now applied first"
        );
        let rewritten = repo.rev_list(&range(base));
        assert_eq!(rewritten.len(), 2);
        assert_ne!(rewritten[0], commits[1], "commits are rewritten");
    }

    #[test]
    fn rewords_head_parent() {
        let (repo, base) = linear_repo(2);
        let commits = repo.rev_list(&range(base));
        let steps = reword_commit(&commits, commits[0], "commit 0 reworded\n\nA body line.\n")
            .expect("steps");

        rebase(&repo, base, &steps);

        assert_eq!(messages(&repo), ["commit 1", "commit 0 reworded", "base"]);
        assert_eq!(
            full_message(&repo),
            "commit 1",
            "the untouched commit keeps its message"
        );
        let rewritten = repo.rev_list(&range(base));
        let text = repo.git(&["log", "-1", "--format=%B", &rewritten[0].to_string()]);
        assert_eq!(
            text.trim_end(),
            "commit 0 reworded\n\nA body line.",
            "the full message, not only the subject, is replaced"
        );
    }

    #[test]
    fn squashes_head_into_parent_with_combined_message() {
        let (repo, base) = linear_repo(2);
        let commits = repo.rev_list(&range(base));
        let steps = squash_commit_into_parent(&commits, commits[1]).expect("steps");

        rebase(&repo, base, &steps);

        assert_eq!(messages(&repo), ["commit 0", "base"]);
        assert_eq!(
            full_message(&repo),
            "commit 0\n\ncommit 1",
            "git combines both messages, first commit first"
        );
        assert!(repo.path().join("file-1.txt").is_file());
    }

    #[test]
    fn fixup_keeps_the_first_message() {
        let (repo, base) = linear_repo(2);
        let commits = repo.rev_list(&range(base));
        let mut steps = picks(&commits);
        steps[1].action = RebaseAction::Fixup;

        rebase(&repo, base, &steps);

        assert_eq!(messages(&repo), ["commit 0", "base"]);
        assert_eq!(full_message(&repo), "commit 0");
        assert!(repo.path().join("file-1.txt").is_file());
    }

    #[test]
    fn drops_a_middle_commit() {
        let (repo, base) = linear_repo(3);
        let commits = repo.rev_list(&range(base));
        let steps = drop_commit(&commits, commits[1]).expect("steps");

        rebase(&repo, base, &steps);

        assert_eq!(messages(&repo), ["commit 2", "commit 0", "base"]);
        assert!(
            !repo.path().join("file-1.txt").exists(),
            "the dropped commit's change is gone"
        );
        assert!(repo.path().join("file-2.txt").is_file());
    }

    #[test]
    fn a_conflicting_reorder_leaves_the_rebasing_state() {
        let repo = TestRepo::new();
        repo.write("conflict.txt", "base\n");
        let base = repo.commit("base");
        repo.write("conflict.txt", "first\n");
        let first = repo.commit("first");
        repo.write("conflict.txt", "second\n");
        let second = repo.commit("second");

        let steps = picks(&[first, second]);
        let swapped = vec![steps[1].clone(), steps[0].clone()];
        repo.git_handle()
            .rebase_edit(base, &swapped)
            .expect("a conflict is a resumable stop, not an error");

        let repo_state = Repo::open(repo.path())
            .expect("open")
            .state()
            .expect("state");
        assert_eq!(repo_state, RepoState::Rebasing);
        assert!(
            Repo::open(repo.path())
                .expect("open")
                .conflicts()
                .expect("conflicts")
                .iter()
                .any(|entry| entry.path == "conflict.txt")
        );
    }

    #[test]
    fn refuses_a_merge_in_the_range() {
        let repo = TestRepo::new();
        repo.write("base.txt", "base\n");
        let base = repo.commit("base");
        repo.write("main.txt", "main\n");
        repo.commit("main work");
        repo.git(&["checkout", "-q", "-b", "side", &base.to_string()]);
        repo.write("side.txt", "side\n");
        repo.commit("side work");
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["merge", "--no-ff", "-m", "merge side", "side"]);

        let commits = repo.rev_list(&range(base));
        let error = repo
            .git_handle()
            .rebase_edit(base, &picks(&commits))
            .expect_err("a merge in the range is refused");
        assert!(error.to_string().contains("merge commits"), "{error}");
        assert_eq!(
            Repo::open(repo.path())
                .expect("open")
                .state()
                .expect("state"),
            RepoState::Normal
        );
    }

    #[test]
    fn refuses_a_dirty_worktree() {
        let (repo, base) = linear_repo(1);
        let commits = repo.rev_list(&range(base));
        repo.write("file-0.txt", "uncommitted\n");

        let error = repo
            .git_handle()
            .rebase_edit(base, &picks(&commits))
            .expect_err("dirty worktree is refused");
        assert!(error.to_string().contains("uncommitted changes"), "{error}");
        assert_eq!(repo.head(), commits[0], "nothing was rewritten");
    }

    #[test]
    fn refuses_a_detached_head() {
        let (repo, base) = linear_repo(1);
        let commits = repo.rev_list(&range(base));
        repo.git(&["checkout", "-q", "--detach"]);

        let error = repo
            .git_handle()
            .rebase_edit(base, &picks(&commits))
            .expect_err("detached HEAD is refused");
        assert!(error.to_string().contains("detached"), "{error}");
    }

    #[test]
    fn steps_must_cover_the_range_exactly_once() {
        let (repo, base) = linear_repo(2);
        let commits = repo.rev_list(&range(base));
        let error = repo
            .git_handle()
            .rebase_edit(base, &picks(&commits[..1]))
            .expect_err("a missing step is refused");
        assert!(error.to_string().contains("exactly once"), "{error}");
    }

    #[test]
    fn move_commit_swaps_neighbours_and_refuses_the_ends() {
        let commits: Vec<Oid> = (1..=3)
            .map(|value| format!("{value:040x}").parse().expect("hex object id"))
            .collect();

        let up = move_commit(&commits, commits[0], MoveDirection::Up).expect("move up");
        assert_eq!(
            up.iter().map(|step| step.commit).collect::<Vec<_>>(),
            [commits[1], commits[0], commits[2]]
        );
        let down = move_commit(&commits, commits[2], MoveDirection::Down).expect("move down");
        assert_eq!(
            down.iter().map(|step| step.commit).collect::<Vec<_>>(),
            [commits[0], commits[2], commits[1]]
        );
        assert!(move_commit(&commits, commits[2], MoveDirection::Up).is_err());
        assert!(move_commit(&commits, commits[0], MoveDirection::Down).is_err());
        assert!(
            move_commit(
                &commits,
                Oid::null(gix::hash::Kind::Sha1),
                MoveDirection::Up
            )
            .is_err()
        );
    }

    #[test]
    fn helpers_leave_every_other_step_a_pick() {
        let (repo, base) = linear_repo(2);
        let commits = repo.rev_list(&range(base));

        let dropped = drop_commit(&commits, commits[0]).expect("steps");
        assert_eq!(dropped[0].action, RebaseAction::Drop);
        assert_eq!(dropped[1].action, RebaseAction::Pick);

        let squashed = squash_commit_into_parent(&commits, commits[0]);
        assert!(
            squashed.is_err(),
            "the oldest commit has no parent to squash into"
        );
    }
}
