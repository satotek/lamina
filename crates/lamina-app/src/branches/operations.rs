//! What each branch or remote command of the sidebar hands to the session.

use gpui::{Context, Window};
use lamina_core::session::{HistoryScope, Operation};
use lamina_core::{RefKind, RemoteInfo, UpstreamInfo};

use crate::data::Source;
use crate::repo_view::RepoView;

use super::Retry;
use super::refs::UpstreamChoice;
use super::refs::{local_name_for_remote, ref_full_name, upstream_choices};

/// Checks out `remote` as `local`, creating `local` from the remote ref when it
/// does not exist yet.
pub(crate) fn checkout_remote(
    view: &mut RepoView,
    remote: &str,
    local: &str,
    existing: bool,
    window: &mut Window,
    cx: &mut Context<RepoView>,
) {
    if existing {
        let label = t!("branch.op.checkout", branch = local).to_string();
        view.run_operation(
            Operation::Checkout {
                branch: local.to_owned(),
            },
            label,
            window,
            cx,
        );
    } else {
        let label = t!(
            "branch.op.create_from_remote",
            local = local,
            remote = remote
        )
        .to_string();
        view.run_operation(
            Operation::CreateBranch {
                name: local.to_owned(),
                start_point: Some(remote.to_owned()),
                checkout: true,
            },
            label,
            window,
            cx,
        );
    }
}

impl RepoView {
    /// The upstream of a local branch, as the last snapshot read it.
    pub(crate) fn branch_upstream(&self, branch: &str) -> Option<UpstreamInfo> {
        match self.source()? {
            Source::Repo(snapshot) => snapshot.upstreams.get(branch).cloned(),
            Source::Fake(_) => None,
        }
    }

    /// One configured remote from the last snapshot.
    pub(crate) fn configured_remote(&self, name: &str) -> Option<RemoteInfo> {
        match self.source()? {
            Source::Repo(snapshot) => snapshot
                .remotes
                .iter()
                .find(|remote| remote.name == name)
                .cloned(),
            Source::Fake(_) => None,
        }
    }

    /// Every remote branch the set-upstream dialog can offer.
    pub(crate) fn upstream_choices(&self) -> Vec<UpstreamChoice> {
        match self.source() {
            Some(Source::Repo(snapshot)) => upstream_choices(&snapshot.refs, &snapshot.remotes),
            _ => Vec::new(),
        }
    }

    /// Checks out a branch: a local branch directly, a remote one as its local
    /// counterpart, created from the remote ref when it does not exist yet.
    ///
    /// The context menus and the sidebar's rows share this, so a branch is
    /// checked out the same way wherever it was clicked.
    pub(crate) fn checkout_branch(
        &mut self,
        kind: RefKind,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_busy() {
            return;
        }
        match kind {
            RefKind::LocalBranch => {
                let label = t!("branch.op.checkout", branch = name).to_string();
                self.run_operation(
                    Operation::Checkout {
                        branch: name.to_owned(),
                    },
                    label,
                    window,
                    cx,
                );
            }
            RefKind::RemoteBranch => {
                let Some(local) = local_name_for_remote(name) else {
                    return;
                };
                let existing = self.has_local_branch(local);
                checkout_remote(self, name, local, existing, window, cx);
            }
            RefKind::Tag => {}
        }
    }

    /// Merges `branch` into the checked-out one.
    pub(crate) fn merge_branch(
        &mut self,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("branch.op.merge", branch = branch).to_string();
        self.run_operation(
            Operation::Merge {
                branch: branch.to_owned(),
                no_ff: false,
            },
            label,
            window,
            cx,
        );
    }

    /// Renames a local branch through the same form the sidebar opens.
    pub(crate) fn rename_branch(
        &mut self,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_rename_branch(branch.to_owned(), window, cx);
    }

    /// Deletes a local branch through the same confirmation the sidebar opens.
    pub(crate) fn delete_branch_here(
        &mut self,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_delete_branch(branch.to_owned(), window, cx);
    }

    pub(crate) fn has_local_branch(&self, branch: &str) -> bool {
        self.session.as_ref().is_some_and(|session| {
            session
                .snapshot()
                .refs
                .iter()
                .any(|info| info.kind == RefKind::LocalBranch && info.short_name == branch)
        })
    }

    /// Asks before removing a remote: its remote-tracking branches go with it.
    pub(crate) fn confirm_remove_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_none() {
            return;
        }
        self.focus_root(window, cx);
        let entity = cx.weak_entity();
        crate::ui::confirm(
            window,
            cx,
            crate::ui::Confirm::new(
                t!("branch.confirm.remove_remote", name = name).to_string(),
                t!("branch.confirm.remove_remote_detail").to_string(),
            )
            .ok_label(t!("branch.confirm.remove_remote_button").to_string()),
            move |window, cx| {
                let name = name.clone();
                let _ = entity.update(cx, |view, cx| view.remove_remote(name, window, cx));
            },
        );
    }

    pub(crate) fn remove_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("branch.op.remove_remote", name = name).to_string();
        self.run_operation(Operation::RemoveRemote { name }, label, window, cx);
    }

    pub(crate) fn fetch_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prune = crate::settings::Settings::get(cx).behaviour.prune_on_fetch;
        let label = t!("branch.op.fetch", name = name).to_string();
        self.run_operation(
            Operation::Fetch {
                remote: Some(name),
                prune,
            },
            label,
            window,
            cx,
        );
    }

    pub(crate) fn unset_branch_upstream(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = t!("branch.op.unset_upstream", branch = branch).to_string();
        self.run_operation(Operation::UnsetBranchUpstream { branch }, label, window, cx);
    }

    /// Runs the force form of an operation the user just confirmed.
    pub(crate) fn run_retry(&mut self, retry: Retry, window: &mut Window, cx: &mut Context<Self>) {
        match retry {
            Retry::DeleteBranch { name, .. } => self.delete_branch(name, true, window, cx),
            Retry::RemoveWorktree { path, .. } => self.remove_worktree(path, true, window, cx),
        }
    }

    /// Reloads history limited to `short_name`'s history and remembers the name
    /// for the sidebar banner. The full ref name is what the walk resolves, so
    /// a branch that is not `refs/heads/...` still reads exactly.
    pub(crate) fn show_only_ref(
        &mut self,
        kind: RefKind,
        short_name: &str,
        cx: &mut Context<Self>,
    ) {
        // The helper's walk is not refiltered until a later phase, so applying
        // the scope locally would show a graph the request did not change.
        if self.refuse_if_ssh(cx) {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let spec = session
            .snapshot()
            .refs
            .iter()
            .find(|info| info.kind == kind && info.short_name == short_name)
            .map(|info| info.full_name.clone())
            .unwrap_or_else(|| short_name.to_owned());
        session.set_history_scope(HistoryScope::Refs(vec![spec]));
        self.branches.set_scope(Some(short_name.to_owned()));
        self.branches.clear_revealed();
        cx.notify();
    }

    /// Restores every ref as a history starting point, undoing "show only".
    pub(crate) fn show_all_refs(&mut self, cx: &mut Context<Self>) {
        if self.refuse_if_ssh(cx) {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        session.set_history_scope(HistoryScope::All);
        self.branches.set_scope(None);
        self.branches.clear_revealed();
        cx.notify();
    }

    /// Whether `kind`/`short_name` is kept out of the history graph.
    pub(crate) fn is_ref_hidden(&self, kind: RefKind, short_name: &str) -> bool {
        let full_name = ref_full_name(kind, short_name);
        self.hidden_refs.contains(&full_name)
    }

    /// How many refs the history graph leaves out.
    pub(crate) fn hidden_ref_count(&self) -> usize {
        self.hidden_refs.len()
    }

    /// Leaves `kind`/`short_name`'s history out of the graph until "show all".
    ///
    /// The ref itself stays in the sidebar, so the branch can still be checked
    /// out or merged; only the walk that starts at its tip is dropped.
    pub(crate) fn hide_ref_in_history(
        &mut self,
        kind: RefKind,
        short_name: &str,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_if_ssh(cx) || self.session.is_none() {
            return;
        }
        let full_name = ref_full_name(kind, short_name);
        if self.hidden_refs.contains(&full_name) {
            return;
        }
        self.hidden_refs.push(full_name.clone());
        if let Some(key) = self.repository_key.clone() {
            crate::settings::update(cx, |settings| {
                settings.history.hide_ref(&key, full_name.clone())
            });
        }
        if let Some(session) = self.session.as_ref() {
            session.set_hidden_refs(self.hidden_refs.clone());
        }
        cx.notify();
    }

    /// Restores every hidden ref to the history graph.
    pub(crate) fn show_hidden_refs(&mut self, cx: &mut Context<Self>) {
        if self.refuse_if_ssh(cx) || self.session.is_none() {
            return;
        }
        if self.hidden_refs.is_empty() {
            return;
        }
        self.hidden_refs.clear();
        if let Some(key) = self.repository_key.clone() {
            crate::settings::update(cx, |settings| settings.history.show_all_refs(&key));
        }
        if let Some(session) = self.session.as_ref() {
            session.set_hidden_refs(Vec::new());
        }
        cx.notify();
    }
}
