//! History operations: cherry-pick, revert, reset, tags, per-region conflict
//! resolution and continue/abort of whichever operation git left in progress.
//!
//! Every command here runs through the `git` executable (`super::Git`), so
//! hooks, signing and the user's config behave as in a terminal. Multi-step
//! operations that stop on a conflict are not failures: the repository keeps
//! the state git recorded and the UI lets the user resolve or abort it.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::repo::Repo;
use crate::session::ResetMode;
use crate::{Error, Oid, RepoState, Result};

use super::{Git, checked_ref, command_error};

/// `--empty=drop` arrived in git 2.45; older git stops on an empty pick.
const EMPTY_DROP_VERSION: (u32, u32, u32) = (2, 45, 0);

/// What a multi-commit cherry-pick left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CherryPickOutcome {
    /// Commits dropped because the branch already carried their change.
    pub skipped: Vec<Oid>,
}

/// How a pick sequence moves past a commit that becomes empty.
#[derive(Clone, Copy)]
enum EmptyPicks {
    /// git drops it by itself (`--empty=drop`).
    Drop,
    /// git stops on it; detect the empty stop and run `--skip`.
    Skip,
}

impl Git {
    /// Write `content` as the worktree form of `path`, then stage it.
    ///
    /// The bytes come from the conflict view, which already shows the content
    /// in the user's checkout form (line endings included), so they are written
    /// unchanged. `git add` then collapses the unmerged stages into a single
    /// resolved entry.
    pub fn resolve_with_content(&self, path: &str, content: &[u8]) -> Result<()> {
        let target = self.worktree_file(path)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                Error::other(format!(
                    "could not create the directory for {path}: {error}"
                ))
            })?;
            // A directory that is a symlink would carry the write out of the
            // worktree without any `..` in the path.
            let root = self.workdir().canonicalize().map_err(|error| {
                Error::other(format!("could not resolve the worktree: {error}"))
            })?;
            let parent = parent.canonicalize().map_err(|error| {
                Error::other(format!(
                    "could not resolve the directory for {path}: {error}"
                ))
            })?;
            if !parent.starts_with(&root) {
                return Err(Error::other(format!(
                    "path must stay inside the worktree: {path}"
                )));
            }
        }
        // Writing through a symlink would also land outside the worktree. A
        // resolved symlink conflict is not text anyway; taking a side is how
        // git resolves it.
        if fs::symlink_metadata(&target).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(Error::other(format!(
                "cannot write {path}: it is a symlink in the worktree"
            )));
        }
        fs::write(&target, content)
            .map_err(|error| Error::other(format!("could not write {path}: {error}")))?;
        self.run(&["add", "--", path], None).map(|_| ())
    }

    /// The worktree path for a repository-relative `path`, refusing anything
    /// that could point outside it.
    ///
    /// Paths come from the index, but the bytes are written without git's own
    /// path checks, so `..`, absolute paths and `.git` itself are rejected here
    /// instead of trusted.
    fn worktree_file(&self, path: &str) -> Result<PathBuf> {
        let relative = Path::new(path);
        if relative.as_os_str().is_empty() {
            return Err(Error::other("path is empty"));
        }
        if relative.is_absolute() {
            return Err(Error::other(format!(
                "path must be relative to the worktree: {path}"
            )));
        }
        for component in relative.components() {
            match component {
                // Case-insensitive because Windows and default macOS checkouts
                // are, so `.GIT` would name the repository as well.
                Component::Normal(name)
                    if name
                        .to_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case(".git")) =>
                {
                    return Err(Error::other(format!("path points into .git: {path}")));
                }
                Component::Normal(_) => {}
                _ => {
                    return Err(Error::other(format!(
                        "path must stay inside the worktree: {path}"
                    )));
                }
            }
        }
        Ok(self.workdir().join(relative))
    }

    /// `git cherry-pick <commit>`; a merge commit is picked against its first
    /// parent, because the UI asks to "pick this commit", not to choose a
    /// mainline.
    ///
    /// A pick that stops on a conflict leaves the repository in the
    /// cherry-picking state for the user to resolve; like a stopped merge, that
    /// is a normal outcome rather than an error.
    ///
    /// A pick whose change the branch already has produces no commit at all;
    /// it is skipped to keep the repository clean and reported as an error.
    pub fn cherry_pick(&self, commit: Oid) -> Result<()> {
        self.apply_commit("cherry-pick", commit, RepoState::CherryPicking, false)
    }

    /// `git cherry-pick <commit>...` for several commits, oldest first.
    ///
    /// A pick whose change the branch already has is dropped instead of
    /// stopping the sequence, and the remaining commits still apply. Git does
    /// that itself from 2.45 (`--empty=drop`); an older git stops on the empty
    /// pick, which is then skipped by hand. A real conflict still stops the
    /// sequence in the cherry-picking state for the user to resolve, exactly
    /// like [`Git::cherry_pick`].
    pub fn cherry_pick_many(&self, commits: &[Oid]) -> Result<CherryPickOutcome> {
        if commits.is_empty() {
            return Ok(CherryPickOutcome::default());
        }
        // A failed `git --version` does not block the pick: the skip path works
        // on every supported git and the pick itself would report the failure.
        let empty = match self.version() {
            Ok(version) if version >= EMPTY_DROP_VERSION => EmptyPicks::Drop,
            _ => EmptyPicks::Skip,
        };
        self.cherry_pick_sequence(commits, empty)
    }

    fn cherry_pick_sequence(
        &self,
        commits: &[Oid],
        empty: EmptyPicks,
    ) -> Result<CherryPickOutcome> {
        let merges = self.merge_commits(commits)?;
        // `-m` applies to every commit on one command line, so a run that mixes
        // a merge with anything else cannot be replayed as one sequence.
        if merges > 0 && commits.len() > 1 {
            return Err(Error::other(
                "cherry-picking several commits cannot include a merge commit; pick it on its own",
            ));
        }

        let mut args = vec!["cherry-pick".to_owned()];
        if matches!(empty, EmptyPicks::Drop) {
            args.push("--empty=drop".to_owned());
        }
        if merges == 1 {
            args.extend(["-m".to_owned(), "1".to_owned()]);
        }
        args.extend(commits.iter().map(ToString::to_string));

        self.pick_sequence(args, empty)
    }

    /// Runs a pick command until the sequence finishes or a conflict stops it.
    ///
    /// A pick that is still empty (one that was empty to begin with) is skipped
    /// by hand. Each `--skip` continues the sequence on its own, so the next
    /// iteration handles a later empty pick or conflict too.
    fn pick_sequence(&self, mut args: Vec<String>, empty: EmptyPicks) -> Result<CherryPickOutcome> {
        let mut skipped = Vec::new();
        loop {
            let output = self.run_output(&args, None)?;
            if output.status.success() {
                if matches!(empty, EmptyPicks::Drop) {
                    skipped.extend(dropped_commits(&output.stderr));
                }
                return Ok(CherryPickOutcome { skipped });
            }

            let error = command_error(&args, &output);
            if matches!(empty, EmptyPicks::Drop) {
                // Drops made before an exit 1 remain skipped; the sequence
                // stops on the commit that failed, not on them.
                skipped.extend(dropped_commits(&output.stderr));
            }
            if !matches!(error, Error::GitCommand { code: Some(1), .. }) {
                return Err(error);
            }
            if self.has_unmerged_paths()? {
                return Ok(CherryPickOutcome { skipped });
            }

            if is_empty_commit_output(&error) && self.state()? == RepoState::CherryPicking {
                // `--empty=drop` already handled a commit that became empty;
                // this is one that was empty to begin with, which even that
                // flag leaves stopped in the sequence.
                if let Some(commit) = self.cherry_pick_head()? {
                    skipped.push(commit);
                }
                args = vec!["cherry-pick".to_owned(), "--skip".to_owned()];
                continue;
            }

            // Another exit 1 that left the pick in progress (for example a
            // failed commit hook) belongs to the existing stopped flow.
            if self.state()? == RepoState::CherryPicking {
                return Ok(CherryPickOutcome { skipped });
            }
            return Err(error);
        }
    }

    /// How many of `commits` are merge commits, in one `rev-list` call.
    fn merge_commits(&self, commits: &[Oid]) -> Result<usize> {
        let mut stdin = commits
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        stdin.push('\n');
        let output = self.run(
            &["rev-list", "--parents", "--no-walk", "--stdin"],
            Some(stdin.as_bytes()),
        )?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.split_whitespace().count() > 2)
            .count())
    }

    /// The commit git stopped on in a cherry-pick sequence, if any.
    fn cherry_pick_head(&self) -> Result<Option<Oid>> {
        match self.run(
            &["rev-parse", "--verify", "--quiet", "CHERRY_PICK_HEAD"],
            None,
        ) {
            Ok(output) => Ok(String::from_utf8_lossy(&output.stdout).trim().parse().ok()),
            Err(Error::GitCommand { code: Some(1), .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// `git revert --no-edit <commit>` (`-m 1` for a merge), with the same
    /// conflict handling as [`Git::cherry_pick`].
    pub fn revert(&self, commit: Oid) -> Result<()> {
        self.apply_commit("revert", commit, RepoState::Reverting, true)
    }

    fn apply_commit(
        &self,
        command: &str,
        commit: Oid,
        stopped: RepoState,
        no_edit: bool,
    ) -> Result<()> {
        let id = commit.to_string();
        let mut args = vec![command.to_owned()];
        if no_edit {
            // A GUI has no editor for the message git prepares from the
            // reverted commit.
            args.push("--no-edit".to_owned());
        }
        if self.is_merge_commit(commit)? {
            // Git refuses to pick or revert a merge without a mainline.
            args.extend(["-m".to_owned(), "1".to_owned()]);
        }
        args.push(id);

        match self.run_unit(&args, None) {
            Ok(()) => Ok(()),
            // A conflict, a refusal to start and an empty pick all exit 1; the
            // message and the recorded state, not the code alone, tell them
            // apart. A fatal error (exit 128), such as a pick already in
            // progress, still leaves the earlier state behind and must not
            // pass as success.
            Err(error @ Error::GitCommand { code: Some(1), .. }) => {
                // An empty pick has nothing to resolve, so it is not a stop.
                // Without the skip the repository would stay mid-pick with
                // zero conflicts, and the UI would offer a banner for an
                // operation the user cannot finish.
                if is_empty_commit_output(&error) && !self.has_unmerged_paths()? {
                    if self.state()? == stopped {
                        // Only single commits reach here (see `cherry_pick`),
                        // so the skip finishes the sequencer as well; a
                        // multi-commit range would go on with the next commit.
                        self.run(&[command, "--skip"], None)?;
                    }
                    return Err(Error::other(empty_commit_message(stopped)));
                }
                if self.state()? == stopped {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Whether the index holds unmerged entries, i.e. what a stopped pick
    /// needs before it is a conflict the user can resolve.
    fn has_unmerged_paths(&self) -> Result<bool> {
        let output = self.run(&["ls-files", "--unmerged", "-z"], None)?;
        Ok(!output.stdout.is_empty())
    }

    /// Whether `commit` has more than one parent.
    fn is_merge_commit(&self, commit: Oid) -> Result<bool> {
        let output = self.run(
            &["rev-list", "--parents", "-n", "1", &commit.to_string()],
            None,
        )?;
        // `<commit> <parent>...`; a merge lists at least two parents.
        Ok(String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .count()
            > 2)
    }

    /// `git reset --<mode> <target>` on the current branch.
    pub fn reset(&self, target: Oid, mode: ResetMode) -> Result<()> {
        let flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        };
        self.run(&["reset", flag, &target.to_string()], None)
            .map(|_| ())
    }

    /// Create a lightweight tag, or an annotated one when `message` is set.
    ///
    /// The message is passed on stdin (`-F -`) so newlines and a leading `-`
    /// need no quoting or temporary file. `--` separates the options from the
    /// name and target, so neither can be read as a flag.
    pub fn create_tag(&self, name: &str, target: Oid, message: Option<&str>) -> Result<()> {
        let name = checked_ref(name, "tag")?;
        // git's own rules for ref names: spaces, `..`, `~^:?*[`, a trailing
        // `.lock`, ... The command only sets the exit code, so the message has
        // to come from here.
        match self.run(&["check-ref-format", &format!("refs/tags/{name}")], None) {
            Ok(_) => {}
            Err(Error::GitCommand { code: Some(1), .. }) => {
                return Err(Error::other(format!("`{name}` is not a valid tag name")));
            }
            Err(error) => return Err(error),
        }

        let mut args = vec!["tag".to_owned()];
        let stdin = match message {
            Some(message) => {
                args.extend(["-a".to_owned(), "-F".to_owned(), "-".to_owned()]);
                Some(message.as_bytes().to_vec())
            }
            None => None,
        };
        args.extend(["--".to_owned(), name.to_owned(), target.to_string()]);
        self.run_unit(&args, stdin)
    }

    /// `git tag -d <name>`.
    pub fn delete_tag(&self, name: &str) -> Result<()> {
        let name = checked_ref(name, "tag")?;
        let args = [
            "tag".to_owned(),
            "-d".to_owned(),
            "--".to_owned(),
            name.to_owned(),
        ];
        self.run_unit(&args, None)
    }

    /// Continue whichever of merge, rebase, cherry-pick, revert or `am` git
    /// left in progress (see [`Repo::state`]).
    ///
    /// A continue may want an editor for the commit message; a GUI has none, so
    /// it runs with `GIT_EDITOR=true` (and, for a rebase, the sequence editor
    /// too) instead of blocking on one. Merge keeps using the prepared message
    /// exactly like [`Git::merge_continue`].
    pub fn continue_in_progress(&self) -> Result<()> {
        match self.state()? {
            RepoState::Merging => self.merge_continue(),
            RepoState::Rebasing => self.continue_without_editor(&["rebase", "--continue"], true),
            RepoState::CherryPicking => {
                self.continue_without_editor(&["cherry-pick", "--continue"], false)
            }
            RepoState::Reverting => self.continue_without_editor(&["revert", "--continue"], false),
            RepoState::ApplyingPatches => {
                self.continue_without_editor(&["am", "--continue"], false)
            }
            RepoState::Bisecting => Err(Error::other("nothing to continue: git bisect is running")),
            RepoState::Normal => Err(Error::other(
                "nothing to continue: no operation is in progress",
            )),
        }
    }

    /// Abort whichever operation git left in progress, restoring the state from
    /// before it started. A bisect is ended with `git bisect reset`.
    pub fn abort_in_progress(&self) -> Result<()> {
        match self.state()? {
            RepoState::Merging => self.merge_abort(),
            RepoState::Rebasing => self.run(&["rebase", "--abort"], None).map(|_| ()),
            RepoState::CherryPicking => self.run(&["cherry-pick", "--abort"], None).map(|_| ()),
            RepoState::Reverting => self.run(&["revert", "--abort"], None).map(|_| ()),
            RepoState::ApplyingPatches => self.run(&["am", "--abort"], None).map(|_| ()),
            RepoState::Bisecting => self.run(&["bisect", "reset"], None).map(|_| ()),
            RepoState::Normal => Err(Error::other(
                "nothing to abort: no operation is in progress",
            )),
        }
    }

    /// The operation recorded in the git dir, read through the same module the
    /// UI uses so a busy/abort decision never disagrees with the snapshot.
    pub(super) fn state(&self) -> Result<RepoState> {
        Repo::open(self.workdir())?.state()
    }

    fn continue_without_editor(&self, args: &[&str], sequence: bool) -> Result<()> {
        let mut git = self.clone().with_env("GIT_EDITOR", "true");
        if sequence {
            git = git.with_env("GIT_SEQUENCE_EDITOR", "true");
        }
        git.run(args, None).map(|_| ())
    }
}

/// Whether git's output says the pick produced no commit.
///
/// A pick of a change the branch already has stops with "The previous
/// cherry-pick is now empty" on stderr. A revert on newer git instead gets as
/// far as its commit step, which fails with "nothing to commit", and that text
/// reaches here through the stdout fallback in `command_error`, so both
/// streams are covered.
fn is_empty_commit_output(error: &Error) -> bool {
    let Error::GitCommand { stderr, .. } = error else {
        return false;
    };
    ["now empty", "nothing to commit", "nothing added to commit"]
        .iter()
        .any(|text| stderr.contains(text))
}

/// What the user needs to know when an empty pick is skipped.
fn empty_commit_message(stopped: RepoState) -> &'static str {
    match stopped {
        RepoState::Reverting => "The commit is already reverted: reverting it changes nothing.",
        _ => "The commit is already applied: cherry-picking it changes nothing.",
    }
}

/// Commits git reports as dropped, from lines like
/// `dropping <oid> <subject> -- patch contents already upstream`.
///
/// Only `--empty=drop` prints these, and `LC_ALL=C` pins the wording. A git
/// whose message differs only loses the note, not the skip.
fn dropped_commits(stderr: &[u8]) -> Vec<Oid> {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("dropping ")?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;

    #[test]
    fn parses_dropped_commit_lines() {
        let oid: Oid = "0705b8830170255f6e2bfda69769a64d028c2e7b"
            .parse()
            .expect("oid");
        let line = format!("dropping {oid} two -- patch contents already upstream\n");
        assert_eq!(dropped_commits(line.as_bytes()), vec![oid]);
        assert!(dropped_commits(b"error: could not apply abc\n").is_empty());
    }

    /// Git before 2.45 has no `--empty=drop`; the skip path must still drop an
    /// empty middle pick and apply the rest.
    #[test]
    fn the_skip_path_drops_an_empty_middle_pick() {
        let repo = TestRepo::new();
        repo.write("a.txt", "base\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "side"]);
        repo.write("one.txt", "one\n");
        let first = repo.commit("one");
        repo.write("shared.txt", "shared\n");
        let empty = repo.commit("shared");
        repo.write("three.txt", "three\n");
        let last = repo.commit("three");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("shared.txt", "shared\n");
        repo.commit("shared already");

        let outcome = repo
            .git_handle()
            .cherry_pick_sequence(&[first, empty, last], EmptyPicks::Skip)
            .expect("the skip path applies the remaining commits");

        assert_eq!(outcome.skipped, vec![empty]);
        assert_eq!(repo.open().state().expect("state"), RepoState::Normal);
        assert_eq!(repo.git(&["log", "-1", "--format=%s"]).trim(), "three");
        assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).trim(), "4");
        assert_eq!(repo.git(&["status", "--porcelain=v2"]), "");
    }
}
