//! Writing a commit as a patch file and applying one from disk.
//!
//! `format-patch` and `apply` are git CLI commands, run through
//! [`strata_core::git_cli::Git`]; applying goes through [`Operation::ApplyPatch`]
//! so it is serialized with the other writes and refreshes the snapshot when it
//! finishes. The file pickers are GPUI's native dialogs.
//!
//! Neither action runs git on the UI thread: the save writes the file and reads
//! the patch on the background executor, and the apply hands bytes to the
//! session worker.

use std::path::PathBuf;

use gpui::{Context, PathPromptOptions, Window};
use strata_core::Oid;
use strata_core::session::Operation;

use super::RepoView;

impl RepoView {
    /// "Save as patch…": asks where, then writes `git format-patch -1 <id>`.
    pub(crate) fn save_commit_as_patch(
        &mut self,
        commit: Oid,
        short: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The helper produces the bytes; the file is saved here.
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let git = session.git_extras();
        // The suggested folder has to exist here. A remote root is a path on
        // the host, so the dialog starts from the home directory instead.
        let directory = if self.is_ssh() {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
        } else {
            self.work_dir().unwrap_or_else(|| PathBuf::from("."))
        };
        let suggested = format!("{short}.patch");
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    let message = t!("patch.choose_failed", error = error).to_string();
                    this.update(cx, |view, cx| view.report_patch_error(message, cx))
                        .ok();
                    return;
                }
            };
            let patch = cx
                .background_executor()
                .spawn(async move { git.format_patch(commit) })
                .await;
            let shown_path = path.display().to_string();
            let written = patch.and_then(|bytes| {
                std::fs::write(&path, bytes)
                    .map_err(|error| strata_core::Error::other(error.to_string()))
            });
            this.update(cx, |view, cx| {
                match written {
                    Ok(()) => {
                        view.status_note = Some(t!("patch.saved", path = shown_path).to_string());
                    }
                    Err(error) => {
                        view.status_error = Some(
                            t!(
                                "patch.save_failed",
                                path = shown_path,
                                error = super::one_line(&error.to_string())
                            )
                            .to_string(),
                        );
                    }
                }
                view.notify_if_active(cx);
            })
            .ok();
        })
        .detach();
    }

    /// "Apply patch…": chooses a `*.patch`/`*.diff` file and applies it.
    pub(crate) fn open_apply_patch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() || self.is_busy() {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("patch.choose").to_string().into()),
        });
        let handle = window.window_handle();
        let entity = cx.weak_entity();
        cx.spawn(async move |_this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    let _ = entity.update(cx, |view, cx| {
                        view.report_patch_error(
                            t!("patch.choose_failed", error = error).to_string(),
                            cx,
                        )
                    });
                    return;
                }
            };
            let Some(path) = path else {
                return;
            };
            let shown_path = path.display().to_string();
            let read = cx
                .background_executor()
                .spawn(async move { std::fs::read(&path) })
                .await;
            let patch = match read {
                Ok(bytes) => bytes,
                Err(error) => {
                    let message =
                        t!("patch.read_failed", path = shown_path, error = error).to_string();
                    let _ = entity.update(cx, |view, cx| view.report_patch_error(message, cx));
                    return;
                }
            };
            let _ = handle.update(cx, |_, window, cx| {
                let _ = entity.update(cx, |view, cx| view.start_apply_patch(patch, window, cx));
            });
        })
        .detach();
    }

    /// Hands the patch to the session worker.
    fn start_apply_patch(&mut self, patch: Vec<u8>, window: &mut Window, cx: &mut Context<Self>) {
        let label = t!("patch.op.apply").to_string();
        if let Some(id) = self.run_operation(Operation::ApplyPatch { patch }, label, window, cx) {
            self.pending_apply_patch = Some(id);
        }
    }

    /// Shows a picker or read failure in the status bar.
    fn report_patch_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.status_error = Some(message);
        self.notify_if_active(cx);
    }
}
