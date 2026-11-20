//! Merge and conflict resolution commands.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{Error, Oid, Result};

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

    /// The paths `git merge-tree --write-tree --name-only` reports as conflicts
    /// for a merge of `ours` and `theirs`.
    ///
    /// The command writes the merged tree into the object database but updates
    /// no ref and touches neither the index nor the working tree, so it is safe
    /// to run for a preview. `None` means the installed git predates
    /// `--write-tree` (2.38), or its `--name-only` option is unknown; callers
    /// then fall back to their own classification instead of failing.
    pub fn merge_tree_conflicts(&self, ours: Oid, theirs: Oid) -> Result<Option<Vec<String>>> {
        if self.version()? < (2, 38, 0) {
            return Ok(None);
        }
        let args = [
            "-c".to_owned(),
            // Paths come back as raw UTF-8, so they match the tree paths in
            // the comparison.
            "core.quotePath=false".to_owned(),
            "merge-tree".to_owned(),
            "--write-tree".to_owned(),
            "--name-only".to_owned(),
            ours.to_string(),
            theirs.to_string(),
        ];
        let output = self.run_output(&args, None, true)?;
        // Exit 1 is the expected answer for a conflicted merge.
        if !matches!(output.status.code(), Some(0) | Some(1)) {
            return Ok(None);
        }
        Ok(Some(parse_merge_tree_conflicts(&output.stdout)))
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
    ///
    /// `--cleanup=strip` is what git applies to an edited message, and it
    /// removes the `# Conflicts:` block git wrote into `MERGE_MSG`. Without it,
    /// `git commit --no-edit` uses the `whitespace` cleanup and stores those
    /// comment lines in the merge commit's body. git does the stripping itself
    /// so `core.commentChar` keeps working.
    pub fn merge_continue(&self) -> Result<()> {
        self.run(&["commit", "--no-edit", "--cleanup=strip"], None)?;
        Ok(())
    }

    /// Run the user's configured merge tool for one conflicted path.
    ///
    /// `--no-prompt` keeps a GUI from waiting on a terminal question. git still
    /// decides whether the tool resolved the path, and stages it when it did.
    ///
    /// A run that never reaches a tool (no `merge.tool` configured, or every
    /// fallback missing) exits non-zero after leaving git's stage files in the
    /// working tree. Those are removed here so they cannot show up as
    /// untracked files on top of a failed attempt.
    pub fn mergetool(&self, path: &str, gui: bool) -> Result<()> {
        let merged = self.workdir().join(path);
        let directory = merged.parent().unwrap_or_else(|| self.workdir());
        let before = directory_names(directory);
        let mut args = vec!["mergetool".to_owned(), "--no-prompt".to_owned()];
        if gui {
            args.push("--gui".to_owned());
        }
        args.push("--".to_owned());
        args.push(path.to_owned());
        let result = self.run(&args, None);
        if result.is_err() {
            remove_mergetool_leftovers(directory, &merged, &before);
        }
        result.map(|_| ())
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

/// The entry names of `directory`, or an empty set when it cannot be read.
fn directory_names(directory: &Path) -> BTreeSet<OsString> {
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name())
                .collect()
        })
        .unwrap_or_default()
}

/// Deletes the stage files a failed merge tool run left beside `merged`.
///
/// git writes `<stem>_BASE_<pid><ext>` (and `_LOCAL_`, `_REMOTE_`, `_BACKUP_`)
/// next to the file while a tool runs and removes them when the run ends. A
/// run that never reached a tool leaves them behind, so only names that were
/// absent before the run and carry one of those markers are removed.
fn remove_mergetool_leftovers(directory: &Path, merged: &Path, before: &BTreeSet<OsString>) {
    let Some(stem) = merged.file_stem().and_then(|stem| stem.to_str()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if before.contains(&name) {
            continue;
        }
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_mergetool_temp(name, stem) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether `name` is one of the stage files git writes for `merged`'s stem.
///
/// The pattern is `<stem>_<MARKER>_<pid><ext>`: the stem keeps a neighbour of
/// another file from matching, and the digits after the marker are git's pid.
fn is_mergetool_temp(name: &str, stem: &str) -> bool {
    let Some(rest) = name.strip_prefix(stem) else {
        return false;
    };
    ["_BACKUP_", "_BASE_", "_LOCAL_", "_REMOTE_"]
        .iter()
        .any(|marker| {
            let Some(after) = rest.strip_prefix(marker) else {
                return false;
            };
            let digits = after
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            if digits.is_empty() {
                return false;
            }
            let tail = &after[digits.len()..];
            tail.is_empty() || tail.starts_with('.')
        })
}

/// Reads the conflicted file names from `git merge-tree --write-tree
/// --name-only`'s stdout.
///
/// The first line is the merged tree's id, the file names follow, and a blank
/// line ends that section before git's informational messages.
fn parse_merge_tree_conflicts(stdout: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(stdout);
    let mut lines = text.lines();
    // The tree id is of no use to the caller.
    if lines.next().is_none() {
        return Vec::new();
    }
    let mut conflicts = Vec::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        conflicts.push(line.to_owned());
    }
    conflicts
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use tempfile::TempDir;

    use super::{
        directory_names, is_mergetool_temp, parse_merge_tree_conflicts, remove_mergetool_leftovers,
    };

    #[test]
    fn merge_tree_names_end_at_the_information_section() {
        let output = b"1234567890abcdef\nsrc/a.txt\ndir with space/b.txt\n\nAuto-merging src/a.txt\nCONFLICT (content): Merge conflict in src/a.txt\n";
        assert_eq!(
            parse_merge_tree_conflicts(output),
            vec!["src/a.txt", "dir with space/b.txt"]
        );
        assert!(parse_merge_tree_conflicts(b"1234567890abcdef\n").is_empty());
        assert!(parse_merge_tree_conflicts(b"").is_empty());
    }

    #[test]
    fn mergetool_temp_names_carry_the_stem_marker_and_pid() {
        assert!(is_mergetool_temp("greeting_BACKUP_87732.txt", "greeting"));
        assert!(is_mergetool_temp("greeting_BASE_87732.txt", "greeting"));
        assert!(is_mergetool_temp("greeting_LOCAL_87732.txt", "greeting"));
        assert!(is_mergetool_temp("greeting_REMOTE_87732.txt", "greeting"));
        assert!(
            is_mergetool_temp("greeting_LOCAL_87732", "greeting"),
            "a file without an extension has no tail"
        );
        assert!(
            !is_mergetool_temp("greeting_BASE.txt", "greeting"),
            "the pid is what tells git's stage file from a user's file"
        );
        assert!(
            !is_mergetool_temp("other_BASE_87732.txt", "greeting"),
            "another file's stage file must not match this stem"
        );
        assert!(!is_mergetool_temp("greeting.txt", "greeting"));
        assert!(!is_mergetool_temp("greeting_backup_87732.txt", "greeting"));
    }

    #[test]
    fn cleanup_removes_only_the_files_the_failed_run_created() {
        let dir = TempDir::new().expect("temporary directory");
        let merged = dir.path().join("greeting.txt");
        fs::write(&merged, "x\n").expect("write the conflicted file");
        // Present before the run: neither may be touched.
        fs::write(dir.path().join("greeting_BASE_111.txt"), "old\n").expect("write");
        fs::write(dir.path().join("other_BASE_999.txt"), "old\n").expect("write");
        let before = directory_names(dir.path());

        for name in [
            "greeting_BACKUP_42.txt",
            "greeting_BASE_42.txt",
            "greeting_LOCAL_42.txt",
            "greeting_REMOTE_42.txt",
            // A neighbour's stage file and an unrelated file are not ours.
            "other_BASE_42.txt",
            "notes.txt",
        ] {
            fs::write(dir.path().join(name), "temp\n").expect("write leftover");
        }

        remove_mergetool_leftovers(dir.path(), &merged, &before);

        let left: BTreeSet<String> = fs::read_dir(dir.path())
            .expect("read the directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        let expected: BTreeSet<String> = [
            "greeting.txt",
            "greeting_BASE_111.txt",
            "other_BASE_999.txt",
            "other_BASE_42.txt",
            "notes.txt",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(left, expected, "only the run's own stage files are removed");
    }
}
