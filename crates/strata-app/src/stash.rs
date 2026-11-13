//! Stashing from the UI: the stash dialog and the sidebar's stash entries.
//!
//! Every write goes through `Session::run(Operation::Stash…)`.

use gpui::{Context, Window};

use crate::repo_view::RepoView;

impl RepoView {
    /// `StashChanges`: asks for a message and stashes the working tree.
    pub(crate) fn stash_changes(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        // Implemented by the stash task.
    }
}
