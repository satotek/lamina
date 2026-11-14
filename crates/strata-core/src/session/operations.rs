//! The operation worker: git CLI mutations, strictly one at a time.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::Result;
use crate::diff::{DiffOptions, diff_text};
use crate::error::Error;
use crate::git_cli::{ApplyMode, CommitOptions};
use crate::patch::{LineEnding, LineSelection, PatchFile, PatchMode, build_patch};
use crate::session::history::LoadCommand;
use crate::session::publish::Shared;
use crate::session::refresh::RefreshStatus;
use crate::session::{
    ConflictSide, Operation, OperationId, SessionEvent, head_file, worktree_as_git,
};

/// A wedged status refresh must not block the operation queue forever. The
/// limit is far above any real status scan.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

/// Git emits progress far faster than a UI can render it; 20 events per second
/// keeps the bar moving without flooding the event channel.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Throttles git's progress lines into `OperationProgress` events while keeping
/// the last line, which is always reported when the command ends: it carries
/// the outcome ("main -> origin/main") the intermediate counters do not.
struct ProgressReporter<'a> {
    shared: &'a Shared,
    id: OperationId,
    last: Option<(String, Option<u8>)>,
    emitted: Option<(String, Option<u8>)>,
    emitted_at: Option<Instant>,
}

impl<'a> ProgressReporter<'a> {
    fn new(shared: &'a Shared, id: OperationId) -> Self {
        Self {
            shared,
            id,
            last: None,
            emitted: None,
            emitted_at: None,
        }
    }

    fn line(&mut self, text: &str, percent: Option<u8>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.last = Some((text.to_owned(), percent));
        let due = self
            .emitted_at
            .is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL);
        if due {
            self.emit_last();
        }
    }

    /// The final line is reported even if the throttle would have dropped it.
    fn finish(mut self) {
        if self.last != self.emitted {
            self.emit_last();
        }
    }

    fn emit_last(&mut self) {
        let Some((text, percent)) = self.last.clone() else {
            return;
        };
        self.shared.emit(SessionEvent::OperationProgress {
            id: self.id,
            text: text.clone(),
            percent,
        });
        self.emitted = Some((text, percent));
        self.emitted_at = Some(Instant::now());
    }
}

pub(crate) struct OperationJob {
    pub(crate) id: OperationId,
    pub(crate) operation: Operation,
}

pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<OperationJob>,
    status: Sender<RefreshStatus>,
    load: Sender<LoadCommand>,
) {
    while let Ok(job) = rx.recv() {
        if shared.is_shutdown() {
            return;
        }
        run_one(&shared, job, &status, &load);
    }
}

fn run_one(
    shared: &Arc<Shared>,
    job: OperationJob,
    status: &Sender<RefreshStatus>,
    load: &Sender<LoadCommand>,
) {
    shared.set_operation_running(true);
    shared.publish(|snapshot| snapshot.busy = Some(job.id));
    shared.emit(SessionEvent::OperationStarted { id: job.id });

    let result = execute(shared, job.id, &job.operation);
    // A prompt answered as the command ended must not leave a waiter for the
    // next command that reuses the id.
    shared.forget_prompt(job.id);

    // Refreshes held back during the command are dropped if the session
    // forgets them here: a status scan cannot see a ref that moved. Held
    // index/worktree changes are covered by the refresh requested below.
    let held = shared.take_pending();
    let history_stale = held.refs_changed()
        || (result.is_ok()
            && (changes_refs(&job.operation)
                || matches!(job.operation, Operation::WriteCommitGraph)));
    if history_stale {
        let generation = shared.begin_reload();
        let _ = load.send(LoadCommand { generation });
    }

    // Clearing `busy` before the refresh lands would flash the pre-operation
    // status, which is exactly what a commit must not do. A full history reload
    // already re-reads the configuration, so only carry the config flag into
    // this refresh when no reload covers it.
    let (ack_tx, ack_rx) = mpsc::channel();
    let _ = status.send(RefreshStatus {
        ack: Some(ack_tx),
        config: held.config && !history_stale,
        ..RefreshStatus::default()
    });
    let _ = ack_rx.recv_timeout(REFRESH_TIMEOUT);

    shared.publish(|snapshot| snapshot.busy = None);
    let error = result.err().map(|error| error_message(&error));
    shared.emit(SessionEvent::OperationFinished { id: job.id, error });
    shared.set_operation_running(false);

    // Events that arrived after `operation_running` was cleared are handled
    // normally; the ones that arrived in the window above are not.
    let late = shared.take_pending();
    if late.refs_changed() {
        let generation = shared.begin_reload();
        let _ = load.send(LoadCommand { generation });
        let _ = status.send(RefreshStatus::default());
    } else if late.config || late.status_changed() {
        let _ = status.send(RefreshStatus {
            config: late.config,
            ..RefreshStatus::default()
        });
    }
}

/// git's stderr is the message a user expects from a failed command.
fn error_message(error: &Error) -> String {
    match error {
        Error::GitCommand { stderr, .. } if !stderr.is_empty() => stderr.clone(),
        other => other.to_string(),
    }
}

/// Operations that move refs, so history and ref badges are stale afterwards.
fn changes_refs(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::Commit { .. }
            | Operation::Checkout { .. }
            | Operation::CreateBranch { .. }
            | Operation::DeleteBranch { .. }
            | Operation::RenameBranch { .. }
            | Operation::SetBranchUpstream { .. }
            | Operation::UnsetBranchUpstream { .. }
            | Operation::AddRemote { .. }
            | Operation::RenameRemote { .. }
            | Operation::SetRemoteUrl { .. }
            | Operation::RemoveRemote { .. }
            | Operation::Fetch { .. }
            | Operation::FetchUnshallow
            | Operation::Pull { .. }
            | Operation::Push { .. }
            | Operation::StashPush { .. }
            | Operation::StashPushPaths { .. }
            | Operation::StashPop { .. }
            | Operation::StashApply { .. }
            | Operation::StashDrop { .. }
            | Operation::Merge { .. }
            | Operation::MergeAbort
            | Operation::MergeContinue
            | Operation::CherryPick { .. }
            | Operation::CherryPickMany { .. }
            | Operation::Revert { .. }
            | Operation::RebaseEdit { .. }
            | Operation::Rebase { .. }
            | Operation::Reset { .. }
            | Operation::UndoReflog { .. }
            | Operation::CreateTag { .. }
            | Operation::DeleteTag { .. }
            | Operation::ContinueInProgress
            | Operation::AbortInProgress
            // Only the new-branch form adds a ref; the others check one out or
            // add a detached worktree.
            | Operation::WorktreeAdd {
                new_branch: Some(_),
                ..
            }
    )
}

fn execute(shared: &Arc<Shared>, id: OperationId, operation: &Operation) -> Result<()> {
    // Credential prompts from git travel to the window and back through the
    // session. The handler blocks the command's thread until the user answers,
    // which is what git expects from an askpass program.
    let git = shared.git.clone().with_askpass({
        let shared = Arc::clone(shared);
        move |prompt| shared.request_answer(id, prompt)
    });
    let git = &git;
    match operation {
        Operation::StagePaths(paths) => git.stage_paths(paths),
        Operation::UnstagePaths(paths) => git.unstage_paths(paths),
        Operation::DiscardPaths(paths) => git.discard_paths(paths),
        Operation::DiscardStagedPaths { paths } => git.discard_staged_paths(paths),
        Operation::DeleteUntracked(paths) => git.delete_untracked(paths),
        Operation::StageLines { path, selection } => {
            apply_lines(shared, path, selection, PatchMode::Stage)
        }
        Operation::UnstageLines { path, selection } => {
            apply_lines(shared, path, selection, PatchMode::Unstage)
        }
        Operation::DiscardLines { path, selection } => {
            apply_lines(shared, path, selection, PatchMode::Discard)
        }
        Operation::Commit { message, amend } => git.commit(
            message,
            CommitOptions {
                amend: *amend,
                ..CommitOptions::default()
            },
        ),
        Operation::Checkout { branch } => git.checkout_branch(branch),
        Operation::CreateBranch {
            name,
            start_point,
            checkout,
        } => git.create_branch(name, start_point.as_deref(), *checkout),
        Operation::DeleteBranch { name, force } => git.delete_branch(name, *force),
        Operation::RenameBranch {
            name,
            new_name,
            rename_upstream,
        } => git.rename_branch(name, new_name, rename_upstream.as_ref()),
        Operation::SetBranchUpstream {
            branch,
            remote,
            remote_branch,
        } => git.set_branch_upstream(branch, remote, remote_branch),
        Operation::UnsetBranchUpstream { branch } => git.unset_branch_upstream(branch),
        Operation::AddRemote { name, url, fetch } => git.add_remote(name, url, *fetch),
        Operation::RenameRemote { name, new_name } => git.rename_remote(name, new_name),
        Operation::SetRemoteUrl {
            name,
            fetch_url,
            push_url,
        } => git.set_remote_url(name, fetch_url, push_url.as_deref()),
        Operation::RemoveRemote { name } => git.remove_remote(name),
        Operation::Fetch { remote, prune } => {
            let mut progress = ProgressReporter::new(shared, id);
            let result = git.fetch_with_progress(remote.as_deref(), *prune, |text, percent| {
                progress.line(text, percent)
            });
            progress.finish();
            result
        }
        Operation::FetchUnshallow => {
            let mut progress = ProgressReporter::new(shared, id);
            let result =
                git.fetch_unshallow_with_progress(|text, percent| progress.line(text, percent));
            progress.finish();
            result
        }
        Operation::Pull { rebase } => {
            let mut progress = ProgressReporter::new(shared, id);
            let result =
                git.pull_with_progress(*rebase, |text, percent| progress.line(text, percent));
            progress.finish();
            result
        }
        Operation::Push {
            remote,
            branch,
            set_upstream,
            force_with_lease,
        } => {
            let mut progress = ProgressReporter::new(shared, id);
            let result = git.push_with_progress(
                remote.as_deref(),
                branch.as_deref(),
                *set_upstream,
                *force_with_lease,
                |text, percent| progress.line(text, percent),
            );
            progress.finish();
            result
        }
        Operation::StashPush {
            message,
            include_untracked,
        } => git.stash_push(message.as_deref(), *include_untracked),
        Operation::StashPushPaths {
            message,
            include_untracked,
            paths,
        } => git.stash_push_paths(message.as_deref(), *include_untracked, paths),
        Operation::StashPop { index } => git.stash_pop(*index),
        Operation::StashApply { index } => git.stash_apply(*index),
        Operation::StashDrop { index } => git.stash_drop(*index),
        Operation::Merge { branch, no_ff } => git.merge(branch, *no_ff),
        Operation::MergeAbort => git.merge_abort(),
        Operation::ResolveConflict { path, side } => {
            git.checkout_side(path, matches!(side, ConflictSide::Ours))
        }
        Operation::MarkResolved(paths) => git.mark_resolved(paths),
        Operation::MergeContinue => git.merge_continue(),
        Operation::OpenMergetool { path } => git.mergetool(path),
        Operation::ResolveWithContent {
            path,
            content,
            expected,
        } => git.resolve_with_content(path, content, *expected),
        Operation::CherryPick { commit } => git.cherry_pick(*commit),
        Operation::CherryPickMany { commits } => {
            let mut progress = ProgressReporter::new(shared, id);
            let result = git.cherry_pick_many(commits);
            if let Ok(outcome) = &result
                && !outcome.skipped.is_empty()
            {
                let ids: Vec<String> = outcome
                    .skipped
                    .iter()
                    .map(|commit| {
                        let id = commit.to_string();
                        id[..7.min(id.len())].to_owned()
                    })
                    .collect();
                progress.line(
                    &format!("skipped {} (already applied)", ids.join(", ")),
                    None,
                );
            }
            progress.finish();
            result.map(|_| ())
        }
        Operation::Revert { commit } => git.revert(*commit),
        Operation::RebaseEdit { onto, steps } => git.rebase_edit(*onto, steps),
        Operation::Rebase {
            onto,
            upstream,
            autostash,
        } => git.rebase(*onto, *upstream, *autostash),
        Operation::Reset { target, mode } => git.reset(*target, *mode),
        Operation::UndoReflog { entry } => git.undo_reflog(entry.as_ref()).map(|_| ()),
        Operation::CreateTag {
            name,
            target,
            message,
        } => git.create_tag(name, *target, message.as_deref()),
        Operation::DeleteTag { name } => git.delete_tag(name),
        Operation::ContinueInProgress => git.continue_in_progress(),
        Operation::AbortInProgress => git.abort_in_progress(),
        Operation::SubmoduleUpdate { paths, init } => git.submodule_update(paths, *init),
        Operation::WorktreeAdd {
            path,
            branch,
            new_branch,
            commit,
        } => git.worktree_add(path, branch.as_deref(), new_branch.as_deref(), *commit),
        Operation::WorktreeRemove { path, force } => git.worktree_remove(path, *force),
        Operation::WorktreePrune => git.worktree_prune(),
        Operation::LfsTrack { path } => git.lfs_track(path),
        Operation::LfsLock { path } => git.lfs_lock(path),
        Operation::LfsUnlock { path } => git.lfs_unlock(path),
        Operation::LfsPull { path } => git.lfs_pull(path),
        Operation::WriteCommitGraph => {
            let mut progress = ProgressReporter::new(shared, id);
            let cancelled = || shared.is_shutdown();
            let result = git.write_commit_graph_with_progress(&cancelled, |text, percent| {
                progress.line(text, percent)
            });
            progress.finish();
            result.map(|_| ())
        }
    }
}

/// Recomputes the diff from the current state and applies the selected lines.
///
/// A `LineSelection` holds hunk and line indices, so it must be applied to a
/// freshly built diff instead of the view the user clicked: the index or the
/// worktree may have moved since then.
fn apply_lines(
    shared: &Shared,
    path: &str,
    selection: &LineSelection,
    mode: PatchMode,
) -> Result<()> {
    let repo = &shared.repo;
    let (old, new) = match mode {
        PatchMode::Stage | PatchMode::Discard => {
            (repo.file_in_index(path)?, worktree_as_git(repo, path)?)
        }
        PatchMode::Unstage => (head_file(repo, path)?, repo.file_in_index(path)?),
    };

    // Patch generation must keep the default options. The selection is built
    // from the view the user clicked, but `git apply` rewrites the real index
    // and worktree text, so the hunks have to describe every real line change:
    // whitespace-insensitive comparison would drop changes from the patch, and
    // zero context would produce a patch `git apply` refuses without
    // `--unidiff-zero`.
    let diff = diff_text(
        old.as_deref().unwrap_or_default(),
        new.as_deref().unwrap_or_default(),
        &DiffOptions::default(),
    );
    let file = PatchFile {
        path,
        old_path: old.is_some().then_some(path),
        diff: &diff,
        line_ending: line_ending(new.as_deref().or(old.as_deref()).unwrap_or_default()),
        is_deleted: new.is_none() && old.is_some(),
    };
    // Nothing selected, no changes left, or a binary file: no patch to apply.
    let Some(patch) = build_patch(&file, selection, mode) else {
        return Ok(());
    };
    shared.git.apply_patch(&patch, apply_mode(mode))
}

fn apply_mode(mode: PatchMode) -> ApplyMode {
    match mode {
        PatchMode::Stage => ApplyMode::Stage,
        PatchMode::Unstage => ApplyMode::Unstage,
        PatchMode::Discard => ApplyMode::Discard,
    }
}

/// The patch builder has to put terminators back, and it only knows the two
/// forms git writes; CRLF detection keeps a Windows checkout applicable.
fn line_ending(bytes: &[u8]) -> LineEnding {
    if bytes.windows(2).any(|pair| pair == b"\r\n") {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    }
}
