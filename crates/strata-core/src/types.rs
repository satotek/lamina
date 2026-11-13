//! Plain data types shared between core modules and the UI.

pub use gix::ObjectId as Oid;

/// One commit as shown in the log. Owned and cheap to clone into the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: Oid,
    pub parents: Vec<Oid>,
    /// First line of the message.
    pub summary: String,
    pub author_name: String,
    pub author_email: String,
    /// Seconds since the Unix epoch.
    pub author_time: i64,
    pub committer_time: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInfo {
    /// Full name, e.g. `refs/heads/main`.
    pub full_name: String,
    /// Short name, e.g. `main` or `origin/main`.
    pub short_name: String,
    pub kind: RefKind,
    /// Peeled commit id (annotated tags are peeled to their commit).
    pub target: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadInfo {
    /// HEAD points to a branch. `target` is `None` for an unborn branch.
    Branch {
        short_name: String,
        target: Option<Oid>,
    },
    Detached {
        target: Oid,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

/// A file changed between two trees (commit view) or in index/worktree (status view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Repository-relative path using `/` separators.
    pub path: String,
    /// Previous path for renames/copies.
    pub old_path: Option<String>,
    pub kind: ChangeKind,
}

/// What a tree entry is, as the file list of a commit needs to draw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TreeEntryKind {
    File,
    Directory,
    /// A symbolic link; its blob content is the link target.
    Symlink,
    /// A gitlink to another repository.
    Submodule,
}

/// One entry of a directory at a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The entry's name inside its directory.
    pub name: String,
    /// Repository-relative path using `/` separators.
    pub path: String,
    pub kind: TreeEntryKind,
    /// The raw git mode, e.g. `0o100644` or `0o120000`.
    pub mode: u16,
    /// Blob size in bytes for files and symlinks. `None` for directories and
    /// submodules, and when the object header cannot be read cheaply.
    pub size: Option<u64>,
}

/// Working tree status split the way the staging UI shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeStatus {
    /// HEAD vs index.
    pub staged: Vec<FileChange>,
    /// Index vs working tree, including untracked files.
    pub unstaged: Vec<FileChange>,
}

/// One entry of the stash list. `index` is the `N` in `stash@{N}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashInfo {
    pub index: usize,
    pub id: Oid,
    pub message: String,
}

/// Tracking state of a local branch against its upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamInfo {
    /// Short name of the upstream, e.g. `origin/main`.
    pub name: String,
    /// Commits on the local branch that the upstream lacks.
    pub ahead: u32,
    /// Commits on the upstream that the local branch lacks.
    pub behind: u32,
    /// The configured upstream ref no longer exists (e.g. deleted on the remote and pruned).
    pub gone: bool,
}

/// A configured remote.
///
/// A remote is listed even while it has no remote-tracking branches: fetching
/// and pushing only need its URLs, and the UI offers every configured remote
/// before the first fetch created any refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    /// The first `remote.<name>.url`, as git resolves it.
    pub fetch_url: Option<String>,
    /// The push URL, but only when pushes do not already use `fetch_url`.
    pub push_url: Option<String>,
}

/// An operation git left half-done, as recorded in the git dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RepoState {
    #[default]
    Normal,
    Merging,
    Rebasing,
    CherryPicking,
    Reverting,
    Bisecting,
    /// `git am` / applying a mailbox.
    ApplyingPatches,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    BothModified,
    BothAdded,
    BothDeleted,
    AddedByUs,
    AddedByThem,
    DeletedByUs,
    DeletedByThem,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictEntry {
    pub path: String,
    pub kind: ConflictKind,
}

/// Index stages of a conflicted path: 1 = common ancestor, 2 = ours, 3 = theirs.
/// A side is `None` when that stage is absent (e.g. deleted on that side).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictSides {
    pub base: Option<Vec<u8>>,
    pub ours: Option<Vec<u8>>,
    pub theirs: Option<Vec<u8>>,
}
