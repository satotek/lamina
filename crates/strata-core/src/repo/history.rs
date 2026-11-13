//! History limited to a path.
//!
//! A commit changed `path` when its tree entry for `path` differs from the entry
//! in its first parent. Only the path's own tree entries are read: the lookup
//! descends the tree along the path components and never compares whole trees.
//! With a commit-graph, tree ids come from the graph instead of re-parsing every
//! commit object.

use std::collections::HashMap;

use gix::objs::{FindExt, TreeRefIter};

use super::log::CancelCheck;
use super::{CommitWalk, Repo};
use crate::{Error, Oid, Result};

/// Resolved entries are kept per tree id. In linear history a commit's tree is
/// its child's parent tree, so the next row is usually a cache hit.
const ENTRY_CACHE_LIMIT: usize = 8_192;

/// 8 MiB of decoded objects is enough for a walk to reuse recent trees and
/// commits without competing with the UI for memory.
const OBJECT_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// A tree entry as it decides whether a commit changed the path: id and mode.
/// The mode matters because a file turning executable, or a file becoming a
/// directory, is a change even when the blob id stays the same.
type Entry = (Oid, u16);

/// Streams ids of commits that changed `path` (a file, or a directory prefix),
/// in the same order as [`Repo::walk_commits`].
///
/// A commit is reported when its tree entry for `path` differs from its first
/// parent's. That matches `git log --full-history --date-order HEAD --branches
/// --remotes --tags refs/stash -- <path>` (with `refs/stash` only when a stash
/// exists) except for merge commits that are TREESAME to their first parent but
/// differ from another parent: git's full history lists those, this walk does
/// not, because it never traverses the other parents. No rename following
/// either.
pub struct PathHistory {
    repo: gix::Repository,
    walk: CommitWalk,
    /// Path split into components; empty for the repository root.
    components: Vec<Vec<u8>>,
    graph: Option<gix::commitgraph::Graph>,
    entries: HashMap<Oid, Option<Entry>>,
    odb_buf: Vec<u8>,
    failed: bool,
    /// `None` for the public, uncancellable walk.
    cancelled: Option<CancelCheck>,
}

impl Iterator for PathHistory {
    type Item = Result<Oid>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            // The commit walk checks too, but this is the loop that can keep
            // running without ever yielding a row: a path that never matches
            // would otherwise walk the whole history before the caller gets
            // control back.
            if self.is_cancelled() {
                return None;
            }
            let commit = match self.walk.next()? {
                Ok(commit) => commit,
                Err(error) => {
                    // The walk cannot be trusted after an error, so stop instead
                    // of emitting every later commit as unchanged.
                    self.failed = true;
                    return Some(Err(error));
                }
            };
            match self.changed(commit.id, commit.parents.first().copied()) {
                Ok(true) => return Some(Ok(commit.id)),
                Ok(false) => {}
                Err(error) => {
                    self.failed = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

impl PathHistory {
    fn is_cancelled(&self) -> bool {
        match &self.cancelled {
            Some(cancelled) => cancelled(),
            None => false,
        }
    }
}

impl PathHistory {
    fn changed(&mut self, commit: Oid, parent: Option<Oid>) -> Result<bool> {
        let tree = self.tree_id(commit)?;
        let Some(parent) = parent else {
            // A root commit changed the path exactly when the path exists there.
            // An empty path names the whole tree, which a root commit always has.
            return Ok(self.components.is_empty() || self.entry(tree)?.is_some());
        };
        let parent_tree = match self.tree_id(parent) {
            Ok(tree) => tree,
            // The first parent is outside the walk only in a shallow clone; its
            // tree cannot be read, so the commit is reported as changed rather
            // than silently dropped.
            Err(_) => return Ok(self.entry(tree)?.is_some()),
        };
        if tree == parent_tree {
            return Ok(false);
        }
        if self.components.is_empty() {
            return Ok(true);
        }
        Ok(self.entry(tree)? != self.entry(parent_tree)?)
    }

    /// The commit's root tree, from the commit-graph when it covers the commit.
    fn tree_id(&self, commit: Oid) -> Result<Oid> {
        if let Some(graph) = &self.graph
            && let Some(commit) = graph.commit_by_id(commit)
        {
            return Ok(commit.root_tree_id().to_owned());
        }
        let commit = self.repo.find_commit(commit).map_err(Error::gix)?;
        Ok(commit.tree_id().map_err(Error::gix)?.detach())
    }

    /// The entry `path` resolves to in `tree`, or `None` if it is absent.
    fn entry(&mut self, tree: Oid) -> Result<Option<Entry>> {
        if let Some(cached) = self.entries.get(&tree) {
            return Ok(*cached);
        }
        let entry = self.lookup(tree)?;
        if self.entries.len() >= ENTRY_CACHE_LIMIT {
            // The cache is only a locality hint; dropping it costs tree reads.
            self.entries.clear();
        }
        self.entries.insert(tree, entry);
        Ok(entry)
    }

    fn lookup(&mut self, mut tree: Oid) -> Result<Option<Entry>> {
        for (index, component) in self.components.iter().enumerate() {
            let data = self
                .repo
                .objects
                .find(&tree, &mut self.odb_buf)
                .map_err(Error::gix)?;
            if data.kind != gix::objs::Kind::Tree {
                return Ok(None);
            }
            let Some(entry) = TreeRefIter::from_bytes(data.data, data.object_hash)
                .filter_map(std::result::Result::ok)
                .find(|entry| entry.filename == component.as_slice())
            else {
                return Ok(None);
            };
            if index + 1 == self.components.len() {
                return Ok(Some((entry.oid.to_owned(), entry.mode.value())));
            }
            // Only the final component may be a file; anything above it is a
            // directory, so a non-tree entry means the path does not exist.
            if !entry.mode.is_tree() {
                return Ok(None);
            }
            tree = entry.oid.to_owned();
        }
        Ok(None)
    }
}

impl Repo {
    /// Like `git log --full-history --date-order HEAD --branches --remotes
    /// --tags refs/stash -- <path>`, without rename following.
    ///
    /// `path` may name a file or a directory; a directory matches changes to any
    /// entry below it because its tree id covers the whole subtree. An empty
    /// path names the repository root and reports every commit.
    pub fn path_history(&self, path: &str) -> Result<PathHistory> {
        self.path_history_with_cancel(path, None)
    }

    /// [`path_history`](Self::path_history) that stops once `cancelled` returns
    /// `true`, even between commits that do not match the path.
    pub(crate) fn path_history_cancellable(
        &self,
        path: &str,
        cancelled: CancelCheck,
    ) -> Result<PathHistory> {
        self.path_history_with_cancel(path, Some(cancelled))
    }

    fn path_history_with_cancel(
        &self,
        path: &str,
        cancelled: Option<CancelCheck>,
    ) -> Result<PathHistory> {
        let mut repo = self.local();
        let walk = self.walk_commits_with(cancelled.clone())?;
        // A missing or corrupt graph only costs speed here.
        let graph = repo.commit_graph_if_enabled().ok().flatten();
        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        Ok(PathHistory {
            repo,
            walk,
            components: components(path),
            graph,
            entries: HashMap::new(),
            odb_buf: Vec::new(),
            failed: false,
            cancelled,
        })
    }
}

/// Split a repository-relative path the way git does for a pathspec: repeated
/// and trailing slashes do not matter, and `.` components name the current tree.
fn components(path: &str) -> Vec<Vec<u8>> {
    path.split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(|component| component.as_bytes().to_vec())
        .collect()
}
