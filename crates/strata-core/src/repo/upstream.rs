//! Upstream tracking of local branches. Owned by the upstream task.

use std::collections::HashMap;

use super::Repo;
use crate::{Result, UpstreamInfo};

impl Repo {
    /// Upstream state of every local branch that has one configured, keyed by
    /// the branch's short name (e.g. `main`).
    pub fn upstreams(&self) -> Result<HashMap<String, UpstreamInfo>> {
        todo!("upstream task")
    }
}
