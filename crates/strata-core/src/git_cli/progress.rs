//! Streaming variants of the network commands, so the UI can show git's
//! `--progress` output while fetch/pull/push runs.
//!
//! The argument shape mirrors the non-streaming methods in `ops.rs`, which stay
//! the canonical form of each command; only the way stderr is consumed differs.

use super::{Git, checked_ref};
use crate::Result;

impl Git {
    /// [`Git::fetch`] with stderr delivered line by line. Each line is trimmed
    /// and comes with the percentage git reported in it, if any.
    pub fn fetch_with_progress(
        &self,
        remote: Option<&str>,
        prune: bool,
        on_line: impl FnMut(&str, Option<u8>),
    ) -> Result<()> {
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
        self.stream(&args, on_line)
    }

    /// [`Git::pull`] with stderr delivered line by line.
    pub fn pull_with_progress(
        &self,
        rebase: Option<bool>,
        on_line: impl FnMut(&str, Option<u8>),
    ) -> Result<()> {
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
        // A stop on conflicts is a normal outcome here, as for `Git::pull`.
        let result = self.stream(&args, on_line);
        self.finish_pull(result)
    }

    /// [`Git::push`] with stderr delivered line by line.
    pub fn push_with_progress(
        &self,
        remote: Option<&str>,
        branch: Option<&str>,
        set_upstream: bool,
        force_with_lease: bool,
        on_line: impl FnMut(&str, Option<u8>),
    ) -> Result<()> {
        let remote = remote
            .map(|remote| checked_ref(remote, "remote"))
            .transpose()?;
        let branch = branch
            .map(|branch| checked_ref(branch, "branch"))
            .transpose()?;
        if remote.is_none() {
            if branch.is_some() {
                return Err(crate::Error::other(
                    "push needs a remote when a branch is given",
                ));
            }
            if set_upstream || force_with_lease {
                return Err(crate::Error::other(
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
        self.stream(&args, on_line)
    }

    fn stream(&self, args: &[String], mut on_line: impl FnMut(&str, Option<u8>)) -> Result<()> {
        self.run_streaming(args, |line| {
            let text = line.text.trim();
            if !text.is_empty() {
                on_line(text, progress_percent(text));
            }
        })
        .map(|_| ())
    }

    /// [`Git::write_commit_graph`] with git's progress lines delivered while
    /// the command runs. Cancelling kills the child and returns `Ok(false)`.
    pub fn write_commit_graph_with_progress(
        &self,
        cancelled: &(impl Fn() -> bool + Send + Sync),
        mut on_line: impl FnMut(&str, Option<u8>),
    ) -> Result<bool> {
        let args = [
            "commit-graph",
            "write",
            "--reachable",
            "--changed-paths",
            "--progress",
        ];
        self.run_stderr_cancellable(&args, cancelled, |text| {
            let text = text.trim();
            if !text.is_empty() {
                on_line(text, progress_percent(text));
            }
        })
    }
}

/// The percentage carried by a progress line.
///
/// Git writes `Receiving objects:  45% (450/1000)` and terminates counters with
/// `Writing objects: 100% (3/3), done.`, so the number directly before the first
/// `%` is the value the UI wants. Remote-side lines keep their `remote: ` prefix,
/// which does not affect the lookup.
fn progress_percent(line: &str) -> Option<u8> {
    let percent = line.find('%')?;
    let digits = &line[..percent];
    let start = digits
        .rfind(|ch: char| !ch.is_ascii_digit())
        .map_or(0, |index| index + 1);
    let value: u32 = digits[start..].parse().ok()?;
    u8::try_from(value.min(100)).ok()
}

#[cfg(test)]
mod tests {
    use super::progress_percent;

    #[test]
    fn parses_counter_lines() {
        assert_eq!(
            progress_percent("Receiving objects:  45% (450/1000)"),
            Some(45)
        );
        assert_eq!(
            progress_percent("Writing objects: 100% (3/3), done."),
            Some(100)
        );
        assert_eq!(
            progress_percent("remote: Compressing objects:  12% (1/8)"),
            Some(12)
        );
        assert_eq!(
            progress_percent("Resolving deltas:   7% (7/100), done."),
            Some(7)
        );
        assert_eq!(
            progress_percent("remote: Counting objects: 100% (4/4)"),
            Some(100)
        );
    }

    #[test]
    fn lines_without_a_counter_have_no_percentage() {
        assert_eq!(
            progress_percent("Total 3 (delta 0), reused 0 (delta 0)"),
            None
        );
        assert_eq!(progress_percent("From /tmp/remote.git"), None);
        assert_eq!(
            progress_percent("  9d03397..7945325  main -> origin/main"),
            None
        );
        assert_eq!(progress_percent(""), None);
        assert_eq!(progress_percent("%"), None);
    }

    #[test]
    fn clamps_out_of_range_values() {
        assert_eq!(progress_percent("Receiving objects: 250% (5/2)"), Some(100));
    }
}
