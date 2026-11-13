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

use super::{Git, checked_ref};

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
    pub fn cherry_pick(&self, commit: Oid) -> Result<()> {
        self.apply_commit("cherry-pick", commit, RepoState::CherryPicking, false)
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
            // Both commands exit 1 for a conflict and for a refusal to start
            // one; the recorded state, not the code alone, tells them apart. A
            // fatal error (exit 128), such as a pick already in progress, still
            // leaves the earlier state behind and must not pass as success.
            Err(error @ Error::GitCommand { code: Some(1), .. }) => {
                if self.state()? == stopped {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
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
    fn state(&self) -> Result<RepoState> {
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
