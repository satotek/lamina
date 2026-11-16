//! Owned by the repo-log task: commit walk, refs, HEAD.

mod path;
mod walk;

use std::collections::HashSet;
use std::sync::Arc;

use gix::bstr::ByteSlice as _;
use gix::objs::FindExt as _;
use gix::objs::commit::ref_iter::Token as CommitToken;

use super::Repo;
use crate::{CommitInfo, Error, HeadInfo, Oid, RefInfo, RefKind, Result, StashInfo};

pub(crate) use path::{PathCommit, PathWalk};

/// The commits at a shallow clone's boundary, whose parents are not in the
/// repository. Everything that walks ancestry stops at these commits, the way
/// git treats them as roots.
#[derive(Debug, Clone, Default)]
pub(crate) struct Shallow {
    /// Sorted, so a boundary check is a binary search.
    boundary: Arc<[Oid]>,
}

impl Shallow {
    /// The boundary of `repo`, empty when it is not a shallow clone.
    pub(crate) fn load(repo: &gix::Repository) -> Result<Self> {
        let Some(commits) = repo.shallow_commits().map_err(Error::gix)? else {
            return Ok(Self::default());
        };
        let mut boundary: Vec<Oid> = commits.iter().copied().collect();
        boundary.sort_unstable();
        Ok(Self {
            boundary: boundary.into(),
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.boundary.is_empty()
    }

    pub(crate) fn is_boundary(&self, id: Oid) -> bool {
        self.boundary.binary_search(&id).is_ok()
    }
}

/// Cooperative cancellation for a walk: returns `true` to stop it.
///
/// Shared between the commit walk and the path-history filter, which has to keep
/// checking between commits that do not match the path.
pub(crate) type CancelCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Streams commits reachable from HEAD and the user-facing refs, in topological
/// order (children before parents), newest first among independent commits.
///
/// This is `git log --date-order HEAD --branches --remotes --tags refs/stash`
/// plus every older stash in the `refs/stash` reflog (`refs/stash` only when a
/// stash exists). Private namespaces that tools create
/// (`refs/notes/`, `refs/original/`, `refs/prefetch/`, `refs/bisect/`, ...) are
/// deliberately left out: their commits would add lanes that no user asked to
/// see.
///
/// `gix`'s topological walk builds the whole reachability graph before yielding
/// anything, so it is replaced by the incremental walk in `walk.rs`, which uses
/// commit-graph generation numbers to emit the newest commits while older
/// history is still being counted.
pub struct CommitWalk {
    repo: gix::Repository,
    walk: walk::Walk,
    shallow: Shallow,
    object_buf: Vec<u8>,
    parents_buf: Vec<Oid>,
}

impl CommitWalk {
    pub(crate) fn estimated_commits(&self) -> Option<usize> {
        self.walk.estimated_commits()
    }

    /// Whether the walk reads generation numbers from a commit-graph.
    pub fn has_commit_graph(&self) -> bool {
        self.walk.has_commit_graph()
    }

    /// Reachable commits the walk had to read from the object database because
    /// the commit-graph does not contain them. Zero for a missing graph, which
    /// no graph-backed walk is expected to cover.
    pub fn commits_outside_graph(&self) -> usize {
        self.walk.commits_outside_graph()
    }
}

impl Iterator for CommitWalk {
    type Item = Result<CommitInfo>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.walk.next_id() {
            Ok(Some(id)) => Some(commit_info(
                &self.repo,
                id,
                &self.shallow,
                &mut self.object_buf,
                &mut self.parents_buf,
            )),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

impl Repo {
    /// Parent ids of `commit`, first parent first.
    pub fn parents(&self, commit: Oid) -> Result<Vec<Oid>> {
        let repo = self.local();
        // A boundary commit's parents are outside the clone: it is a root here,
        // which is also what makes its diff fall back to the empty tree.
        if Shallow::load(&repo)?.is_boundary(commit) {
            return Ok(Vec::new());
        }
        let commit = repo.find_commit(commit).map_err(Error::gix)?;
        Ok(commit.parent_ids().map(|id| id.detach()).collect())
    }

    /// Commits reachable from `tip` but not from `base`, newest first.
    ///
    /// This is what `git rev-list base..tip` lists, so its length is how many
    /// commits a rebase onto `base` would replay. Zero commits means the branch
    /// only moves to `base` (or is already there), which
    /// [`is_ancestor`](Self::is_ancestor) tells apart.
    pub fn commits_between(&self, base: Oid, tip: Oid) -> Result<Vec<Oid>> {
        let repo = self.local();
        let walk = repo
            .rev_walk([tip])
            .with_hidden([base])
            .all()
            .map_err(Error::gix)?;
        let mut commits = Vec::new();
        for step in walk {
            commits.push(step.map_err(Error::gix)?.id().detach());
        }
        Ok(commits)
    }

    /// Committer timestamp of `commit`, in seconds since the epoch. Malformed
    /// dates read as zero, the way the commit log treats them.
    pub fn commit_time(&self, commit: Oid) -> Result<i64> {
        let repo = self.local();
        let commit = repo.find_commit(commit).map_err(Error::gix)?;
        let committer = commit.committer().map_err(Error::gix)?;
        Ok(committer.time().map_or(0, |time| time.seconds))
    }

    /// Whether `ancestor` is reachable from `descendant`, as
    /// `git merge-base --is-ancestor` answers it. A commit is its own ancestor.
    pub fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool> {
        let repo = self.local();
        let base = repo.merge_base(ancestor, descendant).map_err(Error::gix)?;
        Ok(base.is_some_and(|base| base.detach() == ancestor))
    }

    /// Stash entries, newest (`stash@{0}`) first. Empty when nothing is stashed.
    pub fn stashes(&self) -> Result<Vec<StashInfo>> {
        let repo = self.local();
        let Some(reference) = repo.try_find_reference("refs/stash").map_err(Error::gix)? else {
            return Ok(Vec::new());
        };
        let mut platform = reference.log_iter();
        // The reflog file is oldest first; reading it backwards yields stash@{0} first.
        let Some(lines) = platform.rev().map_err(Error::gix)? else {
            return Ok(Vec::new());
        };
        let mut stashes = Vec::new();
        for line in lines {
            let line = line.map_err(|err| Error::other(err.to_string()))?;
            let message = line.message.to_string();
            stashes.push(StashInfo {
                index: stashes.len(),
                id: line.new_oid,
                branch: stash_branch(&message),
                created_time: line.signature.time.seconds,
                message,
            });
        }
        Ok(stashes)
    }

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

    /// The user-facing refs: local branches, remote-tracking branches and tags.
    ///
    /// Tool-owned namespaces such as `refs/notes/` are not listed; the commit
    /// walk skips them as well, so a ref badge always has a row to sit on.
    pub fn refs(&self) -> Result<Vec<RefInfo>> {
        let repo = self.local();
        let graph = repo.commit_graph_if_enabled().ok().flatten();
        let mut refs = Vec::new();

        let platform = repo.references().map_err(Error::gix)?;
        let references = platform.all().map_err(Error::gix)?;
        for reference in references {
            let reference = reference.map_err(Error::gix)?;
            let inner = reference.detach();
            // Symbolic refs (notably `refs/remotes/origin/HEAD`) alias another ref and
            // would show up as duplicate entries in the UI.
            let gix::refs::Target::Object(target) = inner.target else {
                continue;
            };
            let Some(kind) = ref_kind(inner.name.as_bstr()) else {
                continue;
            };

            let target = match resolve_commit(&repo, graph.as_ref(), target, inner.peeled)? {
                Some(target) => target,
                // Dangling refs are skipped so one broken ref cannot hide the whole list.
                None => continue,
            };

            refs.push(RefInfo {
                short_name: inner.name.shorten().to_string(),
                full_name: inner.name.as_bstr().to_string(),
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
        self.walk_commits_with(&[], None)
    }

    /// [`walk_commits`](Self::walk_commits) that stops once `cancelled` returns
    /// `true`, for a caller that must be able to give up on a long walk, and
    /// that leaves the tips of `hidden_refs` out of the starting points.
    ///
    /// A hidden ref still exists and still appears in the ref list; only the
    /// walk it would start is dropped, so commits reachable from another ref
    /// stay. `HEAD` is never hidden: the checked-out branch's own commits are
    /// what the user is looking at.
    pub(crate) fn walk_commits_with(
        &self,
        hidden_refs: &[String],
        cancelled: Option<CancelCheck>,
    ) -> Result<CommitWalk> {
        let repo = self.local();
        // A missing or corrupt commit-graph is not fatal: object reads still work.
        let graph = repo.commit_graph_if_enabled().ok().flatten();
        let tips = walk_tips(&repo, graph.as_ref(), hidden_refs)?;
        let shallow = Shallow::load(&repo)?;
        let walk = walk::Walk::new(
            repo.objects.clone(),
            graph,
            &tips,
            shallow.clone(),
            cancelled,
            true,
        )?;
        Ok(CommitWalk {
            repo,
            walk,
            shallow,
            object_buf: Vec::new(),
            parents_buf: Vec::with_capacity(4),
        })
    }

    /// Streams commits reachable from `specs` only, in the same topological
    /// order as [`walk_commits`](Self::walk_commits).
    ///
    /// Each spec names a ref (`refs/heads/topic`, `origin/main`, a tag) or any
    /// other revision `git rev-parse` accepts, such as a full or abbreviated
    /// object id. Unlike the full walk, no other refs are included, not even
    /// HEAD.
    pub fn walk_commits_from_refs(&self, specs: &[String]) -> Result<CommitWalk> {
        self.walk_commits_from_refs_with(specs, None)
    }

    /// [`walk_commits_from_refs`](Self::walk_commits_from_refs) with a cancel
    /// check, matching [`walk_commits_with`](Self::walk_commits_with).
    pub(crate) fn walk_commits_from_refs_with(
        &self,
        specs: &[String],
        cancelled: Option<CancelCheck>,
    ) -> Result<CommitWalk> {
        let repo = self.local();
        let graph = repo.commit_graph_if_enabled().ok().flatten();
        let mut seen = HashSet::new();
        let mut tips = Vec::new();
        for spec in specs {
            // A bad spec is an error on purpose: silently walking a smaller set
            // would look like a branch with no commits.
            let id = repo
                .rev_parse_single(spec.as_str())
                .map_err(Error::gix)?
                .detach();
            if let Some(commit) = resolve_commit(&repo, graph.as_ref(), id, None)?
                && seen.insert(commit)
            {
                tips.push(commit);
            }
        }
        let shallow = Shallow::load(&repo)?;
        let walk = walk::Walk::new(
            repo.objects.clone(),
            graph,
            &tips,
            shallow.clone(),
            cancelled,
            false,
        )?;
        Ok(CommitWalk {
            repo,
            walk,
            shallow,
            object_buf: Vec::new(),
            parents_buf: Vec::with_capacity(4),
        })
    }

    /// A commit walk for path-limited history.
    ///
    /// It yields the same `--date-order` sequence as
    /// [`walk_commits_with`](Self::walk_commits_with), but keeps only the
    /// ordering counters instead of a node per commit: a file history needs the
    /// order of the commits that touched the file, not the reachability state of
    /// the whole repository.
    pub(crate) fn path_walk_with(
        &self,
        cancelled: Option<CancelCheck>,
    ) -> Result<(PathWalk, Shallow, Option<super::bloom::CommitGraphBloom>)> {
        let repo = self.local();
        let graph = repo.commit_graph_if_enabled().ok().flatten();
        let tips = walk_tips(&repo, graph.as_ref(), &[])?;
        let shallow = Shallow::load(&repo)?;
        let bloom = graph
            .as_ref()
            .and_then(|graph| super::bloom::CommitGraphBloom::load(&repo, graph));
        let walk = PathWalk::new(
            repo.objects.clone(),
            graph,
            &tips,
            shallow.clone(),
            cancelled,
        )?;
        Ok((walk, shallow, bloom))
    }

    /// A commit walk for history filtered to `specs` (refs or revisions).
    pub(crate) fn path_walk_from_refs_with(
        &self,
        specs: &[String],
        cancelled: Option<CancelCheck>,
    ) -> Result<(PathWalk, Shallow, Option<super::bloom::CommitGraphBloom>)> {
        let repo = self.local();
        let graph = repo.commit_graph_if_enabled().ok().flatten();
        let mut seen = HashSet::new();
        let mut tips = Vec::new();
        for spec in specs {
            let id = repo
                .rev_parse_single(spec.as_str())
                .map_err(Error::gix)?
                .detach();
            if let Some(commit) = resolve_commit(&repo, graph.as_ref(), id, None)?
                && seen.insert(commit)
            {
                tips.push(commit);
            }
        }
        let shallow = Shallow::load(&repo)?;
        let bloom = graph
            .as_ref()
            .and_then(|graph| super::bloom::CommitGraphBloom::load(&repo, graph));
        let walk = PathWalk::new(
            repo.objects.clone(),
            graph,
            &tips,
            shallow.clone(),
            cancelled,
        )?;
        Ok((walk, shallow, bloom))
    }
}

/// The branch a stash message names: git writes `WIP on <branch>: ...` for a
/// plain `git stash push` and `On <branch>: ...` when a message was given (and
/// when the entry was created with `git stash store`). `(no branch)` is what
/// git writes for a detached HEAD.
fn stash_branch(message: &str) -> Option<String> {
    let rest = message
        .strip_prefix("WIP on ")
        .or_else(|| message.strip_prefix("On "))?;
    // Branch names cannot contain a colon, so the first one ends the name.
    let (branch, _) = rest.split_once(':')?;
    let branch = branch.trim();
    if branch.is_empty() || branch == "(no branch)" {
        return None;
    }
    Some(branch.to_owned())
}

fn ref_kind(full_name: &gix::bstr::BStr) -> Option<RefKind> {
    if full_name.starts_with(b"refs/heads/") {
        Some(RefKind::LocalBranch)
    } else if full_name.starts_with(b"refs/remotes/") {
        Some(RefKind::RemoteBranch)
    } else if full_name.starts_with(b"refs/tags/") {
        Some(RefKind::Tag)
    } else {
        None
    }
}

/// Whether a ref belongs to the history the user sees: local branches,
/// remote-tracking branches, tags and the stash.
///
/// Tool-owned namespaces (`refs/notes/`, `refs/original/`, `refs/prefetch/`,
/// `refs/bisect/`, ...) are excluded, as they are in `Repo::refs`.
fn is_user_ref(full_name: &gix::bstr::BStr) -> bool {
    ref_kind(full_name).is_some() || full_name == b"refs/stash"
}

fn ref_kind_rank(kind: RefKind) -> u8 {
    match kind {
        RefKind::LocalBranch => 0,
        RefKind::RemoteBranch => 1,
        RefKind::Tag => 2,
    }
}

/// The commit a ref points at, if its target can be walked at all.
///
/// `peeled` is the packed-refs peel result for the ref. It is what makes
/// enumerating tens of thousands of annotated tags cheap: the commit is known
/// without reading the tag object. A ref without a peel result still has to be
/// classified, because it may point at an annotated tag object or at a
/// non-commit.
fn resolve_commit(
    repo: &gix::Repository,
    graph: Option<&gix::commitgraph::Graph>,
    target: gix::ObjectId,
    peeled: Option<gix::ObjectId>,
) -> Result<Option<gix::ObjectId>> {
    let mut current = peeled.unwrap_or(target);
    loop {
        if is_commit(repo, graph, current)? {
            return Ok(Some(current));
        }
        if peeled.is_some() {
            // A packed peel result is already fully peeled, so this ref points
            // at a tree or blob and cannot be part of the commit walk.
            return Ok(None);
        }
        match peel_tag(repo, current)? {
            Some(next) => current = next,
            None => return Ok(None),
        }
    }
}

/// Follow one annotated tag. `None` means the object is not a tag, so the chain
/// ends without reaching a commit.
fn peel_tag(repo: &gix::Repository, id: gix::ObjectId) -> Result<Option<gix::ObjectId>> {
    let object = match repo.find_object(id) {
        Ok(object) => object,
        // A missing object cannot be walked, but must not hide the other refs.
        Err(err) if err.is_not_found() => return Ok(None),
        Err(err) => return Err(Error::gix(err)),
    };
    if object.kind != gix::objs::Kind::Tag {
        return Ok(None);
    }
    let tag = object.into_tag();
    match tag.target_id() {
        Ok(id) => Ok(Some(id.detach())),
        Err(err) => Err(Error::gix(err)),
    }
}

/// Whether `id` names a commit.
///
/// Commit-graph membership answers this without touching the object database,
/// which matters when every ref target is checked. Without a graph only the
/// object header is read.
fn is_commit(
    repo: &gix::Repository,
    graph: Option<&gix::commitgraph::Graph>,
    id: gix::ObjectId,
) -> Result<bool> {
    if graph.and_then(|graph| graph.commit_by_id(id)).is_some() {
        return Ok(true);
    }
    match repo.find_header(id) {
        Ok(header) => Ok(header.kind() == gix::objs::Kind::Commit),
        // A ref may point at a tag-to-tree, blob or a missing object; none can be walked.
        Err(err) if err.is_not_found() => Ok(false),
        Err(err) => Err(Error::gix(err)),
    }
}

/// HEAD plus every user-facing ref target, deduplicated, matching the
/// `HEAD --branches --remotes --tags refs/stash` revision set.
///
/// `hidden_refs` names refs whose tips do not start the walk; comparing on the
/// full name keeps a hidden branch from also dropping an unrelated tag that
/// happens to point at the same commit.
fn walk_tips(
    repo: &gix::Repository,
    graph: Option<&gix::commitgraph::Graph>,
    hidden_refs: &[String],
) -> Result<Vec<gix::ObjectId>> {
    let mut checked = HashSet::new();
    let mut seen = HashSet::new();
    let mut tips = Vec::new();

    let head = repo.head().map_err(Error::gix)?;
    if let Some(id) = head.id() {
        let id = id.detach();
        if is_commit(repo, graph, id)? && seen.insert(id) {
            tips.push(id);
        }
    }

    let platform = repo.references().map_err(Error::gix)?;
    let references = platform.all().map_err(Error::gix)?;
    for reference in references {
        let Ok(reference) = reference else {
            continue;
        };
        let inner = reference.detach();
        if !is_user_ref(inner.name.as_bstr()) {
            continue;
        }
        let name = inner.name.as_bstr();
        if hidden_refs
            .iter()
            .any(|hidden| hidden.as_bytes() == name.as_bytes())
        {
            continue;
        }
        let gix::refs::Target::Object(target) = inner.target else {
            // A symbolic ref only aliases a ref that is visited on its own.
            continue;
        };
        // The same commit is usually the target of many refs, so classify each
        // distinct target once. A peel result from packed-refs identifies the
        // final object directly, skipping the tag object read.
        let candidate = inner.peeled.unwrap_or(target);
        if !checked.insert(candidate) {
            continue;
        }
        if let Some(commit) = resolve_commit(repo, graph, target, inner.peeled)?
            && seen.insert(commit)
        {
            tips.push(commit);
        }
    }

    // `refs/stash` names only the newest stash; older ones live in its reflog.
    // They are tips too, or picking `stash@{1}` in the sidebar would have no
    // row in the list to land on.
    if !hidden_refs.iter().any(|hidden| hidden == "refs/stash")
        && let Some(reference) = repo.try_find_reference("refs/stash").map_err(Error::gix)?
    {
        let mut platform = reference.log_iter();
        if let Some(lines) = platform.rev().map_err(Error::gix)? {
            for line in lines {
                let Ok(line) = line else {
                    continue;
                };
                let id = line.new_oid;
                if seen.contains(&id) || !checked.insert(id) {
                    continue;
                }
                if is_commit(repo, graph, id)? && seen.insert(id) {
                    tips.push(id);
                }
            }
        }
    }

    Ok(tips)
}

pub(crate) fn commit_info(
    repo: &gix::Repository,
    id: gix::ObjectId,
    shallow: &Shallow,
    object_buf: &mut Vec<u8>,
    parents_buf: &mut Vec<Oid>,
) -> Result<CommitInfo> {
    let data = repo.objects.find(&id, object_buf).map_err(Error::gix)?;
    if data.kind != gix::objs::Kind::Commit {
        return Err(Error::other(format!("{id} is not a commit")));
    }

    // One pass over the commit data: `Commit`'s per-field accessors each re-parse
    // the header, which doubles the cost of decoding every commit in the log.
    parents_buf.clear();
    let mut author_name = String::new();
    let mut author_email = String::new();
    let mut author_time = 0;
    let mut committer_time = 0;
    let mut summary = String::new();
    for token in gix::objs::CommitRefIter::from_bytes(data.data, repo.object_hash()) {
        match token.map_err(Error::gix)? {
            CommitToken::Parent { id } => parents_buf.push(id),
            CommitToken::Author { signature } => {
                author_name = String::from_utf8_lossy(signature.name.as_ref()).into_owned();
                author_email = String::from_utf8_lossy(signature.email.as_ref()).into_owned();
                // Malformed dates must not fail the whole log; git also tolerates them.
                author_time = signature.time().map_or(0, |time| time.seconds);
            }
            CommitToken::Committer { signature } => {
                committer_time = signature.time().map_or(0, |time| time.seconds);
            }
            CommitToken::Message(message) => {
                summary = first_line(message);
                break;
            }
            _ => {}
        }
    }

    // The boundary commit's parents are absent from a shallow clone, so it is
    // shown as a root and the graph draws no edge to a missing commit.
    if shallow.is_boundary(id) {
        parents_buf.clear();
    }

    let parents = if parents_buf.is_empty() {
        Vec::new()
    } else {
        // Exact-capacity Vec avoids Vec::push's 4-element minimum (80 bytes for Oid).
        let mut vec = Vec::with_capacity(parents_buf.len());
        vec.extend_from_slice(parents_buf);
        vec
    };

    Ok(CommitInfo {
        id,
        parents,
        summary,
        author_name,
        author_email,
        author_time,
        committer_time,
        shallow_boundary: shallow.is_boundary(id),
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

#[cfg(test)]
mod tests {
    use crate::test_support::{TestRepo, shallow_clone_fixture};

    #[test]
    fn a_shallow_clone_walk_ends_at_the_boundary() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "2"]);
        let repo = clone.open();
        let walked: Vec<_> = repo
            .walk_commits()
            .expect("walk")
            .map(|commit| commit.expect("commit"))
            .collect();
        let ids: Vec<_> = walked.iter().map(|commit| commit.id).collect();
        let mut expected = clone.rev_list("HEAD");
        expected.reverse();
        assert_eq!(ids, expected, "the walk lists exactly the depth");

        let boundary = walked.last().expect("a boundary commit");
        assert!(
            boundary.parents.is_empty(),
            "the boundary is a root: it must not point at a missing commit"
        );
        assert!(
            boundary.shallow_boundary,
            "the merge at the end of the fetched depth is the boundary"
        );
        assert!(
            repo.parents(boundary.id).expect("parents").is_empty(),
            "the boundary has no parents to diff against"
        );
        assert!(
            !walked[..walked.len() - 1]
                .iter()
                .any(|commit| commit.parents.is_empty()),
            "only the boundary is a root"
        );
        assert_eq!(
            walked
                .iter()
                .filter(|commit| commit.shallow_boundary)
                .map(|commit| commit.id)
                .collect::<Vec<_>>(),
            [boundary.id],
            "exactly the boundary commit is flagged"
        );
    }

    #[test]
    fn a_depth_one_clone_walks_only_its_tip_as_a_root() {
        let (_source, clone) = shallow_clone_fixture(&["--depth", "1"]);
        let repo = clone.open();
        let walked: Vec<_> = repo
            .walk_commits()
            .expect("walk")
            .map(|commit| commit.expect("commit"))
            .collect();
        assert_eq!(walked.len(), 1);
        assert_eq!(walked[0].id, clone.head());
        assert!(walked[0].parents.is_empty());
        assert!(
            walked[0].shallow_boundary,
            "a depth-one clone's only commit is its boundary"
        );
    }

    #[test]
    fn hidden_refs_keep_their_commits_out_of_the_walk() {
        let repo = TestRepo::new();
        repo.write("base.txt", "base\n");
        let base = repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.write("topic.txt", "topic\n");
        let topic = repo.commit("topic only");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("main.txt", "main\n");
        let main = repo.commit("main only");

        let opened = repo.open();
        let ids: Vec<crate::Oid> = opened
            .walk_commits_with(&["refs/heads/topic".into()], None)
            .expect("walk")
            .map(|commit| commit.expect("commit").id)
            .collect();
        assert!(!ids.contains(&topic), "the hidden branch's commit is gone");
        assert!(ids.contains(&main), "the other branch is still walked");
        assert!(
            ids.contains(&base),
            "a commit both branches reach stays: only the hidden tip is dropped"
        );

        let all: Vec<crate::Oid> = opened
            .walk_commits()
            .expect("walk")
            .map(|commit| commit.expect("commit").id)
            .collect();
        assert!(all.contains(&topic), "hiding is undone by an empty list");
    }

    #[test]
    fn a_refs_only_walk_lists_that_branch_and_its_ancestors() {
        let repo = TestRepo::new();
        repo.write("base.txt", "base\n");
        let base = repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.write("topic.txt", "topic\n");
        let topic = repo.commit("topic only");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("main.txt", "main\n");
        let main = repo.commit("main only");

        let opened = repo.open();
        let ids: Vec<crate::Oid> = opened
            .walk_commits_from_refs(&["refs/heads/topic".into()])
            .expect("walk")
            .map(|commit| commit.expect("commit").id)
            .collect();
        assert_eq!(
            ids,
            [topic, base],
            "only the soloed branch and its ancestors are walked"
        );
        assert!(!ids.contains(&main));
    }

    #[test]
    fn commits_between_lists_the_range_a_rebase_would_replay() {
        let repo = TestRepo::new();
        repo.write("base.txt", "base\n");
        let base = repo.commit("base");
        repo.write("one.txt", "one\n");
        let one = repo.commit("one");
        repo.write("two.txt", "two\n");
        let two = repo.commit("two");

        let opened = repo.open();
        assert_eq!(
            opened.commits_between(base, two).expect("range"),
            [two, one],
            "newest first and the base is excluded"
        );
        assert!(
            opened.commits_between(two, two).expect("range").is_empty(),
            "a branch is up to date with itself"
        );
        assert!(
            opened.commits_between(one, base).expect("range").is_empty(),
            "a tip behind the base replays nothing"
        );
        assert!(opened.is_ancestor(base, two).expect("ancestry"));
        assert!(
            opened.is_ancestor(two, two).expect("ancestry"),
            "a commit is its own ancestor"
        );
        assert!(
            !opened.is_ancestor(two, base).expect("ancestry"),
            "a descendant is not an ancestor"
        );
    }
}
