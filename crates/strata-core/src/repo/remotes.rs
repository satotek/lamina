//! Configured remotes.

use gix::remote::Direction;

use super::Repo;
use crate::{Error, RemoteInfo, Result};

impl Repo {
    /// Every configured remote, sorted by name.
    ///
    /// A remote without remote-tracking branches is included: the branches
    /// only exist after a fetch, while the remote itself is already usable for
    /// fetching and pushing.
    pub fn remotes(&self) -> Result<Vec<RemoteInfo>> {
        let mut repo = self.local();
        // `git remote add` and `git remote set-url` edit the configuration
        // while the session stays open; like `upstreams`, a reload is the only
        // way to see those edits.
        let _ = repo.reload();
        // `remote_names()` is a `BTreeSet`, so names arrive sorted.
        let mut remotes = Vec::new();
        for name in repo.remote_names() {
            let remote = repo.find_remote(&name).map_err(Error::gix)?;
            let fetch_url = remote.url(Direction::Fetch).map(url_string);
            // `url(Push)` already falls back to the fetch URL (with
            // `pushInsteadOf` applied), so a separate push URL is reported
            // exactly when pushes really go somewhere else.
            let push_url = remote
                .url(Direction::Push)
                .map(url_string)
                .filter(|push| Some(push) != fetch_url.as_ref());
            remotes.push(RemoteInfo {
                name: name.to_string(),
                fetch_url,
                push_url,
            });
        }
        Ok(remotes)
    }
}

fn url_string(url: &gix::Url) -> String {
    url.to_bstring().to_string()
}
