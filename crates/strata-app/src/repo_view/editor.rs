//! The keyboard and command-palette entry for the editor.
//!
//! The action carries no file of its own, so it opens whatever the view has
//! selected. Unlike a right-clicked row, that selection may name a file the
//! working tree does not have, or a file the commit on screen does not match;
//! this module checks both and reports the result before spawning anything.
//! The rule is the one the changed-file menu in [`super::details`] uses.

use gpui::Context;

use super::RepoView;
use super::details;
use crate::worktree_panel::MainMode;

/// How the selected file can be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// The diff on screen and the working tree agree, so nothing needs saying.
    WorkingFile,
    /// A commit other than HEAD is on screen. The editor opens the working
    /// copy, and the status bar says which copy that is.
    WorkingCopy,
    /// The file is not in the working tree, so there is nothing to open.
    Missing,
}

fn choice(in_working_tree: bool, at_head: bool) -> Choice {
    match (in_working_tree, at_head) {
        (false, _) => Choice::Missing,
        (true, true) => Choice::WorkingFile,
        (true, false) => Choice::WorkingCopy,
    }
}

impl RepoView {
    /// Opens the selection in the editor after checking it against the working
    /// tree. The title bar's opener (`open_selection_in_editor`) trusts its
    /// caller and stays the one place that spawns the editor.
    pub(crate) fn open_selected_file_in_editor(&mut self, cx: &mut Context<Self>) {
        // A previous run's note must not outlive the selection it described.
        self.status_note = None;
        let path = match self.mode {
            MainMode::Worktree => self.worktree.selected_path().map(str::to_owned),
            MainMode::History | MainMode::Reflog => self
                .files
                .as_ref()
                .and_then(|files| self.selected_file.and_then(|index| files.get(index)))
                .map(|file| file.path.clone()),
        };
        let Some(path) = path else {
            self.report_in_status(t!("title_bar.select_file_to_open").to_string(), cx);
            return;
        };

        let present = details::in_working_tree(self.work_dir().as_deref(), &path);
        match choice(present, self.selection_is_head()) {
            Choice::Missing => {
                self.report_in_status(t!("editor.not_in_worktree", path = path).to_string(), cx);
            }
            Choice::WorkingCopy => {
                self.status_note = Some(t!("editor.opened_working_copy", path = path).to_string());
                // The note is the newest thing the user asked for, so it must
                // not stay hidden behind an older failure.
                self.status_error = None;
                self.open_selection_in_editor(cx);
            }
            Choice::WorkingFile => self.open_selection_in_editor(cx),
        }
    }

    /// The same two lines as the title bar's error reporting, which is private
    /// to that module.
    fn report_in_status(&mut self, message: String, cx: &mut Context<Self>) {
        self.status_error = Some(message);
        cx.notify();
    }

    /// Whether the commit the details pane shows is the one HEAD points at.
    /// The worktree view always shows the working tree, so it always is.
    fn selection_is_head(&self) -> bool {
        if self.mode == MainMode::Worktree {
            return true;
        }
        let source = self.source();
        match (self.selection_commit, details::head_oid(source.as_ref())) {
            (Some(selected), Some(head)) => selected == head,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_missing_from_the_working_tree_is_never_opened() {
        assert_eq!(choice(false, true), Choice::Missing);
        assert_eq!(
            choice(false, false),
            Choice::Missing,
            "the reason is the missing file, not which commit is on screen"
        );
    }

    #[test]
    fn a_commit_away_from_head_opens_the_working_copy_and_says_so() {
        assert_eq!(choice(true, false), Choice::WorkingCopy);
        assert_eq!(choice(true, true), Choice::WorkingFile);
    }
}
