//! The editable todo behind the "Edit history" dialog.
//!
//! Rows are kept in todo order, oldest commit first, because that is the order
//! `git rebase -i` and core's `RebaseEdit` take. The dialog shows them the same
//! way, so a row's position on screen is its position in the plan and "squash
//! into the row above" means exactly what git does.
//!
//! Nothing here touches GPUI or a repository: the view renders a
//! [`RebasePlan`] and calls its methods, and the unit tests below cover the
//! rules without a window.

use std::collections::HashSet;

use strata_core::Oid;
use strata_core::git_cli::{RebaseAction, RebaseStep};

use crate::data::short_oid;

/// What a row does with its commit, as the selector offers it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ActionKind {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
}

impl ActionKind {
    pub(super) const ALL: [Self; 5] = [
        Self::Pick,
        Self::Reword,
        Self::Squash,
        Self::Fixup,
        Self::Drop,
    ];

    /// The word git's todo uses, which is also the selector's label.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "reword",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
        }
    }

    /// The key that selects this action on the focused row.
    pub(super) fn shortcut(self) -> &'static str {
        match self {
            Self::Pick => "p",
            Self::Reword => "r",
            Self::Squash => "s",
            Self::Fixup => "f",
            Self::Drop => "d",
        }
    }

    pub(super) fn from_shortcut(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.shortcut() == key)
    }

    /// Whether the commit is folded into the one before it.
    fn combines(self) -> bool {
        matches!(self, Self::Squash | Self::Fixup)
    }
}

/// One commit of the range and what is going to happen to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PlanRow {
    pub(super) commit: Oid,
    pub(super) short: String,
    /// The summary the commit has now.
    pub(super) summary: String,
    pub(super) action: ActionKind,
    /// The replacement message; `Some` exactly when `action` is `Reword`, so a
    /// reword can never reach git without text.
    message: Option<String>,
}

impl PlanRow {
    /// The summary the row will have after the rewrite: a reworded commit's
    /// new first line, otherwise the current one.
    pub(super) fn display_summary(&self) -> &str {
        match &self.message {
            Some(message) => message.lines().next().unwrap_or("").trim(),
            None => &self.summary,
        }
    }

    /// The replacement message of a reworded row.
    pub(super) fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

/// Why the plan cannot run. Each one names what to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PlanProblem {
    /// The first commit that stays has nothing before it to combine into.
    OrphanCombine {
        /// 1-based position in the todo.
        position: usize,
        short: String,
        action: ActionKind,
    },
    /// Every commit is dropped; resetting the branch says that better.
    NothingRemains,
    /// The plan is the history as it already is.
    NoChanges,
}

impl PlanProblem {
    pub(super) fn reason(&self) -> String {
        match self {
            Self::OrphanCombine {
                position,
                short,
                action,
            } => format!(
                "Row {position} ({short}) is a {} with nothing above it to join. Make it pick or reword.",
                action.label()
            ),
            Self::NothingRemains => {
                "Every commit is dropped. Reset the branch instead to remove them all.".to_owned()
            }
            Self::NoChanges => {
                "Nothing to rewrite yet. Change an action or reorder a row.".to_owned()
            }
        }
    }

    /// Whether this is a mistake in the plan rather than an empty plan, so
    /// the dialog can show it as an error instead of a hint.
    pub(super) fn is_error(&self) -> bool {
        !matches!(self, Self::NoChanges)
    }
}

/// The counts the footer line reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PlanSummary {
    pub(super) before: usize,
    pub(super) after: usize,
    pub(super) dropped: usize,
    pub(super) squashed: usize,
    pub(super) fixed_up: usize,
    pub(super) reworded: usize,
    pub(super) reordered: bool,
}

impl PlanSummary {
    /// "5 commits → 3 commits, 1 dropped, 1 squashed": the size change first,
    /// then only the edits that happened, so an untouched plan reads plainly.
    pub(super) fn text(&self) -> String {
        let mut text = format!("{} → {}", count(self.before), count(self.after));
        let parts = [
            (self.dropped, "dropped"),
            (self.squashed, "squashed"),
            (self.fixed_up, "fixed up"),
            (self.reworded, "reworded"),
        ];
        for (number, word) in parts {
            if number > 0 {
                text.push_str(&format!(", {number} {word}"));
            }
        }
        if self.reordered {
            text.push_str(", reordered");
        }
        text
    }
}

fn count(number: usize) -> String {
    match number {
        1 => "1 commit".to_owned(),
        number => format!("{number} commits"),
    }
}

/// The rows of one edit session.
#[derive(Clone, Debug)]
pub(super) struct RebasePlan {
    rows: Vec<PlanRow>,
    /// The commits in their original order, to tell a reorder from no change.
    original: Vec<Oid>,
}

impl RebasePlan {
    /// A plan that picks every commit as it is. `commits` is `(id, summary)`,
    /// oldest first.
    pub(super) fn new(commits: Vec<(Oid, String)>) -> Self {
        let original = commits.iter().map(|(commit, _)| *commit).collect();
        let rows = commits
            .into_iter()
            .map(|(commit, summary)| PlanRow {
                commit,
                short: short_oid(commit),
                summary,
                action: ActionKind::Pick,
                message: None,
            })
            .collect();
        Self { rows, original }
    }

    pub(super) fn rows(&self) -> &[PlanRow] {
        &self.rows
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Swaps a row with the one above it. Returns the row's new index, or
    /// `None` when it is already first.
    pub(super) fn move_up(&mut self, index: usize) -> Option<usize> {
        let target = index.checked_sub(1)?;
        self.move_to(index, target)
    }

    /// Swaps a row with the one below it. Returns the row's new index, or
    /// `None` when it is already last.
    pub(super) fn move_down(&mut self, index: usize) -> Option<usize> {
        self.move_to(index, index + 1)
    }

    /// Takes the row at `from` out and puts it where the row at `to` is, so a
    /// row dropped on another row lands in that row's place. Returns the new
    /// index, or `None` when nothing moves.
    pub(super) fn move_to(&mut self, from: usize, to: usize) -> Option<usize> {
        if from == to || from >= self.rows.len() || to >= self.rows.len() {
            return None;
        }
        let row = self.rows.remove(from);
        self.rows.insert(to, row);
        Some(to)
    }

    /// Changes a row's action. A reword needs its text and goes through
    /// [`Self::reword`] instead, so this returns `false` for it.
    pub(super) fn set_action(&mut self, index: usize, action: ActionKind) -> bool {
        if action == ActionKind::Reword {
            return false;
        }
        let Some(row) = self.rows.get_mut(index) else {
            return false;
        };
        row.action = action;
        row.message = None;
        true
    }

    /// Marks a row as reworded with `message`. An empty message would make git
    /// refuse the amend, so it is refused here.
    pub(super) fn reword(&mut self, index: usize, message: String) -> bool {
        if message.trim().is_empty() {
            return false;
        }
        let Some(row) = self.rows.get_mut(index) else {
            return false;
        };
        row.action = ActionKind::Reword;
        row.message = Some(message);
        true
    }

    /// The index of the first row that stays, when it is a squash or fixup.
    /// Only that row can lack a commit to combine into: any later one has the
    /// kept row before it.
    pub(super) fn orphan_row(&self) -> Option<usize> {
        let first = self
            .rows
            .iter()
            .position(|row| row.action != ActionKind::Drop)?;
        self.rows[first].action.combines().then_some(first)
    }

    /// The first reason the plan cannot run, mistakes before the empty plan.
    pub(super) fn problem(&self) -> Option<PlanProblem> {
        if let Some(index) = self.orphan_row() {
            let row = &self.rows[index];
            return Some(PlanProblem::OrphanCombine {
                position: index + 1,
                short: row.short.clone(),
                action: row.action,
            });
        }
        if !self.rows.is_empty() && self.rows.iter().all(|row| row.action == ActionKind::Drop) {
            return Some(PlanProblem::NothingRemains);
        }
        if self.is_unchanged() {
            return Some(PlanProblem::NoChanges);
        }
        None
    }

    fn is_unchanged(&self) -> bool {
        self.rows.iter().all(|row| row.action == ActionKind::Pick) && !self.is_reordered()
    }

    fn is_reordered(&self) -> bool {
        self.rows
            .iter()
            .map(|row| row.commit)
            .ne(self.original.iter().copied())
    }

    /// The todo for core, in row order.
    ///
    /// Core demands each commit of the range exactly once; the rows are the
    /// range, only ever permuted, so that holds by construction. The check
    /// still runs because a wrong todo would silently drop a commit.
    pub(super) fn steps(&self) -> Result<Vec<RebaseStep>, PlanProblem> {
        if let Some(problem) = self.problem().filter(PlanProblem::is_error) {
            return Err(problem);
        }
        let mut seen = HashSet::new();
        let mut steps = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            if !seen.insert(row.commit) {
                continue;
            }
            let action = match (row.action, &row.message) {
                (ActionKind::Pick, _) => RebaseAction::Pick,
                (ActionKind::Reword, Some(message)) => RebaseAction::Reword {
                    message: message.clone(),
                },
                // A reword without text cannot be built through the setters.
                (ActionKind::Reword, None) => RebaseAction::Pick,
                (ActionKind::Squash, _) => RebaseAction::Squash,
                (ActionKind::Fixup, _) => RebaseAction::Fixup,
                (ActionKind::Drop, _) => RebaseAction::Drop,
            };
            steps.push(RebaseStep {
                commit: row.commit,
                action,
            });
        }
        Ok(steps)
    }

    pub(super) fn summary(&self) -> PlanSummary {
        let number =
            |action: ActionKind| self.rows.iter().filter(|row| row.action == action).count();
        let dropped = number(ActionKind::Drop);
        let squashed = number(ActionKind::Squash);
        let fixed_up = number(ActionKind::Fixup);
        PlanSummary {
            before: self.original.len(),
            // A squash or fixup disappears into the commit before it.
            after: self
                .rows
                .len()
                .saturating_sub(dropped + squashed + fixed_up),
            dropped,
            squashed,
            fixed_up,
            reworded: number(ActionKind::Reword),
            reordered: self.is_reordered(),
        }
    }
}

#[cfg(test)]
mod tests {
    use strata_core::git_cli::Git;

    use super::*;

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn plan(count: u8) -> RebasePlan {
        RebasePlan::new(
            (1..=count)
                .map(|byte| (oid(byte), format!("commit {byte}")))
                .collect(),
        )
    }

    fn order(plan: &RebasePlan) -> Vec<Oid> {
        plan.rows().iter().map(|row| row.commit).collect()
    }

    #[test]
    fn a_new_plan_picks_everything_in_order_and_asks_for_a_change() {
        let plan = plan(3);
        assert_eq!(order(&plan), vec![oid(1), oid(2), oid(3)]);
        assert!(plan.rows().iter().all(|row| row.action == ActionKind::Pick));
        assert_eq!(plan.problem(), Some(PlanProblem::NoChanges));
        assert!(!PlanProblem::NoChanges.is_error());
        assert_eq!(plan.summary().text(), "3 commits → 3 commits");
    }

    #[test]
    fn moving_one_step_swaps_neighbours_and_stops_at_the_ends() {
        let mut plan = plan(3);
        assert_eq!(plan.move_up(0), None);
        assert_eq!(plan.move_down(2), None);
        assert_eq!(plan.move_down(0), Some(1));
        assert_eq!(order(&plan), vec![oid(2), oid(1), oid(3)]);
        assert_eq!(plan.move_up(2), Some(1));
        assert_eq!(order(&plan), vec![oid(2), oid(3), oid(1)]);
    }

    #[test]
    fn a_row_dropped_on_another_takes_its_place() {
        let mut plan = plan(4);
        assert_eq!(plan.move_to(0, 2), Some(2));
        assert_eq!(order(&plan), vec![oid(2), oid(3), oid(1), oid(4)]);
        assert_eq!(plan.move_to(3, 0), Some(0));
        assert_eq!(order(&plan), vec![oid(4), oid(2), oid(3), oid(1)]);
        assert_eq!(
            plan.move_to(1, 1),
            None,
            "dropping on itself changes nothing"
        );
        assert_eq!(plan.move_to(0, 9), None);
        assert_eq!(plan.move_to(9, 0), None);
    }

    #[test]
    fn actions_travel_with_their_rows() {
        let mut plan = plan(3);
        plan.set_action(0, ActionKind::Drop);
        plan.move_down(0);
        assert_eq!(plan.rows()[1].commit, oid(1));
        assert_eq!(plan.rows()[1].action, ActionKind::Drop);
    }

    #[test]
    fn a_reword_needs_text_and_keeps_it_until_the_action_changes() {
        let mut plan = plan(2);
        assert!(!plan.set_action(0, ActionKind::Reword), "text comes first");
        assert!(!plan.reword(0, "  \n".to_owned()));
        assert_eq!(plan.rows()[0].action, ActionKind::Pick);

        assert!(plan.reword(0, "New title\n\nBody".to_owned()));
        assert_eq!(plan.rows()[0].display_summary(), "New title");
        assert_eq!(plan.rows()[0].message(), Some("New title\n\nBody"));
        assert_eq!(plan.rows()[1].display_summary(), "commit 2");

        assert!(plan.set_action(0, ActionKind::Pick));
        assert_eq!(plan.rows()[0].message(), None);
        assert_eq!(plan.rows()[0].display_summary(), "commit 1");
    }

    #[test]
    fn a_squash_or_fixup_on_the_first_remaining_row_is_invalid() {
        for action in [ActionKind::Squash, ActionKind::Fixup] {
            let mut plan = plan(3);
            plan.set_action(0, action);
            assert_eq!(plan.orphan_row(), Some(0));
            let problem = plan.problem().expect("an orphan combine is a problem");
            assert!(problem.is_error());
            assert!(problem.reason().starts_with("Row 1 ("));
            assert!(problem.reason().contains(action.label()));
            assert!(plan.steps().is_err());
        }
    }

    #[test]
    fn dropping_the_first_commit_moves_the_orphan_to_the_next_row() {
        let mut plan = plan(3);
        plan.set_action(1, ActionKind::Squash);
        assert_eq!(plan.orphan_row(), None);

        plan.set_action(0, ActionKind::Drop);
        assert_eq!(plan.orphan_row(), Some(1));
        match plan.problem() {
            Some(PlanProblem::OrphanCombine { position, .. }) => assert_eq!(position, 2),
            other => panic!("expected an orphan combine, got {other:?}"),
        }
    }

    #[test]
    fn reordering_can_create_and_cure_an_orphan() {
        let mut plan = plan(3);
        plan.set_action(1, ActionKind::Squash);
        assert_eq!(plan.problem(), None);
        plan.move_up(1);
        assert_eq!(plan.orphan_row(), Some(0));
        plan.move_down(0);
        assert_eq!(plan.orphan_row(), None);
    }

    #[test]
    fn a_squash_after_a_drop_still_has_a_commit_to_join() {
        let mut plan = plan(3);
        plan.set_action(1, ActionKind::Drop);
        plan.set_action(2, ActionKind::Squash);
        assert_eq!(plan.orphan_row(), None);
        assert_eq!(plan.problem(), None);
    }

    #[test]
    fn dropping_everything_is_refused() {
        let mut plan = plan(2);
        plan.set_action(0, ActionKind::Drop);
        plan.set_action(1, ActionKind::Drop);
        assert_eq!(plan.problem(), Some(PlanProblem::NothingRemains));
        assert!(plan.steps().is_err());
    }

    #[test]
    fn a_pure_reorder_is_a_change() {
        let mut plan = plan(2);
        plan.move_down(0);
        assert_eq!(plan.problem(), None);
        // Moving back restores the original and with it the "no changes" state.
        plan.move_up(1);
        assert_eq!(plan.problem(), Some(PlanProblem::NoChanges));
    }

    #[test]
    fn steps_follow_the_row_order_with_the_chosen_actions() {
        let mut plan = plan(6);
        plan.move_down(4); // 1 2 3 4 6 5
        plan.set_action(3, ActionKind::Squash); // 4 into 3
        plan.set_action(1, ActionKind::Drop);
        plan.reword(0, "Better".to_owned());
        plan.set_action(5, ActionKind::Fixup);
        let steps = plan.steps().expect("the plan is valid");
        assert_eq!(
            steps,
            vec![
                RebaseStep {
                    commit: oid(1),
                    action: RebaseAction::Reword {
                        message: "Better".to_owned()
                    }
                },
                RebaseStep {
                    commit: oid(2),
                    action: RebaseAction::Drop
                },
                RebaseStep {
                    commit: oid(3),
                    action: RebaseAction::Pick
                },
                RebaseStep {
                    commit: oid(4),
                    action: RebaseAction::Squash
                },
                RebaseStep {
                    commit: oid(6),
                    action: RebaseAction::Pick
                },
                RebaseStep {
                    commit: oid(5),
                    action: RebaseAction::Fixup
                },
            ]
        );
    }

    #[test]
    fn steps_name_every_commit_exactly_once() {
        let mut plan = plan(5);
        plan.move_to(0, 4);
        plan.move_to(2, 0);
        plan.set_action(1, ActionKind::Drop);
        let steps = plan.steps().expect("the plan is valid");
        let mut ids: Vec<_> = steps.iter().map(|step| step.commit).collect();
        ids.sort_by_key(|id| id.to_string());
        assert_eq!(ids, (1..=5).map(oid).collect::<Vec<_>>());
    }

    #[test]
    fn the_summary_counts_what_the_plan_does() {
        let mut plan = plan(5);
        plan.set_action(1, ActionKind::Squash);
        plan.set_action(3, ActionKind::Drop);
        let summary = plan.summary();
        assert_eq!(summary.before, 5);
        assert_eq!(summary.after, 3);
        assert_eq!(
            summary.text(),
            "5 commits → 3 commits, 1 dropped, 1 squashed"
        );

        plan.set_action(2, ActionKind::Fixup);
        plan.reword(0, "x".to_owned());
        plan.move_down(0);
        assert_eq!(
            plan.summary().text(),
            "5 commits → 2 commits, 1 dropped, 1 squashed, 1 fixed up, 1 reworded, reordered"
        );
    }

    #[test]
    fn the_summary_uses_the_singular_for_one_commit() {
        let mut plan = plan(2);
        plan.set_action(1, ActionKind::Squash);
        assert_eq!(plan.summary().text(), "2 commits → 1 commit, 1 squashed");
    }

    /// A throwaway repository with its own git config, so the result never
    /// depends on the machine running the tests.
    struct Scratch {
        dir: tempfile::TempDir,
        config: std::path::PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().expect("temporary directory");
            let config = dir.path().join("gitconfig");
            std::fs::write(
                &config,
                "[user]\n\tname = Strata Test\n\temail = strata@example.invalid\n\
                 [commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n",
            )
            .expect("write test git config");
            let scratch = Self { dir, config };
            std::fs::create_dir(scratch.work()).expect("work directory");
            scratch.git(&["init", "-q"]);
            scratch
        }

        fn work(&self) -> std::path::PathBuf {
            self.dir.path().join("work")
        }

        fn git(&self, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(self.work())
                .env("GIT_CONFIG_GLOBAL", &self.config)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn commit_file(&self, number: usize) -> Oid {
            std::fs::write(
                self.work().join(format!("f{number}.txt")),
                format!("{number}\n"),
            )
            .expect("write file");
            self.git(&["add", "-A"]);
            self.git(&[
                "commit",
                "-q",
                "-m",
                &format!("commit {number}: add f{number}"),
            ]);
            self.git(&["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("commit id")
        }

        fn core(&self) -> Git {
            Git::new(self.work())
                .with_env("GIT_CONFIG_GLOBAL", &self.config)
                .with_env("GIT_CONFIG_NOSYSTEM", "1")
        }

        fn subjects(&self) -> Vec<String> {
            self.git(&["log", "--format=%s"])
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    /// The whole path of the dialog without a window: rows from a real range,
    /// every kind of edit in one plan, the steps run by core, the log checked.
    #[test]
    fn a_plan_with_reorder_squash_drop_and_reword_rewrites_a_real_history() {
        let scratch = Scratch::new();
        let ids: Vec<Oid> = (1..=6).map(|number| scratch.commit_file(number)).collect();
        // One commit is already on a remote, as in the dialog's force-push case.
        scratch.git(&["init", "-q", "--bare", "../remote.git"]);
        scratch.git(&["remote", "add", "origin", "../remote.git"]);
        scratch.git(&["push", "-q", "-u", "origin", "main"]);
        eprintln!("before:\n{}", scratch.git(&["log", "--oneline"]));

        // "Edit history from here" on commit 2: the range starts after commit 1.
        let onto = ids[0];
        let range = scratch.git(&["rev-list", "--reverse", &format!("{onto}..HEAD")]);
        let commits: Vec<(Oid, String)> = range
            .lines()
            .map(|line| {
                let id: Oid = line.parse().expect("commit id");
                let number = ids.iter().position(|candidate| *candidate == id).unwrap() + 1;
                (id, format!("commit {number}: add f{number}"))
            })
            .collect();
        let mut plan = RebasePlan::new(commits);
        assert_eq!(plan.len(), 5);

        // Rows: 2 3 4 5 6.
        plan.move_up(4); // 2 3 4 6 5
        plan.move_to(1, 2); // 2 4 3 6 5
        plan.set_action(2, ActionKind::Squash); // 3 into 4
        plan.set_action(4, ActionKind::Drop); // 5
        assert!(plan.reword(
            0,
            "commit 2: add f2 (reworded)\n\nWhy it changed.".to_owned()
        ));
        assert_eq!(plan.problem(), None);
        assert_eq!(
            plan.summary().text(),
            "5 commits → 3 commits, 1 dropped, 1 squashed, 1 reworded, reordered"
        );

        let steps = plan.steps().expect("the plan is valid");
        scratch
            .core()
            .rebase_edit(onto, &steps)
            .expect("core runs the plan");
        eprintln!("after:\n{}", scratch.git(&["log", "--oneline"]));

        assert_eq!(
            scratch.subjects(),
            vec![
                "commit 6: add f6",
                "commit 4: add f4",
                "commit 2: add f2 (reworded)",
                "commit 1: add f1",
            ]
        );
        // The squash keeps both messages, and the drop removes only its own file.
        let squashed = scratch.git(&["log", "-1", "--format=%B", "HEAD~1"]);
        assert!(squashed.contains("commit 3: add f3"), "{squashed}");
        let reworded = scratch.git(&["log", "-1", "--format=%B", "HEAD~2"]);
        assert!(reworded.contains("Why it changed."), "{reworded}");
        let files = scratch.git(&["ls-tree", "--name-only", "HEAD"]);
        let files: Vec<&str> = files.lines().collect();
        assert_eq!(
            files,
            vec!["f1.txt", "f2.txt", "f3.txt", "f4.txt", "f6.txt"],
            "f5 is dropped and every other file survives"
        );
    }

    #[test]
    fn shortcuts_map_back_to_their_actions() {
        for action in ActionKind::ALL {
            assert_eq!(ActionKind::from_shortcut(action.shortcut()), Some(action));
        }
        assert_eq!(ActionKind::from_shortcut("x"), None);
    }
}
