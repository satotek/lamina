//! Owned by the repo-log task: commit walk, refs, HEAD.

use std::collections::HashSet;

use super::Repo;
use crate::{CommitInfo, Error, HeadInfo, RefInfo, RefKind, Result};

/// Streams commits reachable from HEAD and all refs, in topological order
/// (children before parents), newest first among independent commits.
///
/// This is `git log --all --date-order`. gix's topological walk with
/// [`Sorting::DateOrder`](gix::traverse::commit::topo::Sorting::DateOrder) is the
/// same algorithm git uses, and it needs the reachability graph up front, so the
/// graph is built eagerly while the per-commit object reads stream lazily.
pub struct CommitWalk {
    repo: gix::Repository,
    topo: gix::traverse::commit::Topo<gix::OdbHandle, fn(&gix::oid) -> bool>,
}

impl Iterator for CommitWalk {
    type Item = Result<CommitInfo>;

    fn next(&mut self) -> Option<Self::Item> {
        let info = match self.topo.next()? {
            Ok(info) => info,
            Err(err) => return Some(Err(Error::gix(err))),
        };
        Some(commit_info(&self.repo, info.id).map_err(Error::gix))
    }
}

impl Repo {
    pub fn head(&self) -> Result<HeadInfo> {
        let repo = self.local();
        let head = repo.head().map_err(Error::gix)?;

        if head.is_detached() {
            // A detached HEAD always carries a direct target; `id()` also applies the
            // peeled target so a checked-out tag still resolves to its commit.
            let target = head
                .id()
                .ok_or_else(|| Error::other("detached HEAD has no object id"))?;
            return Ok(HeadInfo::Detached {
                target: target.detach(),
            });
        }

        let name = head
            .referent_name()
            .ok_or_else(|| Error::other("HEAD is neither detached nor symbolic"))?;
        Ok(HeadInfo::Branch {
            short_name: name.shorten().to_string(),
            target: head.id().map(|id| id.detach()),
        })
    }

    pub fn refs(&self) -> Result<Vec<RefInfo>> {
        let repo = self.local();
        let mut refs = Vec::new();

        let platform = repo.references().map_err(Error::gix)?;
        let references = platform.all().map_err(Error::gix)?;
        for reference in references {
            let mut reference = reference.map_err(Error::gix)?;
            // Symbolic refs (notably `refs/remotes/origin/HEAD`) alias another ref and
            // would show up as duplicate entries in the UI.
            if reference.target().kind() == gix::refs::Kind::Symbolic {
                continue;
            }

            let full_name = reference.name().to_string();
            let kind = if full_name.starts_with("refs/heads/") {
                RefKind::LocalBranch
            } else if full_name.starts_with("refs/remotes/") {
                RefKind::RemoteBranch
            } else if full_name.starts_with("refs/tags/") {
                RefKind::Tag
            } else {
                continue;
            };

            // Annotated tags point at a tag object; the UI needs the commit it names.
            let target = match reference.peel_to_id() {
                Ok(id) => id.detach(),
                // Dangling refs are skipped so one broken ref cannot hide the whole list.
                Err(_) => continue,
            };
            if !is_commit(&repo, target)? {
                continue;
            }

            refs.push(RefInfo {
                short_name: reference.name().shorten().to_string(),
                full_name,
                kind,
                target,
            });
        }

        refs.sort_by(|a, b| {
            ref_kind_rank(a.kind)
                .cmp(&ref_kind_rank(b.kind))
                .then_with(|| a.full_name.cmp(&b.full_name))
        });
        Ok(refs)
    }

    pub fn walk_commits(&self) -> Result<CommitWalk> {
        let repo = self.local();
        let tips = walk_tips(&repo)?;
        // A missing or corrupt commit-graph is not fatal: object reads still work.
        let commit_graph = repo.commit_graph_if_enabled().ok().flatten();

        let topo = gix::traverse::commit::topo::Builder::from_iters(
            repo.objects.clone(),
            tips,
            None::<Vec<gix::ObjectId>>,
        )
        .sorting(gix::traverse::commit::topo::Sorting::DateOrder)
        .with_commit_graph(commit_graph)
        .build()
        .map_err(Error::gix)?;

        Ok(CommitWalk { repo, topo })
    }
}

fn ref_kind_rank(kind: RefKind) -> u8 {
    match kind {
        RefKind::LocalBranch => 0,
        RefKind::RemoteBranch => 1,
        RefKind::Tag => 2,
    }
}

fn is_commit(repo: &gix::Repository, id: gix::ObjectId) -> Result<bool> {
    match repo.find_header(id) {
        Ok(header) => Ok(header.kind() == gix::objs::Kind::Commit),
        // A ref may point at a tag-to-tree, blob or a missing object; none can be walked.
        Err(err) if err.is_not_found() => Ok(false),
        Err(err) => Err(Error::gix(err)),
    }
}

/// HEAD plus every ref target, deduplicated, matching the `--all` revision set.
fn walk_tips(repo: &gix::Repository) -> Result<Vec<gix::ObjectId>> {
    let mut seen = HashSet::new();
    let mut tips = Vec::new();

    let head = repo.head().map_err(Error::gix)?;
    if let Some(id) = head.id() {
        push_tip(repo, &mut seen, &mut tips, id.detach())?;
    }
    let platform = repo.references().map_err(Error::gix)?;
    let references = platform.all().map_err(Error::gix)?;
    for reference in references {
        let Ok(mut reference) = reference else {
            continue;
        };
        if let Ok(id) = reference.peel_to_id() {
            push_tip(repo, &mut seen, &mut tips, id.detach())?;
        }
    }

    Ok(tips)
}

fn push_tip(
    repo: &gix::Repository,
    seen: &mut HashSet<gix::ObjectId>,
    tips: &mut Vec<gix::ObjectId>,
    id: gix::ObjectId,
) -> Result<()> {
    if is_commit(repo, id)? && seen.insert(id) {
        tips.push(id);
    }
    Ok(())
}

fn commit_info(repo: &gix::Repository, id: gix::ObjectId) -> gix::Result<CommitInfo> {
    let commit = repo.find_object(id)?.into_commit();
    let author = commit.author()?;
    let committer = commit.committer()?;

    Ok(CommitInfo {
        id,
        parents: commit.parent_ids().map(|id| id.detach()).collect(),
        summary: first_line(commit.message_raw()?),
        author_name: author.name.to_string(),
        author_email: author.email.to_string(),
        // Malformed dates must not fail the whole log; git also tolerates them.
        author_time: gix::date::parse_header(author.time)
            .map(|time| time.seconds)
            .unwrap_or(0),
        committer_time: gix::date::parse_header(committer.time)
            .map(|time| time.seconds)
            .unwrap_or(0),
    })
}

/// The log shows the subject as everything up to the first newline, not git's
/// blank-line-separated title, and decodes lossily because messages need not be UTF-8.
fn first_line(message: &gix::bstr::BStr) -> String {
    let line = message
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(line)).into_owned()
}
