//! Traits abstracting an open repository session for the UI.
//!
//! Keeping the UI behind [`RepositorySession`], [`RepoReader`], and
//! [`GitExtras`] decouples rendering and view state from the local `gix` /
//! `git` implementation so remote backends can be added without UI changes.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::git_cli::{CommandLog, Git};
use crate::lfs::{LfsInfo, LfsLock, LfsPointer, read_object};
use crate::merge::ConflictStyle;
use crate::repo::{CompareMode, MergeTool, Repo, SelectionRange, UpstreamTarget};
use crate::session::{
    CommitQuery, ContentSearchQuery, DiffOptions, DiffRequest, DiffTicket, HistoryScope, Operation,
    OperationId, QueryTicket, RepoSnapshot, Session,
};
use crate::{ConflictSides, FileChange, HeadInfo, Oid, Result, TreeEntry};

/// Where an open repository resides.
///
/// Used by the UI for tab identity, window titles, and repository-scoped
/// settings instead of assuming every repository is on the local filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RepoLocation {
    Local(PathBuf),
}

impl RepoLocation {
    /// Local repository root (working tree root, or git directory for a bare
    /// repository), if this repository lives on the local machine.
    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path.as_path()),
        }
    }

    /// Display representation of the repository location.
    pub fn display(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
        }
    }
}

impl fmt::Display for RepoLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(path) => write!(f, "{}", path.display()),
        }
    }
}

/// On-demand repository reads the UI runs outside [`RepoSnapshot`].
///
/// Returned behind an `Arc` so UI views can dispatch reads on a background
/// executor without borrowing the session.
pub trait RepoReader: Send + Sync {
    fn workdir(&self) -> Option<&Path>;
    fn git_dir(&self) -> &Path;
    fn head(&self) -> Result<HeadInfo>;
    fn commit_changes(&self, commit: Oid) -> Result<Vec<FileChange>>;
    fn tree_entries(&self, commit: Oid, dir_path: &str) -> Result<Vec<TreeEntry>>;
    fn blob_at(&self, commit: Oid, path: &str) -> Result<Vec<u8>>;
    fn file_in_worktree(&self, path: &str) -> Result<Option<Vec<u8>>>;
    fn working_tree_encoding(&self, path: &str) -> Option<String>;
    fn lfs_info_at(&self, commit: Oid, path: &str) -> Result<Option<LfsInfo>>;
    fn lfs_info_in_index(&self, path: &str) -> Result<Option<LfsInfo>>;
    fn lfs_info_in_worktree(&self, path: &str) -> Result<Option<LfsInfo>>;
    fn uses_lfs(&self) -> bool;
    /// Reads the stored LFS object for `pointer` when present and within `max_bytes`.
    fn read_lfs_object(&self, pointer: &LfsPointer, max_bytes: usize) -> Result<Option<Vec<u8>>>;
    fn parents(&self, commit: Oid) -> Result<Vec<Oid>>;
    fn commits_between(&self, base: Oid, tip: Oid) -> Result<Vec<Oid>>;
    fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool>;
    fn commit_message(&self, commit: Oid) -> Result<String>;
    fn merge_message(&self) -> Result<Option<String>>;
    fn conflict_sides(&self, path: &str) -> Result<ConflictSides>;
    fn conflict_style(&self) -> ConflictStyle;
    fn merge_tool(&self) -> Option<MergeTool>;
    fn selection_range(&self, selected: &[Oid]) -> Result<Option<SelectionRange>>;
    fn commit_changed_file_count(&self, commit: Oid) -> Result<usize>;
    fn branch_upstream_target(&self, branch: &str) -> Result<Option<UpstreamTarget>>;
    /// Resolves a revision spec (ref, tag, or object id) to its commit id.
    fn resolve_commit(&self, spec: &str) -> Result<Option<Oid>>;
}

impl RepoReader for Repo {
    fn workdir(&self) -> Option<&Path> {
        Repo::workdir(self)
    }

    fn git_dir(&self) -> &Path {
        Repo::git_dir(self)
    }

    fn head(&self) -> Result<HeadInfo> {
        Repo::head(self)
    }

    fn commit_changes(&self, commit: Oid) -> Result<Vec<FileChange>> {
        Repo::commit_changes(self, commit)
    }

    fn tree_entries(&self, commit: Oid, dir_path: &str) -> Result<Vec<TreeEntry>> {
        Repo::tree_entries(self, commit, dir_path)
    }

    fn blob_at(&self, commit: Oid, path: &str) -> Result<Vec<u8>> {
        Repo::blob_at(self, commit, path)
    }

    fn file_in_worktree(&self, path: &str) -> Result<Option<Vec<u8>>> {
        Repo::file_in_worktree(self, path)
    }

    fn working_tree_encoding(&self, path: &str) -> Option<String> {
        Repo::working_tree_encoding(self, path)
    }

    fn lfs_info_at(&self, commit: Oid, path: &str) -> Result<Option<LfsInfo>> {
        Repo::lfs_info_at(self, commit, path)
    }

    fn lfs_info_in_index(&self, path: &str) -> Result<Option<LfsInfo>> {
        Repo::lfs_info_in_index(self, path)
    }

    fn lfs_info_in_worktree(&self, path: &str) -> Result<Option<LfsInfo>> {
        Repo::lfs_info_in_worktree(self, path)
    }

    fn uses_lfs(&self) -> bool {
        Repo::uses_lfs(self)
    }

    fn read_lfs_object(&self, pointer: &LfsPointer, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        read_object(self.common_dir(), pointer, max_bytes)
    }

    fn parents(&self, commit: Oid) -> Result<Vec<Oid>> {
        Repo::parents(self, commit)
    }

    fn commits_between(&self, base: Oid, tip: Oid) -> Result<Vec<Oid>> {
        Repo::commits_between(self, base, tip)
    }

    fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool> {
        Repo::is_ancestor(self, ancestor, descendant)
    }

    fn commit_message(&self, commit: Oid) -> Result<String> {
        Repo::commit_message(self, commit)
    }

    fn merge_message(&self) -> Result<Option<String>> {
        Repo::merge_message(self)
    }

    fn conflict_sides(&self, path: &str) -> Result<ConflictSides> {
        Repo::conflict_sides(self, path)
    }

    fn conflict_style(&self) -> ConflictStyle {
        Repo::conflict_style(self)
    }

    fn merge_tool(&self) -> Option<MergeTool> {
        Repo::merge_tool(self)
    }

    fn selection_range(&self, selected: &[Oid]) -> Result<Option<SelectionRange>> {
        Repo::selection_range(self, selected)
    }

    fn commit_changed_file_count(&self, commit: Oid) -> Result<usize> {
        Repo::commit_changed_file_count(self, commit)
    }

    fn branch_upstream_target(&self, branch: &str) -> Result<Option<UpstreamTarget>> {
        Repo::branch_upstream_target(self, branch)
    }

    fn resolve_commit(&self, spec: &str) -> Result<Option<Oid>> {
        let mut walk = self.walk_commits_from_refs(&[spec.to_owned()])?;
        match walk.next() {
            Some(Ok(commit)) => Ok(Some(commit.id)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }
}

/// `git` CLI helpers the UI invokes outside serialized [`Operation`]s.
///
/// Returned behind an `Arc` so blocking probes (`lfs_available`, `lfs_locks`,
/// `format_patch`) can move onto background executors.
pub trait GitExtras: Send + Sync {
    fn workdir(&self) -> &Path;
    fn log_handle(&self) -> Arc<CommandLog>;
    fn lfs_available(&self) -> bool;
    fn lfs_locks(&self) -> Result<Vec<LfsLock>>;
    fn format_patch(&self, commit: Oid) -> Result<Vec<u8>>;
}

impl GitExtras for Git {
    fn workdir(&self) -> &Path {
        Git::workdir(self)
    }

    fn log_handle(&self) -> Arc<CommandLog> {
        Git::log_handle(self)
    }

    fn lfs_available(&self) -> bool {
        Git::lfs_available(self)
    }

    fn lfs_locks(&self) -> Result<Vec<LfsLock>> {
        Git::lfs_locks(self)
    }

    fn format_patch(&self, commit: Oid) -> Result<Vec<u8>> {
        Git::format_patch(self, commit)
    }
}

/// Object-safe session interface consumed by the UI.
///
/// Local repositories are backed by [`Session`]; future remote backends can
/// implement this trait without touching UI callers.
pub trait RepositorySession: Send {
    fn location(&self) -> RepoLocation;
    fn reader(&self) -> Arc<dyn RepoReader>;
    fn git_extras(&self) -> Arc<dyn GitExtras>;

    fn snapshot(&self) -> Arc<RepoSnapshot>;
    fn run(&self, op: Operation) -> OperationId;
    fn answer_askpass(&self, id: OperationId, answer: Option<String>) -> bool;

    fn request_diff(&self, request: DiffRequest) -> DiffTicket;
    fn request_diff_with(&self, request: DiffRequest, options: DiffOptions) -> DiffTicket;
    fn cancel_diff(&self, ticket: DiffTicket);

    fn search_commits(&self, query: CommitQuery) -> QueryTicket;
    fn path_history(&self, path: String) -> QueryTicket;
    fn path_history_streaming(&self, path: String) -> QueryTicket;
    fn request_commit_stat(&self, commit: Oid) -> QueryTicket;
    fn request_compare(&self, a: Oid, b: Oid, mode: CompareMode) -> QueryTicket;
    fn request_compare_from(&self, base: Option<Oid>, head: Oid) -> QueryTicket;
    fn request_compare_with_worktree(&self, base: Oid) -> QueryTicket;
    fn request_submodules_and_worktrees(&self) -> QueryTicket;
    fn request_reflog(&self, limit: usize) -> QueryTicket;
    fn request_in_progress_details(&self) -> QueryTicket;
    fn request_blame(&self, commit: Oid, path: String) -> QueryTicket;
    fn content_search_streaming(&self, query: ContentSearchQuery) -> QueryTicket;
    fn request_commit_signatures(&self, commits: Vec<Oid>) -> QueryTicket;
    fn cancel_query(&self, ticket: QueryTicket);

    fn reload(&self);
    fn set_history_scope(&self, scope: HistoryScope);
    fn history_scope(&self) -> HistoryScope;
    fn set_hidden_refs(&self, refs: Vec<String>);
    fn hidden_refs(&self) -> Vec<String>;
}

impl RepositorySession for Session {
    fn location(&self) -> RepoLocation {
        let root = self
            .shared
            .repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.shared.repo.git_dir().to_path_buf());
        RepoLocation::Local(root)
    }

    fn reader(&self) -> Arc<dyn RepoReader> {
        Arc::clone(&self.reader)
    }

    fn git_extras(&self) -> Arc<dyn GitExtras> {
        Arc::clone(&self.git_extras)
    }

    fn snapshot(&self) -> Arc<RepoSnapshot> {
        Session::snapshot(self)
    }

    fn run(&self, op: Operation) -> OperationId {
        Session::run(self, op)
    }

    fn answer_askpass(&self, id: OperationId, answer: Option<String>) -> bool {
        Session::answer_askpass(self, id, answer)
    }

    fn request_diff(&self, request: DiffRequest) -> DiffTicket {
        Session::request_diff(self, request)
    }

    fn request_diff_with(&self, request: DiffRequest, options: DiffOptions) -> DiffTicket {
        Session::request_diff_with(self, request, options)
    }

    fn cancel_diff(&self, ticket: DiffTicket) {
        Session::cancel_diff(self, ticket);
    }

    fn search_commits(&self, query: CommitQuery) -> QueryTicket {
        Session::search_commits(self, query)
    }

    fn path_history(&self, path: String) -> QueryTicket {
        Session::path_history(self, path)
    }

    fn path_history_streaming(&self, path: String) -> QueryTicket {
        Session::path_history_streaming(self, path)
    }

    fn request_commit_stat(&self, commit: Oid) -> QueryTicket {
        Session::request_commit_stat(self, commit)
    }

    fn request_compare(&self, a: Oid, b: Oid, mode: CompareMode) -> QueryTicket {
        Session::request_compare(self, a, b, mode)
    }

    fn request_compare_from(&self, base: Option<Oid>, head: Oid) -> QueryTicket {
        Session::request_compare_from(self, base, head)
    }

    fn request_compare_with_worktree(&self, base: Oid) -> QueryTicket {
        Session::request_compare_with_worktree(self, base)
    }

    fn request_submodules_and_worktrees(&self) -> QueryTicket {
        Session::request_submodules_and_worktrees(self)
    }

    fn request_reflog(&self, limit: usize) -> QueryTicket {
        Session::request_reflog(self, limit)
    }

    fn request_in_progress_details(&self) -> QueryTicket {
        Session::request_in_progress_details(self)
    }

    fn request_blame(&self, commit: Oid, path: String) -> QueryTicket {
        Session::request_blame(self, commit, path)
    }

    fn content_search_streaming(&self, query: ContentSearchQuery) -> QueryTicket {
        Session::content_search_streaming(self, query)
    }

    fn request_commit_signatures(&self, commits: Vec<Oid>) -> QueryTicket {
        Session::request_commit_signatures(self, commits)
    }

    fn cancel_query(&self, ticket: QueryTicket) {
        Session::cancel_query(self, ticket);
    }

    fn reload(&self) {
        Session::reload(self);
    }

    fn set_history_scope(&self, scope: HistoryScope) {
        Session::set_history_scope(self, scope);
    }

    fn history_scope(&self) -> HistoryScope {
        Session::history_scope(self)
    }

    fn set_hidden_refs(&self, refs: Vec<String>) {
        Session::set_hidden_refs(self, refs);
    }

    fn hidden_refs(&self) -> Vec<String> {
        Session::hidden_refs(self)
    }
}
