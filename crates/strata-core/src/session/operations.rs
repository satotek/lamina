//! The operation worker: git CLI mutations, strictly one at a time.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crate::Result;
use crate::diff::{DiffOptions, diff_text};
use crate::error::Error;
use crate::git_cli::{ApplyMode, CommitOptions};
use crate::patch::{LineEnding, LineSelection, PatchFile, PatchMode, build_patch};
use crate::session::history::LoadCommand;
use crate::session::publish::Shared;
use crate::session::refresh::RefreshStatus;
use crate::session::{Operation, OperationId, SessionEvent, head_file, worktree_as_git};

/// A wedged status refresh must not block the operation queue forever. The
/// limit is far above any real status scan.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

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
    shared: &Shared,
    job: OperationJob,
    status: &Sender<RefreshStatus>,
    load: &Sender<LoadCommand>,
) {
    shared.set_operation_running(true);
    shared.publish(|snapshot| snapshot.busy = Some(job.id));
    shared.emit(SessionEvent::OperationStarted { id: job.id });

    let result = execute(shared, &job.operation);

    // Refreshes held back during the command are dropped if the session
    // forgets them here: a status scan cannot see a ref that moved. Held
    // index/worktree changes are covered by the refresh requested below.
    let held = shared.take_pending();
    let refs_moved = held.refs_changed() || (result.is_ok() && changes_refs(&job.operation));
    if refs_moved {
        let generation = shared.begin_reload();
        let _ = load.send(LoadCommand { generation });
    }

    // Clearing `busy` before the refresh lands would flash the pre-operation
    // status, which is exactly what a commit must not do.
    let (ack_tx, ack_rx) = mpsc::channel();
    let _ = status.send(RefreshStatus { ack: Some(ack_tx) });
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
    } else if late.status_changed() {
        let _ = status.send(RefreshStatus::default());
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
            | Operation::Fetch { .. }
            | Operation::Pull { .. }
            | Operation::Push { .. }
            | Operation::StashPush { .. }
            | Operation::StashPop { .. }
            | Operation::StashApply { .. }
            | Operation::StashDrop { .. }
    )
}

fn execute(shared: &Shared, operation: &Operation) -> Result<()> {
    let git = &shared.git;
    match operation {
        Operation::StagePaths(paths) => git.stage_paths(paths),
        Operation::UnstagePaths(paths) => git.unstage_paths(paths),
        Operation::DiscardPaths(paths) => git.discard_paths(paths),
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
        Operation::Fetch { remote, prune } => git.fetch(remote.as_deref(), *prune),
        Operation::Pull { rebase } => git.pull(*rebase),
        Operation::Push {
            remote,
            branch,
            set_upstream,
            force_with_lease,
        } => git.push(
            remote.as_deref(),
            branch.as_deref(),
            *set_upstream,
            *force_with_lease,
        ),
        Operation::StashPush {
            message,
            include_untracked,
        } => git.stash_push(message.as_deref(), *include_untracked),
        Operation::StashPop { index } => git.stash_pop(*index),
        Operation::StashApply { index } => git.stash_apply(*index),
        Operation::StashDrop { index } => git.stash_drop(*index),
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
