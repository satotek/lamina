//! Thin wrappers for the mutating git commands the UI needs.

use std::path::{Path, PathBuf};

use crate::{Error, Oid, Result};

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
        self.run_unit(&args, Some(patch.as_bytes().to_vec()))
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

    /// Pull the current branch's upstream. `None` keeps the user's configured
    /// `pull.rebase`; `Some` forces rebase or merge for this call.
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
        self.run_unit(&args, None)
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
        let mut args = vec!["stash".to_owned(), "push".to_owned()];
        if include_untracked {
            args.push("-u".to_owned());
        }
        if let Some(message) = message {
            args.push("-m".to_owned());
            args.push(message.to_owned());
        }
        self.run_unit(&args, None)
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
