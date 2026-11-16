//! LFS metadata the helper reports. The object path stays on the host: a
//! client names an id, and the helper turns a checked `sha256:` id into
//! `lfs/objects/aa/bb/<hex>`.

use serde::{Deserialize, Serialize};

/// Pointer metadata, or the size of a checked-out LFS file that has no pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfsInfoDto {
    /// `sha256:…`, or empty when the worktree holds the real file.
    pub oid: String,
    pub size: u64,
    pub present: bool,
}

impl From<&strata_core::lfs::LfsInfo> for LfsInfoDto {
    fn from(info: &strata_core::lfs::LfsInfo) -> Self {
        Self {
            oid: info.oid.clone(),
            size: info.size,
            present: info.present,
        }
    }
}

impl From<LfsInfoDto> for strata_core::lfs::LfsInfo {
    fn from(info: LfsInfoDto) -> Self {
        Self {
            oid: info.oid,
            size: info.size,
            present: info.present,
        }
    }
}

/// One lock from `git lfs locks --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfsLockDto {
    pub id: String,
    pub path: String,
    pub owner: Option<String>,
    pub locked_at: Option<String>,
}

impl From<&strata_core::lfs::LfsLock> for LfsLockDto {
    fn from(lock: &strata_core::lfs::LfsLock) -> Self {
        Self {
            id: lock.id.clone(),
            path: lock.path.clone(),
            owner: lock.owner.clone(),
            locked_at: lock.locked_at.clone(),
        }
    }
}

impl From<LfsLockDto> for strata_core::lfs::LfsLock {
    fn from(lock: LfsLockDto) -> Self {
        Self {
            id: lock.id,
            path: lock.path,
            owner: lock.owner,
            locked_at: lock.locked_at,
        }
    }
}
