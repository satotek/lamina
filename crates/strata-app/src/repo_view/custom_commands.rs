//! The `RunCustomCommand` action for the repository view.
//!
//! The command itself lives in [`crate::commands`]; this module only reads the
//! current selection into the variables it expands and reports the outcome in
//! the status bar. The process runs in the repository's working tree, never on
//! the UI thread.

use std::path::PathBuf;

use gpui::{Context, Window};
use strata_core::HeadInfo;

use super::RepoView;
use crate::commands::{self, Finished, Variables};
use crate::ui;

impl RepoView {
    /// The variables a custom command can use, resolved from what is on screen.
    pub(crate) fn custom_variables(&self) -> Variables {
        let repo = self.work_dir();
        let snapshot = self.session.as_ref().map(|session| session.snapshot());
        let branch = match snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.head.as_ref())
        {
            Some(HeadInfo::Branch { short_name, target }) if target.is_some() => {
                Some(short_name.clone())
            }
            _ => None,
        };
        let commit = self
            .selected
            .and_then(|index| self.source().and_then(|source| source.commit_id(index)))
            .map(|oid| oid.to_string());
        let file = match self.mode {
            crate::worktree_panel::MainMode::Worktree => {
                self.worktree.selected_path().map(str::to_owned)
            }
            _ => self
                .files
                .as_ref()
                .and_then(|files| self.selected_file.and_then(|index| files.get(index)))
                .map(|file| file.path.clone()),
        };
        let remote = snapshot.as_ref().and_then(|snapshot| {
            let branch = branch.as_ref()?;
            if let Some(upstream) = snapshot.upstreams.get(branch) {
                return upstream
                    .name
                    .split_once('/')
                    .map(|(remote, _)| remote.to_owned());
            }
            match snapshot.remotes.as_slice() {
                [only] => Some(only.name.clone()),
                _ => None,
            }
        });
        Variables {
            repo: repo.map(|path| path.display().to_string()),
            branch,
            commit,
            file,
            remote,
        }
    }

    /// Runs the custom command at `index`, asking first when the file says so.
    pub(crate) fn run_custom_command(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_if_ssh(cx) {
            return;
        }
        let Some(command) = commands::CommandState::get(cx).commands.get(index).cloned() else {
            return;
        };
        let variables = self.custom_variables();
        let argv = match commands::expand(&command.spec.command, &variables) {
            Ok(argv) => argv,
            Err(reason) => {
                self.report_custom_command_error(
                    t!(
                        "commands.failed",
                        title = command.spec.title,
                        detail = reason
                    )
                    .to_string(),
                    cx,
                );
                return;
            }
        };
        let dir = match variables.repo.as_deref().map(PathBuf::from) {
            Some(dir) => dir,
            None => {
                self.report_custom_command_error(t!("commands.no_repository").to_string(), cx);
                return;
            }
        };
        let Some(session) = self.session.as_ref() else {
            self.report_custom_command_error(t!("commands.no_repository").to_string(), cx);
            return;
        };
        let log = session.git_extras().log_handle();
        let view = cx.entity().downgrade();
        let title = command.spec.title.clone();
        let detail = commands::display_line(&argv);
        let run = move |_window: &mut Window, cx: &mut gpui::App| {
            commands::execute(
                dir.clone(),
                argv.clone(),
                title.clone(),
                log.clone(),
                view.clone(),
                cx,
            );
        };
        if command.spec.confirm {
            ui::confirm(
                window,
                cx,
                ui::Confirm::new(
                    t!("commands.run_confirm", title = command.spec.title).to_string(),
                    detail,
                )
                .ok_label(t!("commands.run").to_string())
                .always_ask(),
                run,
            );
        } else {
            run(window, cx);
        }
    }

    fn report_custom_command_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.status_error = Some(message);
        cx.notify();
    }

    /// The end of one run: refresh the repository and say what happened.
    pub(crate) fn custom_command_finished(&mut self, finished: Finished, cx: &mut Context<Self>) {
        if let Some(session) = self.session.as_ref() {
            session.reload();
        }
        match finished.exit_code {
            Some(0) => {
                self.status_note =
                    Some(t!("commands.finished", title = finished.title).to_string());
                self.status_error = None;
            }
            _ => {
                self.status_error = Some(
                    t!(
                        "commands.failed",
                        title = finished.title,
                        detail = commands::failure_line(&finished)
                    )
                    .to_string(),
                );
            }
        }
        cx.notify();
    }
}
