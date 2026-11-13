//! History limited to a path.
//!
//! A commit changed `path` when its tree entry for `path` differs from the entry
//! in its first parent. Only the path's own tree entries are read: the lookup
//! descends the tree along the path components and never compares whole trees.
//! With a commit-graph, tree ids come from the graph instead of re-parsing every
//! commit object.
//!
//! Renames are followed backwards, the way `git log --follow` does: when the
//! tracked path appears in a commit but is absent from its first parent, the
//! parent tree is diffed with rename detection and the tracked path becomes the
//! source of the matching rewrite for all older commits. Each reported commit
//! carries the name the file had there.

use std::collections::HashMap;

use gix::object::tree::diff::ChangeDetached;
use gix::objs::{FindExt, TreeRefIter};

use super::Repo;
use super::log::CancelCheck;
use super::log::PathWalk;
use crate::{CommitInfo, Error, Oid, Result};

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

/// One row of a path-limited history: the commit and the path the file had in
/// it, which differs from the queried path for commits before a rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathHistoryEntry {
    pub commit: Oid,
    /// Repository-relative path of the file at `commit`.
    pub path: String,
}

/// Streams [`PathHistoryEntry`]s for commits that changed the path, in the same
/// order as [`Repo::walk_commits`], following the file across renames.
///
/// A commit is reported when its tree entry for the tracked path differs from
/// its first parent's. That matches `git log --follow --full-history
/// --date-order HEAD --branches --remotes --tags refs/stash -- <path>` (with
/// `refs/stash` only when a stash exists) except for merge commits that are
/// TREESAME to their first parent but differ from another parent: git's full
/// history lists those, this walk does not, because it never traverses the
/// other parents.
pub struct PathHistoryEntries {
    repo: gix::Repository,
    walk: PathWalk,
    /// Path components of the file in the commit being inspected. Moves from
    /// the queried name to older names as the walk goes back past renames.
    components: Vec<Vec<u8>>,
    /// `components` joined back into a path for the caller.
    display_path: String,
    graph: Option<gix::commitgraph::Graph>,
    entries: HashMap<Oid, Option<Entry>>,
    odb_buf: Vec<u8>,
    failed: bool,
    /// `None` for the public, uncancellable walk.
    cancelled: Option<CancelCheck>,
}

impl Iterator for PathHistoryEntries {
    type Item = Result<PathHistoryEntry>;

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
            match self.changed(commit.id, commit.parent) {
                Ok(true) => {
                    // The entry names the file at this commit, before the
                    // tracked path moves to the old name for older commits.
                    let entry = PathHistoryEntry {
                        commit: commit.id,
                        path: self.display_path.clone(),
                    };
                    if let Err(error) = self.follow_rename(commit.id, commit.parent) {
                        self.failed = true;
                        return Some(Err(error));
                    }
                    return Some(Ok(entry));
                }
                Ok(false) => {}
                Err(error) => {
                    self.failed = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

impl PathHistoryEntries {
    pub(crate) fn read_commit_info(
        &mut self,
        id: Oid,
        parents_buf: &mut Vec<Oid>,
    ) -> Result<CommitInfo> {
        super::log::commit_info(&self.repo, id, &mut self.odb_buf, parents_buf)
    }

    fn is_cancelled(&self) -> bool {
        match &self.cancelled {
            Some(cancelled) => cancelled(),
            None => false,
        }
    }

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

    /// Move the tracked path to the old name when this commit renamed the file.
    ///
    /// Only a path that exists in `commit` but not in its parent can be the
    /// destination of a rename; a file that kept its name never runs the tree
    /// diff, which is what keeps this walk cheap.
    fn follow_rename(&mut self, commit: Oid, parent: Option<Oid>) -> Result<()> {
        let Some(parent) = parent else {
            return Ok(());
        };
        if self.components.is_empty() {
            return Ok(());
        }
        let commit_tree = self.tree_id(commit)?;
        let parent_tree = match self.tree_id(parent) {
            Ok(tree) => tree,
            Err(_) => return Ok(()),
        };
        if commit_tree == parent_tree
            || self.entry(commit_tree)?.is_none()
            || self.entry(parent_tree)?.is_some()
        {
            return Ok(());
        }

        let Some(old_path) = self.rename_source(parent_tree, commit_tree)? else {
            return Ok(());
        };
        self.set_path(&old_path);
        Ok(())
    }

    /// The parent-side path of the rename whose destination is the tracked
    /// path, as `git diff -M` would report it.
    fn rename_source(&self, parent_tree: Oid, commit_tree: Oid) -> Result<Option<Vec<u8>>> {
        let parent = self.repo.find_tree(parent_tree).map_err(Error::gix)?;
        let commit = self.repo.find_tree(commit_tree).map_err(Error::gix)?;
        let changes = self
            .repo
            .diff_tree_to_tree(
                Some(&parent),
                Some(&commit),
                Some(
                    gix::diff::Options::default()
                        .with_rewrites(Some(gix::diff::Rewrites::default())),
                ),
            )
            .map_err(Error::gix)?;

        for change in changes {
            if let ChangeDetached::Rewrite {
                location,
                source_location,
                copy: false,
                ..
            } = change
                && location.as_slice() == self.display_path.as_bytes()
            {
                return Ok(Some(source_location.into()));
            }
        }
        Ok(None)
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

    /// The entry the tracked path resolves to in `tree`, or `None` if absent.
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

    /// Point the walk at another name; cached entries were resolved for the old
    /// one, so they cannot be reused.
    fn set_path(&mut self, path: &[u8]) {
        self.components = path
            .split(|byte| *byte == b'/')
            .filter(|component| {
                let component: &[u8] = component;
                !component.is_empty() && component != b"."
            })
            .map(<[u8]>::to_vec)
            .collect();
        self.display_path = self
            .components
            .iter()
            .map(|component| String::from_utf8_lossy(component))
            .collect::<Vec<_>>()
            .join("/");
        self.entries.clear();
    }
}

/// Streams ids of commits that changed `path`, for callers that do not need the
/// per-commit path.
pub struct PathHistory {
    entries: PathHistoryEntries,
}

impl Iterator for PathHistory {
    type Item = Result<Oid>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.entries.next()?.map(|entry| entry.commit))
    }
}

impl Repo {
    /// Like `git log --follow --full-history --date-order HEAD --branches
    /// --remotes --tags refs/stash -- <path>`.
    ///
    /// `path` may name a file or a directory; a directory matches changes to any
    /// entry below it because its tree id covers the whole subtree. An empty
    /// path names the repository root and reports every commit. Only files
    /// follow renames; a directory only follows its own path components, which
    /// is also how git treats a pathspec.
    pub fn path_history(&self, path: &str) -> Result<PathHistory> {
        self.path_history_with_cancel(path, None)
    }

    /// [`path_history`](Self::path_history) that also reports the name the file
    /// had in each commit, for an old-name column in the UI.
    pub fn path_history_entries(&self, path: &str) -> Result<PathHistoryEntries> {
        self.path_history_entries_with_cancel(path, None)
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

    /// [`path_history_entries`](Self::path_history_entries) that stops once
    /// `cancelled` returns `true`, even between commits that do not match the path.
    pub(crate) fn path_history_entries_cancellable(
        &self,
        path: &str,
        cancelled: CancelCheck,
    ) -> Result<PathHistoryEntries> {
        self.path_history_entries_with_cancel(path, Some(cancelled))
    }

    fn path_history_with_cancel(
        &self,
        path: &str,
        cancelled: Option<CancelCheck>,
    ) -> Result<PathHistory> {
        Ok(PathHistory {
            entries: self.path_history_entries_with_cancel(path, cancelled)?,
        })
    }

    fn path_history_entries_with_cancel(
        &self,
        path: &str,
        cancelled: Option<CancelCheck>,
    ) -> Result<PathHistoryEntries> {
        let mut repo = self.local();
        let walk = self.path_walk_with(cancelled.clone())?;
        // A missing or corrupt graph only costs speed here.
        let graph = repo.commit_graph_if_enabled().ok().flatten();
        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        let mut entries = PathHistoryEntries {
            repo,
            walk,
            components: Vec::new(),
            display_path: String::new(),
            graph,
            entries: HashMap::new(),
            odb_buf: Vec::new(),
            failed: false,
            cancelled,
        };
        entries.set_path(path.as_bytes());
        Ok(entries)
    }
}
