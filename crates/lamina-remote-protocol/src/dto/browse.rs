use serde::{Deserialize, Serialize};

/// One subdirectory of a listed folder on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntryDto {
    pub name: String,
    /// A work tree with `.git`, or a bare repository, so the picker can mark
    /// what is worth opening without a second round trip per entry.
    pub repository: bool,
}

/// A folder on the host and the subdirectories in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirListingDto {
    /// Absolute and canonical, so `..` and symlinks never reach the app.
    pub path: String,
    /// `None` at the filesystem root.
    pub parent: Option<String>,
    /// Whether `path` itself is a repository.
    pub repository: bool,
    /// Sorted by name, case-insensitively. Files are left out: only a folder
    /// can be opened.
    pub entries: Vec<DirEntryDto>,
    /// The helper stopped at its entry limit.
    pub truncated: bool,
}
