//! Merge and conflict resolution commands.

use std::path::PathBuf;

use crate::{Error, Result};

use super::{Git, checked_ref};

/// Keep inline pathspecs well below the ~32 KiB Windows command-line limit.
const MAX_INLINE_PATHSPECS: usize = 64;
const MAX_INLINE_PATHSPEC_BYTES: usize = 8 * 1024;

impl Git {
    /// Merge `branch` into the current branch.
    ///
    /// A merge that stops on conflicts is a normal outcome, not a failure: the
    /// caller shows the conflict state and lets the user resolve it. Any other
    /// non-zero exit, such as a refusal to overwrite local changes, stays an
    /// error.
    pub fn merge(&self, branch: &str, no_ff: bool) -> Result<()> {
        let branch = checked_ref(branch, "branch")?;
        let mut args = vec![
            "merge".to_owned(),
            // A GUI has no editor for the prepared merge message.
            "--no-edit".to_owned(),
        ];
        if no_ff {
            args.push("--no-ff".to_owned());
        }
        args.push(branch.to_owned());

        match self.run_unit(&args, None) {
            Ok(()) => Ok(()),
            // `git merge` exits 1 both for conflicts and for a refusal to start
            // one, so the merge state, not the code alone, distinguishes them.
            // A fatal error (exit 128), such as a merge already in progress,
            // still has MERGE_HEAD on disk and must not pass as success.
            Err(error @ Error::GitCommand { code: Some(1), .. }) => {
                if self.in_progress("MERGE_HEAD")? {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// `git merge --abort`, restoring HEAD and the worktree of the pre-merge state.
    pub fn merge_abort(&self) -> Result<()> {
        self.run(&["merge", "--abort"], None)?;
        Ok(())
    }

    /// Take one side of a conflicted `path` and stage it.
    ///
    /// A side that deleted the file has no version to check out, so that case
    /// resolves the path by removing it from the index instead.
    pub fn checkout_side(&self, path: &str, ours: bool) -> Result<()> {
        if !self.has_stage(path, if ours { 2 } else { 3 })? {
            self.run(&["rm", "--quiet", "--", path], None)?;
            return Ok(());
        }
        let flag = if ours { "--ours" } else { "--theirs" };
        self.run(&["checkout", flag, "--", path], None)?;
        self.run(&["add", "--", path], None)?;
        Ok(())
    }

    /// Stage paths the user resolved by editing, clearing their unmerged entries.
    pub fn mark_resolved(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["add".to_owned()];
        let stdin = push_pathspecs(&mut args, paths);
        self.run_unit(&args, stdin)
    }

    /// Commit a stopped merge with the message git prepared in `MERGE_MSG`.
    pub fn merge_continue(&self) -> Result<()> {
        self.run(&["commit", "--no-edit"], None)?;
        Ok(())
    }

    /// Run the user's configured merge tool for one conflicted path.
    ///
    /// `--no-prompt` keeps a GUI from waiting on a terminal question. git still
    /// decides whether the tool resolved the path, and stages it when it did.
    pub fn mergetool(&self, path: &str) -> Result<()> {
        let args = vec![
            "mergetool".to_owned(),
            "--no-prompt".to_owned(),
            "--".to_owned(),
            path.to_owned(),
        ];
        self.run(&args, None)?;
        Ok(())
    }

    /// Whether `path` has an unmerged index entry in `stage` (1, 2 or 3).
    fn has_stage(&self, path: &str, stage: u32) -> Result<bool> {
        // `ls-files --stage` is the only index read that reports the stage
        // without having to resolve the conflict first.
        let output = self.run(&["ls-files", "--stage", "-z", "--", path], None)?;
        Ok(stage_in_listing(&output.stdout, path.as_bytes(), stage))
    }

    /// Whether git records `name` in the git dir, e.g. `MERGE_HEAD`.
    fn in_progress(&self, name: &str) -> Result<bool> {
        // `--git-path` resolves per-worktree paths the way git itself does.
        let output = self.run(&["rev-parse", "--git-path", name], None)?;
        let text = String::from_utf8_lossy(&output.stdout);
        let path = PathBuf::from(text.trim());
        let path = if path.is_absolute() {
            path
        } else {
            self.workdir().join(path)
        };
        Ok(path.is_file())
    }
}

/// Look for `path` in one NUL-separated `git ls-files --stage -z` record set.
fn stage_in_listing(output: &[u8], path: &[u8], stage: u32) -> bool {
    output.split(|byte| *byte == 0).any(|record| {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return false;
        };
        // Paths may contain spaces, so compare them as bytes after the tab
        // instead of splitting the whole record on whitespace.
        if &record[tab + 1..] != path {
            return false;
        }
        record[..tab]
            .split(|byte| *byte == b' ')
            .nth(2)
            .is_some_and(|value| value.len() == 1 && value[0] == b'0' + stage as u8)
    })
}

/// Append path arguments, switching to a NUL-separated pathspec file on stdin
/// when the list would risk the command-line limit. Returns the stdin payload.
fn push_pathspecs(args: &mut Vec<String>, paths: &[String]) -> Option<Vec<u8>> {
    if paths.len() <= MAX_INLINE_PATHSPECS
        && paths.iter().map(|path| path.len() + 1).sum::<usize>() <= MAX_INLINE_PATHSPEC_BYTES
    {
        args.push("--".to_owned());
        args.extend(paths.iter().cloned());
        return None;
    }
    args.push("--pathspec-from-file=-".to_owned());
    args.push("--pathspec-file-nul".to_owned());
    let mut stdin = Vec::new();
    for path in paths {
        stdin.extend_from_slice(path.as_bytes());
        stdin.push(0);
    }
    Some(stdin)
}
