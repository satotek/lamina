//! Thin wrappers for the mutating git commands the UI needs.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::{Error, Oid, RepoState, Result};

use super::{Git, checked_ref};

/// How a patch is applied. The patch text comes from the diff module, which
/// already produces `git apply`-compatible output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    /// Apply to the index only, leaving the worktree alone.
    Stage,
    /// Reverse-apply to the index, i.e. unstage.
    Unstage,
    /// Reverse-apply to the worktree, dropping the change.
    Discard,
}

/// Optional flags for [`Git::commit`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitOptions {
    pub amend: bool,
    pub allow_empty: bool,
    pub signoff: bool,
}

/// One entry of `git stash list`, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    pub index: usize,
    pub message: String,
    pub oid: Oid,
}

/// Keep inline pathspecs well below the ~32 KiB Windows command-line limit.
const MAX_INLINE_PATHSPECS: usize = 64;
const MAX_INLINE_PATHSPEC_BYTES: usize = 8 * 1024;

impl Git {
    pub fn stage_paths(&self, paths: &[impl AsRef<Path>]) -> Result<()> {
        let paths = collect_paths(paths);
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["add".to_owned(), "-A".to_owned()];
        let stdin = push_pathspecs(&mut args, &paths);
        self.run_unit(&args, stdin)
    }

    /// Restore the index entries to HEAD. With an unborn HEAD `git restore
    /// --staged` cannot resolve HEAD, so newly added files are dropped from
    /// the index with `git rm --cached` instead.
    pub fn unstage_paths(&self, paths: &[impl AsRef<Path>]) -> Result<()> {
        let paths = collect_paths(paths);
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = if self.has_head()? {
            vec!["restore".to_owned(), "--staged".to_owned()]
        } else {
            vec![
                "rm".to_owned(),
                "--cached".to_owned(),
                "--ignore-unmatch".to_owned(),
                "-r".to_owned(),
            ]
        };
        let stdin = push_pathspecs(&mut args, &paths);
        self.run_unit(&args, stdin)
    }

    pub fn discard_paths(&self, paths: &[impl AsRef<Path>]) -> Result<()> {
        let paths = collect_paths(paths);
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["restore".to_owned(), "--worktree".to_owned()];
        let stdin = push_pathspecs(&mut args, &paths);
        self.run_unit(&args, stdin)
    }

    /// Discard the staged and the worktree changes at `paths` in one step: the
    /// index and the worktree are both restored from HEAD.
    ///
    /// A path that is only in the index (added with `git add`, absent from
    /// HEAD) has no HEAD content to restore from, so it is removed from the
    /// index and deleted from the worktree instead. On an unborn branch every
    /// staged path is such a file, and this deletes them.
    ///
    /// Long lists are split into chunks because each `git diff`/`git restore`
    /// invocation takes its paths on the command line, which Windows caps at
    /// roughly 32 KiB.
    pub fn discard_staged_paths(&self, paths: &[impl AsRef<Path>]) -> Result<()> {
        let paths = collect_paths(paths);
        if paths.is_empty() {
            return Ok(());
        }
        for chunk in chunks_by_bytes(&paths, MAX_INLINE_PATHSPECS, MAX_INLINE_PATHSPEC_BYTES) {
            let added = self.added_to_index(chunk)?;
            let mut remove = Vec::new();
            let mut restore = Vec::new();
            for path in chunk {
                let text = path_string(path);
                if added.contains(&text) {
                    remove.push(text);
                } else {
                    restore.push(path.clone());
                }
            }

            if !remove.is_empty() {
                let mut args = vec![
                    "rm".to_owned(),
                    "--cached".to_owned(),
                    "--ignore-unmatch".to_owned(),
                    "-r".to_owned(),
                    "--".to_owned(),
                ];
                args.extend(remove.iter().cloned());
                self.run_unit(&args, None)?;
                for path in &remove {
                    remove_worktree_file(&self.workdir, path)?;
                }
            }

            if !restore.is_empty() {
                let mut args = vec![
                    "restore".to_owned(),
                    "--source=HEAD".to_owned(),
                    "--staged".to_owned(),
                    "--worktree".to_owned(),
                ];
                let stdin = push_pathspecs(&mut args, &restore);
                self.run_unit(&args, stdin)?;
            }
        }
        Ok(())
    }

    /// Remove untracked files (and directories) at the given paths.
    ///
    /// `git clean` has no `--pathspec-from-file`, so long lists are split into
    /// several invocations instead.
    pub fn delete_untracked(&self, paths: &[impl AsRef<Path>]) -> Result<()> {
        let paths = collect_paths(paths);
        if paths.is_empty() {
            return Ok(());
        }
        for chunk in chunks_by_bytes(&paths, MAX_INLINE_PATHSPECS, MAX_INLINE_PATHSPEC_BYTES) {
            let mut args = vec![
                "clean".to_owned(),
                "-f".to_owned(),
                "-d".to_owned(),
                "--".to_owned(),
            ];
            args.extend(chunk.iter().map(|path| path_string(path)));
            self.run_unit(&args, None)?;
        }
        Ok(())
    }

    /// Apply a diff read from stdin, so patches with any quoting or encoding
    /// reach git byte-for-byte without a temp file.
    pub fn apply_patch(&self, patch: &str, mode: ApplyMode) -> Result<()> {
        self.apply_patch_bytes(patch.as_bytes(), mode)
    }

    /// [`apply_patch`](Self::apply_patch) for a patch whose lines carry the
    /// file's original bytes, such as a Shift_JIS checkout.
    pub fn apply_patch_bytes(&self, patch: &[u8], mode: ApplyMode) -> Result<()> {
        let mut args = vec!["apply".to_owned()];
        match mode {
            ApplyMode::Stage => args.push("--cached".to_owned()),
            ApplyMode::Unstage => {
                args.push("--cached".to_owned());
                args.push("--reverse".to_owned());
            }
            ApplyMode::Discard => args.push("--reverse".to_owned()),
        }
        args.push("--recount".to_owned());
        args.push("--whitespace=nowarn".to_owned());
        args.push("-".to_owned());
        self.run_unit(&args, Some(patch.to_vec()))
    }

    /// Apply a whole patch file to the working tree.
    ///
    /// `--3way` falls back to the blobs the patch names, which is what makes a
    /// patch from another checkout apply when the context has drifted. When
    /// those blobs are not in this repository either, the plain form is tried
    /// so a small context shift still applies; both messages are reported when
    /// that fails too.
    pub fn apply_patch_file(&self, patch: &[u8]) -> Result<()> {
        let attempt = |three_way: bool| {
            let mut args = vec!["apply".to_owned()];
            if three_way {
                args.push("--3way".to_owned());
            }
            args.push("--recount".to_owned());
            args.push("--whitespace=nowarn".to_owned());
            args.push("-".to_owned());
            self.run_unit(&args, Some(patch.to_vec()))
        };
        match attempt(true) {
            Ok(()) => Ok(()),
            Err(three_way) => attempt(false).map_err(|plain| {
                Error::other(format!(
                    "git apply --3way failed:\n{three_way}\n\ngit apply failed:\n{plain}"
                ))
            }),
        }
    }

    /// Commit with the message on stdin (`-F -`). This keeps multi-line
    /// messages, quotes and non-ASCII text away from shell-style quoting.
    pub fn commit(&self, message: &str, options: CommitOptions) -> Result<()> {
        let mut args = vec!["commit".to_owned(), "-F".to_owned(), "-".to_owned()];
        if options.amend {
            args.push("--amend".to_owned());
        }
        if options.allow_empty {
            args.push("--allow-empty".to_owned());
        }
        if options.signoff {
            args.push("--signoff".to_owned());
        }
        self.run_unit(&args, Some(message.as_bytes().to_vec()))
    }

    pub fn checkout_branch(&self, name: &str) -> Result<()> {
        let name = checked_ref(name, "branch")?;
        // Trailing `--` keeps a same-named file from being checked out.
        let args = vec!["checkout".to_owned(), name.to_owned(), "--".to_owned()];
        self.run_unit(&args, None)
    }

    /// `git checkout --detach <commit>`.
    ///
    /// [`Git::checkout_branch`] appends `--`, which makes git treat the
    /// argument as a pathspec. An object id must not go through that form.
    pub fn checkout_detached(&self, commit: &Oid) -> Result<()> {
        let id = commit.to_string();
        let args = vec!["checkout".to_owned(), "--detach".to_owned(), id];
        self.run_unit(&args, None)
    }

    pub fn create_branch(
        &self,
        name: &str,
        start_point: Option<&str>,
        checkout: bool,
    ) -> Result<()> {
        let name = checked_ref(name, "branch")?;
        let start_point = start_point
            .map(|start| checked_ref(start, "start point"))
            .transpose()?;
        let mut args = if checkout {
            vec!["switch".to_owned(), "-c".to_owned(), name.to_owned()]
        } else {
            vec!["branch".to_owned(), "--".to_owned(), name.to_owned()]
        };
        if let Some(start_point) = start_point {
            args.push(start_point.to_owned());
        }
        self.run_unit(&args, None)
    }

    pub fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        let name = checked_ref(name, "branch")?;
        let flag = if force { "-D" } else { "-d" };
        let args = vec![
            "branch".to_owned(),
            flag.to_owned(),
            "--".to_owned(),
            name.to_owned(),
        ];
        self.run_unit(&args, None)
    }

    /// Check out submodules at the commits the superproject records.
    ///
    /// An empty `paths` updates every submodule. `init` clones and registers
    /// uninitialised ones first. A path list is passed after `--`, so a path
    /// that looks like an option cannot be read as one.
    pub fn submodule_update(&self, paths: &[impl AsRef<Path>], init: bool) -> Result<()> {
        let paths = collect_paths(paths);
        let mut args = vec!["submodule".to_owned(), "update".to_owned()];
        if init {
            args.push("--init".to_owned());
        }
        if !paths.is_empty() {
            args.push("--".to_owned());
            args.extend(paths.iter().map(|path| path_string(path)));
        }
        self.run_unit(&args, None)
    }

    /// Add a linked worktree at `path`.
    ///
    /// The three forms mirror git's: an existing `branch`, a `new_branch`
    /// starting at `commit`, or `commit` alone for a detached checkout.
    pub fn worktree_add(
        &self,
        path: &Path,
        branch: Option<&str>,
        new_branch: Option<&str>,
        commit: Option<Oid>,
    ) -> Result<()> {
        let path = checked_worktree_path(path)?;
        let branch = branch.map(|name| checked_ref(name, "branch")).transpose()?;
        let new_branch = new_branch
            .map(|name| checked_ref(name, "branch"))
            .transpose()?;

        let mut args = vec!["worktree".to_owned(), "add".to_owned()];
        match (branch, new_branch, commit) {
            (Some(branch), None, None) => {
                args.push("--".to_owned());
                args.push(path_string(path));
                args.push(branch.to_owned());
            }
            (None, Some(new_branch), Some(commit)) => {
                args.push("-b".to_owned());
                args.push(new_branch.to_owned());
                args.push("--".to_owned());
                args.push(path_string(path));
                args.push(commit.to_string());
            }
            (None, None, Some(commit)) => {
                args.push("--detach".to_owned());
                args.push("--".to_owned());
                args.push(path_string(path));
                args.push(commit.to_string());
            }
            _ => {
                return Err(Error::other(
                    "worktree add needs an existing branch, a new branch at a commit, or a detached commit",
                ));
            }
        }
        self.run_unit(&args, None)
    }

    /// Remove the linked worktree checked out at `path`. `force` also drops a
    /// dirty worktree; a locked one still has to be unlocked first.
    pub fn worktree_remove(&self, path: &Path, force: bool) -> Result<()> {
        let path = checked_worktree_path(path)?;
        let mut args = vec!["worktree".to_owned(), "remove".to_owned()];
        if force {
            args.push("--force".to_owned());
        }
        args.push("--".to_owned());
        args.push(path_string(path));
        self.run_unit(&args, None)
    }

    /// Drop registrations of worktrees whose checkout directory is gone.
    pub fn worktree_prune(&self) -> Result<()> {
        let args = vec!["worktree".to_owned(), "prune".to_owned()];
        self.run_unit(&args, None)
    }

    /// Write one commit-graph covering every reachable commit, with changed-path
    /// Bloom filters so path-limited history and blame can use them too.
    ///
    /// This is `git commit-graph write --reachable --changed-paths`; the caller
    /// decides when the user asked for it.
    pub fn write_commit_graph(&self) -> Result<()> {
        let args = vec![
            "commit-graph".to_owned(),
            "write".to_owned(),
            "--reachable".to_owned(),
            "--changed-paths".to_owned(),
        ];
        self.run_unit(&args, None)
    }

    /// `remote == None` fetches every remote.
    pub fn fetch(&self, remote: Option<&str>, prune: bool) -> Result<()> {
        let remote = remote
            .map(|remote| checked_ref(remote, "remote"))
            .transpose()?;
        let mut args = vec!["fetch".to_owned(), "--progress".to_owned()];
        if prune {
            args.push("--prune".to_owned());
        }
        match remote {
            Some(remote) => args.push(remote.to_owned()),
            None => args.push("--all".to_owned()),
        }
        self.run_unit(&args, None)
    }

    /// `git fetch --unshallow`: fill in the history below a shallow clone's
    /// boundary. Only meaningful for a shallow clone, which is the only place
    /// the UI offers it.
    pub fn fetch_unshallow(&self) -> Result<()> {
        self.run_unit(
            &[
                "fetch".to_owned(),
                "--unshallow".to_owned(),
                "--progress".to_owned(),
            ],
            None,
        )
    }

    /// Pull the current branch's upstream. `None` keeps the user's configured
    /// `pull.rebase`; `Some` forces rebase or merge for this call.
    ///
    /// A pull that stops on conflicts leaves a merge or rebase in progress, so
    /// it succeeds like [`Git::merge`] does; only other failures are errors.
    pub fn pull(&self, rebase: Option<bool>) -> Result<()> {
        let mut args = vec![
            "pull".to_owned(),
            "--progress".to_owned(),
            // A GUI has no editor; never leave a merge message half-written.
            "--no-edit".to_owned(),
        ];
        match rebase {
            Some(true) => args.push("--rebase".to_owned()),
            Some(false) => args.push("--no-rebase".to_owned()),
            None => {}
        }
        let result = self.run_unit(&args, None);
        self.finish_pull(result)
    }

    /// Turn a pull that stopped on conflicts into the normal, resumable
    /// outcome [`Git::merge`] already reports.
    ///
    /// The repository is then in a merge or rebase the user resolves, so the
    /// exit code alone must not read as failure. Only exit 1 with that state
    /// recorded counts as a stop: a network or authentication failure, a
    /// refused `--ff-only` and a dirty-worktree refusal leave no such state and
    /// stay errors.
    pub(super) fn finish_pull(&self, result: Result<()>) -> Result<()> {
        match result {
            Ok(()) => Ok(()),
            Err(error @ Error::GitCommand { code: Some(1), .. }) => match self.state()? {
                RepoState::Merging | RepoState::Rebasing => Ok(()),
                _ => Err(error),
            },
            Err(error) => Err(error),
        }
    }

    /// Push `branch` to `remote`. A branch without a remote is rejected
    /// because git would read the branch name as the repository.
    pub fn push(
        &self,
        remote: Option<&str>,
        branch: Option<&str>,
        set_upstream: bool,
        force_with_lease: bool,
    ) -> Result<()> {
        let remote = remote
            .map(|remote| checked_ref(remote, "remote"))
            .transpose()?;
        let branch = branch
            .map(|branch| checked_ref(branch, "branch"))
            .transpose()?;
        if remote.is_none() {
            if branch.is_some() {
                return Err(Error::other("push needs a remote when a branch is given"));
            }
            if set_upstream || force_with_lease {
                return Err(Error::other(
                    "push needs a remote for --set-upstream or --force-with-lease",
                ));
            }
        }
        let mut args = vec!["push".to_owned(), "--progress".to_owned()];
        if set_upstream {
            args.push("--set-upstream".to_owned());
        }
        if force_with_lease {
            args.push("--force-with-lease".to_owned());
        }
        if let Some(remote) = remote {
            args.push(remote.to_owned());
        }
        if let Some(branch) = branch {
            args.push(branch.to_owned());
        }
        self.run_unit(&args, None)
    }

    pub fn stash_push(&self, message: Option<&str>, include_untracked: bool) -> Result<()> {
        self.stash_push_with(message, include_untracked, &[])
    }

    /// Stash only the changes at `paths`, as `git stash push -- <paths>`.
    ///
    /// An empty `paths` stashes everything, like
    /// [`stash_push`](Self::stash_push). Other worktree changes are left alone.
    pub fn stash_push_paths(
        &self,
        message: Option<&str>,
        include_untracked: bool,
        paths: &[impl AsRef<Path>],
    ) -> Result<()> {
        let paths = collect_paths(paths);
        self.stash_push_with(message, include_untracked, &paths)
    }

    fn stash_push_with(
        &self,
        message: Option<&str>,
        include_untracked: bool,
        paths: &[PathBuf],
    ) -> Result<()> {
        let mut args = vec!["stash".to_owned(), "push".to_owned()];
        if include_untracked {
            args.push("-u".to_owned());
        }
        if let Some(message) = message {
            args.push("-m".to_owned());
            args.push(message.to_owned());
        }
        let stdin = if paths.is_empty() {
            None
        } else {
            push_pathspecs(&mut args, paths)
        };
        self.run_unit(&args, stdin)
    }

    pub fn stash_apply(&self, index: usize) -> Result<()> {
        let args = vec!["stash".to_owned(), "apply".to_owned(), stash_ref(index)];
        self.run_unit(&args, None)
    }

    pub fn stash_pop(&self, index: usize) -> Result<()> {
        let args = vec!["stash".to_owned(), "pop".to_owned(), stash_ref(index)];
        self.run_unit(&args, None)
    }

    pub fn stash_drop(&self, index: usize) -> Result<()> {
        let args = vec!["stash".to_owned(), "drop".to_owned(), stash_ref(index)];
        self.run_unit(&args, None)
    }

    /// Parse `git stash list` with NUL separators so messages containing any
    /// punctuation (including spaces and colons) stay intact.
    pub fn stash_list(&self) -> Result<Vec<StashEntry>> {
        let output = self.run(&["stash", "list", "--format=%gd%x00%H%x00%gs"], None)?;
        Ok(parse_stash_list(&String::from_utf8_lossy(&output.stdout)))
    }

    /// The subset of `paths` that the index adds relative to HEAD.
    ///
    /// These have no HEAD content, so `git restore --source=HEAD` cannot put
    /// them back and the caller has to remove them instead. On an unborn branch
    /// `git diff --cached` still works and reports every index entry as added.
    fn added_to_index(&self, paths: &[PathBuf]) -> Result<HashSet<String>> {
        let mut args = vec![
            "diff".to_owned(),
            "--cached".to_owned(),
            "--name-only".to_owned(),
            // NUL separators keep unusual file names intact.
            "-z".to_owned(),
            // A staged rename is a delete plus an add, not a rewrite: its new
            // path must be removed rather than restored.
            "--no-renames".to_owned(),
            "--diff-filter=A".to_owned(),
            "--".to_owned(),
        ];
        args.extend(paths.iter().map(|path| path_string(path)));
        let output = self.run(&args, None)?;
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf8_lossy(entry).into_owned())
            .collect())
    }
}

/// Delete the worktree copy of a path that only existed in the index.
///
/// A path that is already gone is fine: the staged change is discarded either way.
fn remove_worktree_file(workdir: &Path, path: &str) -> Result<()> {
    let absolute = workdir.join(path);
    match std::fs::symlink_metadata(&absolute) {
        // `git rm --cached` can leave a directory behind when a staged file
        // replaced one; remove its contents so no empty tree lingers.
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&absolute)?,
        Ok(_) => std::fs::remove_file(&absolute)?,
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Io(error)),
    }
    Ok(())
}

fn stash_ref(index: usize) -> String {
    format!("stash@{{{index}}}")
}

fn parse_stash_list(text: &str) -> Vec<StashEntry> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\0');
            let reference = fields.next()?;
            let oid: Oid = fields.next()?.parse().ok()?;
            let message = fields.next().unwrap_or_default();
            let index = reference
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse()
                .ok()?;
            Some(StashEntry {
                index,
                message: message.to_owned(),
                oid,
            })
        })
        .collect()
}

fn collect_paths(paths: &[impl AsRef<Path>]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| path.as_ref().to_path_buf())
        .collect()
}

fn path_string(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        // Git accepts `/` on every platform, and pathspec files expect it.
        text.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        text.into_owned()
    }
}

/// Reject paths that git would read as an option. Everything else is left to
/// git so its own message reaches the user.
fn checked_worktree_path(path: &Path) -> Result<&Path> {
    if path.as_os_str().is_empty() {
        return Err(Error::other("worktree path is empty"));
    }
    if path.to_string_lossy().starts_with('-') {
        return Err(Error::other(format!(
            "worktree path must not start with `-`: {}",
            path.display()
        )));
    }
    Ok(path)
}

/// Append path arguments, switching to a NUL-separated pathspec file on stdin
/// when the list would risk the command-line limit. Returns the stdin payload.
fn push_pathspecs(args: &mut Vec<String>, paths: &[PathBuf]) -> Option<Vec<u8>> {
    if paths.len() <= MAX_INLINE_PATHSPECS
        && paths
            .iter()
            .map(|path| path_string(path).len() + 1)
            .sum::<usize>()
            <= MAX_INLINE_PATHSPEC_BYTES
    {
        args.push("--".to_owned());
        args.extend(paths.iter().map(|path| path_string(path)));
        return None;
    }
    args.push("--pathspec-from-file=-".to_owned());
    args.push("--pathspec-file-nul".to_owned());
    let mut stdin = Vec::new();
    for path in paths {
        stdin.extend_from_slice(path_string(path).as_bytes());
        stdin.push(0);
    }
    Some(stdin)
}

fn chunks_by_bytes(paths: &[PathBuf], max_items: usize, max_bytes: usize) -> Vec<&[PathBuf]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, path) in paths.iter().enumerate() {
        let length = path_string(path).len() + 1;
        if index > start && (index - start >= max_items || bytes + length > max_bytes) {
            chunks.push(&paths[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += length;
    }
    if start < paths.len() {
        chunks.push(&paths[start..]);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use crate::test_support::TestRepo;

    fn read(repo: &TestRepo, path: &str) -> String {
        std::fs::read_to_string(repo.path().join(path)).expect("read worktree file")
    }

    #[test]
    fn an_applied_patch_changes_the_worktree() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("base");
        repo.write("a.txt", "two\n");
        let patch_commit = repo.commit("change a");
        let patch = repo
            .git_handle()
            .format_patch(patch_commit)
            .expect("format-patch");
        repo.git(&["reset", "--hard", "HEAD~1"]);

        repo.git_handle()
            .apply_patch_file(&patch)
            .expect("the patch applies to the parent");

        assert_eq!(read(&repo, "a.txt"), "two\n");
        assert!(
            repo.git(&["status", "--porcelain=v2"]).contains("a.txt"),
            "git apply --3way leaves the applied path for the user to review"
        );
    }

    #[test]
    fn a_worktree_at_a_new_branch_checks_it_out() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        let head = repo.commit("base");
        let path = repo
            .path()
            .parent()
            .expect("the repository sits in a temp dir")
            .join("topic-checkout");

        repo.git_handle()
            .worktree_add(&path, None, Some("topic"), Some(head))
            .expect("worktree add");

        assert_eq!(
            std::fs::read_to_string(path.join("a.txt")).expect("the checkout has the file"),
            "one\n"
        );
        assert_eq!(
            repo.git(&["rev-parse", "topic"]).trim(),
            head.to_string(),
            "the new branch starts at the commit"
        );
    }

    #[test]
    fn a_patch_that_does_not_apply_reports_both_attempts() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("base");

        let error = repo
            .git_handle()
            .apply_patch_file(b"this is not a patch\n")
            .expect_err("garbage cannot apply");
        let message = error.to_string();
        assert!(message.contains("git apply --3way failed"), "{message}");
        assert!(message.contains("git apply failed"), "{message}");
    }
}
