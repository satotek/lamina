//! Details git records in the git dir while a multi-step operation is stopped.
//!
//! `Repo::state()` only names which operation is running; this module reads the
//! state files (`rebase-merge/`, `rebase-apply/`, `MERGE_HEAD`, `MERGE_MSG`,
//! `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `sequencer/todo`, `BISECT_LOG`,
//! `refs/bisect/*`, `BISECT_EXPECTED_REV`) and resolves commit summaries and ref
//! names via `gix`. Every detail field is optional so partial or missing files
//! never prevent the banner from rendering.

use std::io::ErrorKind;
use std::path::Path;

use gix::bstr::ByteSlice;

use super::Repo;
use crate::{Error, Oid, Result};

/// A commit involved in an in-progress operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProgressCommit {
    /// Full object id when it could be parsed or resolved via `gix`.
    pub id: Option<Oid>,
    /// Abbreviated object id (7 hex chars when full, or the raw prefix from a todo line).
    pub short_id: String,
    /// First line of the commit message, if readable.
    pub summary: Option<String>,
}

/// The target commit and optional ref name for a rebase `onto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseOnto {
    pub id: Option<Oid>,
    pub short_id: String,
    /// Short name of a ref pointing at `id`, preferring local branches over
    /// remote-tracking branches and tags.
    pub ref_name: Option<String>,
}

/// What git knows about the multi-step operation currently in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InProgressDetails {
    Rebase {
        /// Branch being rebased (`head-name`), shortened (e.g. `feature-x`).
        /// `None` when rebasing a detached HEAD or when `head-name` is absent.
        branch: Option<String>,
        /// Target commit (`onto`) and an optional ref name pointing at it.
        onto: Option<RebaseOnto>,
        /// Current step (`msgnum` for merge backend, `next` for apply backend).
        step: Option<usize>,
        /// Total steps (`end` for merge backend, `last` for apply backend).
        total_steps: Option<usize>,
        /// Commit currently being applied (`stopped-sha`, current todo line, or
        /// `original-commit` in the apply backend).
        current_commit: Option<InProgressCommit>,
    },
    Merge {
        /// Commit(s) recorded in `MERGE_HEAD`.
        heads: Vec<InProgressCommit>,
        /// Branch, tag or commit name parsed from `MERGE_MSG`'s first line.
        source: Option<String>,
    },
    CherryPick {
        /// Commit recorded in `CHERRY_PICK_HEAD`.
        commit: Option<InProgressCommit>,
        /// Remaining steps in `.git/sequencer/todo` when in a sequence.
        remaining: Option<usize>,
    },
    Revert {
        /// Commit recorded in `REVERT_HEAD`.
        commit: Option<InProgressCommit>,
        /// Remaining steps in `.git/sequencer/todo` when in a sequence.
        remaining: Option<usize>,
    },
    Bisect {
        /// Commits marked good (from `BISECT_LOG`, falling back to `refs/bisect/*`).
        good_count: Option<usize>,
        /// Commits marked bad (from `BISECT_LOG`, falling back to `refs/bisect/*`).
        bad_count: Option<usize>,
        /// Commit currently checked out for testing (`BISECT_EXPECTED_REV` or `HEAD`).
        current_commit: Option<InProgressCommit>,
    },
    ApplyingPatches {
        /// Current patch index (`rebase-apply/next`).
        step: Option<usize>,
        /// Total patch count (`rebase-apply/last`).
        total_steps: Option<usize>,
        /// Subject line of the current patch.
        subject: Option<String>,
    },
}

impl Repo {
    /// Details git has recorded for the operation currently in progress, or
    /// `None` when the repository state is normal.
    ///
    /// Missing or partial state files in the git dir leave the corresponding
    /// fields as `None` rather than failing the read.
    pub fn in_progress_details(&self) -> Result<Option<InProgressDetails>> {
        let repo = self.local();
        let Some(state) = repo.state() else {
            return Ok(None);
        };
        let git_dir = repo.git_dir();

        let details = match state {
            gix::state::InProgress::Rebase
            | gix::state::InProgress::RebaseInteractive
            | gix::state::InProgress::ApplyMailboxRebase => rebase_details(&repo, git_dir)?,
            gix::state::InProgress::Merge => merge_details(&repo, git_dir)?,
            gix::state::InProgress::CherryPick | gix::state::InProgress::CherryPickSequence => {
                sequencer_details(&repo, git_dir, "CHERRY_PICK_HEAD", true)?
            }
            gix::state::InProgress::Revert | gix::state::InProgress::RevertSequence => {
                sequencer_details(&repo, git_dir, "REVERT_HEAD", false)?
            }
            gix::state::InProgress::Bisect => bisect_details(&repo, git_dir)?,
            gix::state::InProgress::ApplyMailbox => am_details(git_dir)?,
        };

        Ok(Some(details))
    }
}

fn rebase_details(repo: &gix::Repository, git_dir: &Path) -> Result<InProgressDetails> {
    let merge_dir = git_dir.join("rebase-merge");
    if merge_dir.is_dir() {
        let branch =
            read_trimmed(&merge_dir.join("head-name"))?.and_then(|raw| normalize_head_name(&raw));
        let onto = read_trimmed(&merge_dir.join("onto"))?.and_then(|raw| rebase_onto(repo, &raw));
        let step = read_usize(&merge_dir.join("msgnum"))?;
        let total_steps = read_usize(&merge_dir.join("end"))?;

        let stopped = read_trimmed(&merge_dir.join("stopped-sha"))?
            .and_then(|raw| commit_from_spec(repo, &raw, None));
        let current_commit = match stopped {
            Some(commit) => Some(commit),
            None => read_lossy(&merge_dir.join("git-rebase-todo"))?
                .and_then(|todo| parse_rebase_todo_commit(repo, &todo)),
        };

        return Ok(InProgressDetails::Rebase {
            branch,
            onto,
            step,
            total_steps,
            current_commit,
        });
    }

    let apply_dir = git_dir.join("rebase-apply");
    let branch =
        read_trimmed(&apply_dir.join("head-name"))?.and_then(|raw| normalize_head_name(&raw));
    let onto = read_trimmed(&apply_dir.join("onto"))?.and_then(|raw| rebase_onto(repo, &raw));
    let step = read_usize(&apply_dir.join("next"))?;
    let total_steps = read_usize(&apply_dir.join("last"))?;
    let fallback_summary = read_first_non_empty_line(&apply_dir.join("final-commit"))?;
    let current_commit = read_trimmed(&apply_dir.join("original-commit"))?
        .and_then(|raw| commit_from_spec(repo, &raw, fallback_summary));

    Ok(InProgressDetails::Rebase {
        branch,
        onto,
        step,
        total_steps,
        current_commit,
    })
}

fn merge_details(repo: &gix::Repository, git_dir: &Path) -> Result<InProgressDetails> {
    let heads = match read_lossy(&git_dir.join("MERGE_HEAD"))? {
        Some(raw) => raw
            .lines()
            .filter_map(|line| commit_from_spec(repo, line.trim(), None))
            .collect(),
        None => Vec::new(),
    };
    let source = read_lossy(&git_dir.join("MERGE_MSG"))?.and_then(|raw| parse_merge_source(&raw));
    Ok(InProgressDetails::Merge { heads, source })
}

fn sequencer_details(
    repo: &gix::Repository,
    git_dir: &Path,
    head_file: &str,
    is_cherry_pick: bool,
) -> Result<InProgressDetails> {
    let commit =
        read_trimmed(&git_dir.join(head_file))?.and_then(|raw| commit_from_spec(repo, &raw, None));
    let remaining = read_lossy(&git_dir.join("sequencer").join("todo"))?.and_then(|raw| {
        let count = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .count();
        (count > 0).then_some(count)
    });

    if is_cherry_pick {
        Ok(InProgressDetails::CherryPick { commit, remaining })
    } else {
        Ok(InProgressDetails::Revert { commit, remaining })
    }
}

fn bisect_details(repo: &gix::Repository, git_dir: &Path) -> Result<InProgressDetails> {
    let (bad_term, good_term) = bisect_terms(git_dir)?;

    let mut log_good = 0usize;
    let mut log_bad = 0usize;
    let mut saw_log = false;
    if let Some(log) = read_lossy(&git_dir.join("BISECT_LOG"))? {
        let bad_prefix = format!("# {bad_term}:");
        let good_prefix = format!("# {good_term}:");
        for line in log.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with(&bad_prefix) {
                log_bad += 1;
                saw_log = true;
            } else if trimmed.starts_with(&good_prefix) {
                log_good += 1;
                saw_log = true;
            }
        }
    }

    let (ref_good, ref_bad) = count_bisect_refs(repo, &bad_term, &good_term);
    let saw_refs = ref_good + ref_bad > 0;

    let good_count = if saw_log {
        Some(log_good.max(ref_good))
    } else if saw_refs {
        Some(ref_good)
    } else {
        None
    };
    let bad_count = if saw_log {
        Some(log_bad.max(ref_bad))
    } else if saw_refs {
        Some(ref_bad)
    } else {
        None
    };

    let expected = read_trimmed(&git_dir.join("BISECT_EXPECTED_REV"))?
        .and_then(|raw| commit_from_spec(repo, &raw, None));
    let current_commit = match expected {
        Some(commit) => Some(commit),
        None => repo
            .head()
            .ok()
            .and_then(|head| head.id().map(|id| commit_from_oid(repo, id.detach()))),
    };

    Ok(InProgressDetails::Bisect {
        good_count,
        bad_count,
        current_commit,
    })
}

fn am_details(git_dir: &Path) -> Result<InProgressDetails> {
    let apply_dir = git_dir.join("rebase-apply");
    let step = read_usize(&apply_dir.join("next"))?;
    let total_steps = read_usize(&apply_dir.join("last"))?;

    let mut subject = read_first_non_empty_line(&apply_dir.join("final-commit"))?;
    if subject.is_none() {
        subject = read_lossy(&apply_dir.join("info"))?.and_then(|raw| extract_subject_header(&raw));
    }
    if subject.is_none() {
        subject = read_first_non_empty_line(&apply_dir.join("msg"))?;
    }
    if subject.is_none()
        && let Some(step_num) = step
    {
        let patch_name = format!("{step_num:04}");
        subject = read_lossy(&apply_dir.join(patch_name))?
            .and_then(|raw| extract_subject_header(&raw))
            .map(strip_patch_prefix);
    }

    Ok(InProgressDetails::ApplyingPatches {
        step,
        total_steps,
        subject,
    })
}

fn bisect_terms(git_dir: &Path) -> Result<(String, String)> {
    let Some(raw) = read_lossy(&git_dir.join("BISECT_TERMS"))? else {
        return Ok(("bad".to_owned(), "good".to_owned()));
    };
    let mut lines = raw.lines().map(str::trim).filter(|line| !line.is_empty());
    let bad = lines.next().unwrap_or("bad").to_owned();
    let good = lines.next().unwrap_or("good").to_owned();
    Ok((bad, good))
}

fn count_bisect_refs(repo: &gix::Repository, bad_term: &str, good_term: &str) -> (usize, usize) {
    let Ok(platform) = repo.references() else {
        return (0, 0);
    };
    let Ok(references) = platform.prefixed("refs/bisect/") else {
        return (0, 0);
    };
    let bad_dash = format!("{bad_term}-");
    let good_dash = format!("{good_term}-");
    let mut good = 0usize;
    let mut bad = 0usize;
    for reference in references.flatten() {
        let name = reference.name().as_bstr().to_str_lossy();
        let Some(suffix) = name.strip_prefix("refs/bisect/") else {
            continue;
        };
        if suffix == bad_term || suffix.starts_with(&bad_dash) {
            bad += 1;
        } else if suffix == good_term || suffix.starts_with(&good_dash) {
            good += 1;
        }
    }
    (good, bad)
}

fn normalize_head_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "detached HEAD" {
        return None;
    }
    if let Ok(full) = <&gix::refs::FullNameRef>::try_from(trimmed) {
        let short = full.shorten().to_string();
        if !short.is_empty() {
            return Some(short);
        }
    }
    let stripped = trimmed.strip_prefix("refs/heads/").unwrap_or(trimmed);
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

fn rebase_onto(repo: &gix::Repository, raw: &str) -> Option<RebaseOnto> {
    let spec = raw.trim();
    if spec.is_empty() {
        return None;
    }
    if let Some(id) = resolve_oid(repo, spec) {
        return Some(RebaseOnto {
            id: Some(id),
            short_id: short_oid(id),
            ref_name: ref_pointing_at(repo, id),
        });
    }
    is_hex_prefix(spec).then(|| RebaseOnto {
        id: None,
        short_id: short_hex(spec),
        ref_name: None,
    })
}

fn ref_pointing_at(repo: &gix::Repository, target: Oid) -> Option<String> {
    let platform = repo.references().ok()?;
    let references = platform.all().ok()?;
    let mut best: Option<(u8, String)> = None;
    for reference in references.flatten() {
        let inner = reference.detach();
        let rank = if inner.name.as_bstr().starts_with(b"refs/heads/") {
            0
        } else if inner.name.as_bstr().starts_with(b"refs/remotes/") {
            1
        } else if inner.name.as_bstr().starts_with(b"refs/tags/") {
            2
        } else {
            continue;
        };
        let gix::refs::Target::Object(oid) = inner.target else {
            continue;
        };
        let matches = oid == target
            || inner.peeled == Some(target)
            || (rank == 2 && peel_tag_to_target(repo, oid) == Some(target));
        if !matches {
            continue;
        }
        let short_name = inner.name.shorten().to_string();
        let replace = best.as_ref().is_none_or(|(best_rank, best_name)| {
            (rank, short_name.as_str()) < (*best_rank, best_name.as_str())
        });
        if replace {
            best = Some((rank, short_name));
        }
    }
    best.map(|(_, name)| name)
}

fn peel_tag_to_target(repo: &gix::Repository, mut oid: Oid) -> Option<Oid> {
    for _ in 0..8 {
        let object = repo.find_object(oid).ok()?;
        if object.kind != gix::objs::Kind::Tag {
            return Some(oid);
        }
        oid = object.into_tag().target_id().ok()?.detach();
    }
    None
}

fn parse_rebase_todo_commit(repo: &gix::Repository, todo: &str) -> Option<InProgressCommit> {
    for raw_line in todo.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let cmd = tokens.next()?;
        let (sha, after_sha) = match cmd {
            "pick" | "p" | "reword" | "r" | "edit" | "e" | "squash" | "s" | "drop" | "d" => {
                let sha = tokens.next()?;
                (sha, remainder_after(line, sha))
            }
            "fixup" | "f" => {
                let next = tokens.next()?;
                let sha = if matches!(next, "-c" | "-C") {
                    tokens.next()?
                } else {
                    next
                };
                (sha, remainder_after(line, sha))
            }
            "merge" | "m" => {
                let next = tokens.next()?;
                if matches!(next, "-c" | "-C") {
                    let sha = tokens.next()?;
                    (sha, "")
                } else {
                    return None;
                }
            }
            // Commands like `exec`, `break`, `label`, `reset`, `update-ref` do
            // not name a single commit being replayed.
            _ => return None,
        };
        let fallback_summary = parse_todo_summary(after_sha);
        return commit_from_spec(repo, sha, fallback_summary);
    }
    None
}

fn remainder_after<'a>(line: &'a str, token: &str) -> &'a str {
    let Some(idx) = line.find(token) else {
        return "";
    };
    &line[idx + token.len()..]
}

fn parse_todo_summary(rest: &str) -> Option<String> {
    let rest = rest.trim();
    let rest = rest.strip_prefix('#').map(str::trim_start).unwrap_or(rest);
    (!rest.is_empty()).then(|| rest.to_owned())
}

fn parse_merge_source(merge_msg: &str) -> Option<String> {
    let first = merge_msg
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))?;

    for prefix in [
        "Merge branch '",
        "Merge remote-tracking branch '",
        "Merge tag '",
        "Merge commit '",
    ] {
        let Some(rest) = first.strip_prefix(prefix) else {
            continue;
        };
        if let Some((name, _)) = rest.split_once("' into ") {
            return (!name.is_empty()).then(|| name.to_owned());
        }
        if let Some(name) = rest.strip_suffix('\'') {
            return (!name.is_empty()).then(|| name.to_owned());
        }
        if let Some((name, _)) = rest.split_once('\'') {
            return (!name.is_empty()).then(|| name.to_owned());
        }
    }
    None
}

fn extract_subject_header(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("Subject:") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        if line.is_empty() {
            break;
        }
    }
    None
}

fn strip_patch_prefix(subject: String) -> String {
    if let Some(rest) = subject.strip_prefix('[')
        && let Some((tag, after)) = rest.split_once(']')
        && tag.starts_with("PATCH")
    {
        let trimmed = after.trim_start();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    subject
}

fn commit_from_oid(repo: &gix::Repository, id: Oid) -> InProgressCommit {
    InProgressCommit {
        id: Some(id),
        short_id: short_oid(id),
        summary: commit_summary(repo, id),
    }
}

fn commit_from_spec(
    repo: &gix::Repository,
    spec: &str,
    fallback_summary: Option<String>,
) -> Option<InProgressCommit> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if let Some(id) = resolve_oid(repo, spec) {
        return Some(InProgressCommit {
            id: Some(id),
            short_id: short_oid(id),
            summary: commit_summary(repo, id).or(fallback_summary),
        });
    }
    is_hex_prefix(spec).then(|| InProgressCommit {
        id: None,
        short_id: short_hex(spec),
        summary: fallback_summary,
    })
}

fn resolve_oid(repo: &gix::Repository, spec: &str) -> Option<Oid> {
    if let Ok(id) = spec.parse::<Oid>() {
        return Some(id);
    }
    repo.rev_parse_single(spec).ok().map(|id| id.detach())
}

fn commit_summary(repo: &gix::Repository, id: Oid) -> Option<String> {
    let commit = repo.find_commit(id).ok()?;
    let message = commit.message_raw().ok()?;
    let line = message
        .split(|&byte| byte == b'\n')
        .next()
        .unwrap_or_default();
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let summary = String::from_utf8_lossy(line).trim().to_owned();
    (!summary.is_empty()).then_some(summary)
}

fn short_oid(id: Oid) -> String {
    short_hex(&id.to_hex().to_string())
}

fn short_hex(hex: &str) -> String {
    hex[..7.min(hex.len())].to_owned()
}

fn is_hex_prefix(spec: &str) -> bool {
    spec.len() >= 4 && spec.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read_git_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::Io(err)),
    }
}

fn read_lossy(path: &Path) -> Result<Option<String>> {
    Ok(read_git_file(path)?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

fn read_trimmed(path: &Path) -> Result<Option<String>> {
    let Some(text) = read_lossy(path)? else {
        return Ok(None);
    };
    let trimmed = text.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
}

fn read_usize(path: &Path) -> Result<Option<usize>> {
    Ok(read_trimmed(path)?.and_then(|text| text.parse::<usize>().ok()))
}

fn read_first_non_empty_line(path: &Path) -> Result<Option<String>> {
    let Some(text) = read_lossy(path)? else {
        return Ok(None);
    };
    Ok(text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_merge_source_variants() {
        assert_eq!(
            parse_merge_source("Merge branch 'feature-x'\n\n# Conflicts:\n#\ta.txt\n").as_deref(),
            Some("feature-x")
        );
        assert_eq!(
            parse_merge_source("Merge branch 'topic' into main\n").as_deref(),
            Some("topic")
        );
        assert_eq!(
            parse_merge_source("Merge remote-tracking branch 'origin/main' into dev\n").as_deref(),
            Some("origin/main")
        );
        assert_eq!(
            parse_merge_source("Merge tag 'v1.2.0'\n").as_deref(),
            Some("v1.2.0")
        );
        assert_eq!(
            parse_merge_source("Merge commit 'a1b2c3d' into main\n").as_deref(),
            Some("a1b2c3d")
        );
        assert_eq!(parse_merge_source("Custom commit message\n"), None);
    }

    #[test]
    fn normalizes_head_name() {
        assert_eq!(
            normalize_head_name("refs/heads/feature-x\n").as_deref(),
            Some("feature-x")
        );
        assert_eq!(
            normalize_head_name("refs/heads/team/feature\n").as_deref(),
            Some("team/feature")
        );
        assert_eq!(normalize_head_name("detached HEAD\n"), None);
        assert_eq!(normalize_head_name("  \n"), None);
    }

    #[test]
    fn strips_patch_prefix_from_subject() {
        assert_eq!(
            strip_patch_prefix("[PATCH 1/3] Fix parser".to_owned()),
            "Fix parser"
        );
        assert_eq!(strip_patch_prefix("Fix parser".to_owned()), "Fix parser");
    }
}
