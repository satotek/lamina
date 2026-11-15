//! Where Back and Forward return to, and the commit-graph moves that feed them.
//!
//! The history stores a [`Place`] per jump instead of a row index, because a
//! reload can move rows under the selection: the commit id is what the jump
//! meant, and the index is only the fallback for the synthetic benchmark,
//! whose rows have no ids.
//!
//! The graph moves are plain functions over the loaded rows so the commit
//! context menu can decide whether an entry is available without a session of
//! its own, and so the rules are testable on a small list.

use strata_core::Oid;
use strata_core::session::CommitList;

use gpui::{Context, Entity, Window};
use gpui_component::menu::{PopupMenu, PopupMenuItem};

use super::RepoView;
use super::history_ops::head_target;
use crate::worktree_panel::MainMode;

/// How many jumps the back stack keeps. A navigation history older than this
/// is no longer what the user is walking back through.
const LIMIT: usize = 50;

/// A place in the commit list: the commit when the source has ids, plus the row
/// it was shown at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) commit: Option<Oid>,
    pub(crate) index: Option<usize>,
}

/// The two stacks behind the back and forward buttons.
#[derive(Debug, Default)]
pub(crate) struct NavigationHistory {
    /// Places the user came from, newest last.
    back: Vec<Place>,
    /// Places the user went back from, newest last.
    forward: Vec<Place>,
}

impl NavigationHistory {
    pub(crate) fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub(crate) fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Records a jump away from `from`. The forward stack belongs to the path
    /// the user just left, so a new jump makes it stale.
    pub(crate) fn record(&mut self, from: Place) {
        if self.back.last() == Some(&from) {
            // The same place twice in a row cannot be what the user means: the
            // jump did not leave it.
            return;
        }
        self.back.push(from);
        if self.back.len() > LIMIT {
            self.back.remove(0);
        }
        self.forward.clear();
    }

    /// The place before `current`; `current` moves onto the forward stack.
    pub(crate) fn back(&mut self, current: Place) -> Option<Place> {
        let target = self.back.pop()?;
        self.forward.push(current);
        Some(target)
    }

    /// The place `current` was left for; `current` moves back onto the back
    /// stack.
    pub(crate) fn forward(&mut self, current: Place) -> Option<Place> {
        let target = self.forward.pop()?;
        self.back.push(current);
        Some(target)
    }

    #[cfg(test)]
    fn lengths(&self) -> (usize, usize) {
        (self.back.len(), self.forward.len())
    }
}

/// The commit-graph move a shortcut or a menu entry asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GraphMove {
    /// The first parent, which is the commit the branch continued from.
    Parent,
    /// The second parent of a merge, which is the branch that was merged in.
    SecondParent,
    /// The first child in display order, which is the newest commit that kept
    /// this one.
    Child,
    /// The commit HEAD points at.
    Head,
}

impl GraphMove {
    /// The moves the context menu lists, in reading order.
    pub(crate) const MENU: [GraphMove; 4] = [
        GraphMove::Parent,
        GraphMove::SecondParent,
        GraphMove::Child,
        GraphMove::Head,
    ];

    /// The label the menu shows for this move.
    pub(crate) fn menu_label(self) -> String {
        match self {
            GraphMove::Parent => t!("navigation.go_parent"),
            GraphMove::SecondParent => t!("navigation.go_second_parent"),
            GraphMove::Child => t!("navigation.go_child"),
            GraphMove::Head => t!("navigation.go_head"),
        }
        .to_string()
    }

    /// Why the move has no target. The reason is folded into the menu label,
    /// which is where a disabled gesture explains itself.
    fn blocked_reason(self) -> String {
        match self {
            GraphMove::Parent => t!("navigation.no_parent"),
            GraphMove::SecondParent => t!("navigation.no_second_parent"),
            GraphMove::Child => t!("navigation.no_child"),
            GraphMove::Head => t!("navigation.no_head"),
        }
        .to_string()
    }
}

/// The commit `movement` goes to, or `None` when the list has no such row.
///
/// `commit` is the selected row; `head` is where HEAD points, for
/// [`GraphMove::Head`]. The scan is linear because an action runs once, while
/// indexing the list would have to be rebuilt on every reload.
pub(crate) fn graph_target(
    commits: &CommitList,
    head: Option<Oid>,
    commit: Option<Oid>,
    movement: GraphMove,
) -> Option<Oid> {
    match movement {
        GraphMove::Head => head,
        GraphMove::Parent => parent(commits, commit?, 0),
        GraphMove::SecondParent => parent(commits, commit?, 1),
        GraphMove::Child => child(commits, commit?),
    }
}

/// The menu label for a move on `commit`, with the reason it cannot run.
pub(crate) fn menu_label(
    commits: &CommitList,
    head: Option<Oid>,
    commit: Option<Oid>,
    movement: GraphMove,
) -> String {
    let reason = graph_target(commits, head, commit, movement)
        .is_none()
        .then(|| movement.blocked_reason());
    crate::ui::menu_label(movement.menu_label(), reason.as_ref())
}

fn parents_of(commits: &CommitList, id: Oid) -> Option<Vec<Oid>> {
    (0..commits.len()).find_map(|index| {
        let (commit, _) = commits.get(index)?;
        (commit.id == id).then(|| commit.parents.clone())
    })
}

fn parent(commits: &CommitList, id: Oid, which: usize) -> Option<Oid> {
    parents_of(commits, id)?.get(which).copied()
}

/// The first commit in display order that has `id` as a parent.
fn child(commits: &CommitList, id: Oid) -> Option<Oid> {
    (0..commits.len()).find_map(|index| {
        let (commit, _) = commits.get(index)?;
        commit.parents.contains(&id).then_some(commit.id)
    })
}

impl RepoView {
    /// Where the selection is now, for the stack the jump is recorded in.
    fn current_place(&self) -> Place {
        Place {
            commit: self.selection_commit,
            index: self.selected,
        }
    }

    /// "Go back": the previous commit jump, or the parent repository when this
    /// view's own history is empty. The parent fallback keeps the old meaning
    /// of the shortcut in a repository opened from a parent.
    pub(crate) fn navigate_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_place();
        if let Some(place) = self.navigation.back(current) {
            self.reveal_place(place, window, cx);
            return;
        }
        self.navigate_back_to_parent(window, cx);
    }

    /// "Go forward": the commit jump this view last went back from.
    pub(crate) fn navigate_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_place();
        if let Some(place) = self.navigation.forward(current) {
            self.reveal_place(place, window, cx);
        }
    }

    /// One of the commit-graph moves on the selected commit.
    pub(crate) fn graph_move(
        &mut self,
        movement: GraphMove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self.session.as_ref().map(|session| session.snapshot()) else {
            return;
        };
        let head = head_target(snapshot.head.as_ref());
        let Some(target) = graph_target(&snapshot.commits, head, self.selection_commit, movement)
        else {
            return;
        };
        self.navigate_to_commit(target, window, cx);
    }

    /// Selects `target` and records the jump, so Back returns to where the
    /// user was.
    pub(crate) fn navigate_to_commit(
        &mut self,
        target: Oid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selection_commit == Some(target) {
            return;
        }
        let Some(index) = self
            .session
            .as_ref()
            .and_then(|session| session.snapshot().commit_index.get(&target).copied())
        else {
            // Not loaded yet, or outside the walked refs: there is no row to
            // land on, and a history entry that cannot be shown would be a
            // dead end.
            return;
        };
        let from = self.current_place();
        self.navigation.record(from);
        self.reveal_place(
            Place {
                commit: Some(target),
                index: Some(index),
            },
            window,
            cx,
        );
    }

    /// Shows a recorded place: the details pane and the list follow the commit
    /// id, so a reload that moved rows still lands on the same commit.
    fn reveal_place(&mut self, place: Place, window: &mut Window, cx: &mut Context<Self>) {
        let index = place
            .commit
            .and_then(|commit| {
                self.session
                    .as_ref()
                    .and_then(|session| session.snapshot().commit_index.get(&commit).copied())
            })
            .or(place.index)
            .filter(|index| *index < self.commit_count());
        let Some(index) = index else {
            return;
        };
        if self.mode != MainMode::History {
            self.close_worktree(window, cx);
        }
        // The jump addresses the full list, so the views that stand in front
        // of it give way, exactly as a sidebar reveal does.
        self.close_file_history(window, cx);
        if self.search.is_active() {
            self.clear_search(window, cx);
        }
        self.jump(index, cx);
    }

    /// Whether Back can do anything: a commit jump, or the parent repository.
    pub(crate) fn can_navigate_back(&self) -> bool {
        self.navigation.can_back() || self.parent_name.is_some()
    }

    /// Whether Forward has a place to return to.
    pub(crate) fn can_navigate_forward(&self) -> bool {
        self.navigation.can_forward()
    }

    /// The back button's tooltip: the shortcut, and the parent repository when
    /// that is what Back will do.
    pub(crate) fn back_tooltip(&self, window: &Window) -> String {
        let shortcut = self.action_shortcut(&crate::actions::NavigateBack, window);
        if !self.navigation.can_back()
            && let Some(parent) = &self.parent_name
        {
            return match shortcut {
                Some(key) => t!("title_bar.back_tooltip", parent = parent, key = key).to_string(),
                None => t!("title_bar.back", parent = parent).to_string(),
            };
        }
        match shortcut {
            Some(key) => t!("title_bar.nav_back_tooltip", shortcut = key).to_string(),
            None => t!("title_bar.nav_back").to_string(),
        }
    }

    /// The forward button's tooltip.
    pub(crate) fn forward_tooltip(&self, window: &Window) -> String {
        match self.action_shortcut(&crate::actions::NavigateForward, window) {
            Some(key) => t!("title_bar.nav_forward_tooltip", shortcut = key).to_string(),
            None => t!("title_bar.nav_forward").to_string(),
        }
    }

    /// The commit menu's "Go to" submenu, with one entry per graph move.
    pub(crate) fn graph_submenu(
        menu: PopupMenu,
        entity: Entity<RepoView>,
        commit: Oid,
        head: Option<Oid>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(snapshot) = entity
            .read(cx)
            .session
            .as_ref()
            .map(|session| session.snapshot())
        else {
            return menu;
        };
        let entries: Vec<(GraphMove, String, bool)> = GraphMove::MENU
            .iter()
            .map(|movement| {
                let available =
                    graph_target(&snapshot.commits, head, Some(commit), *movement).is_some();
                (
                    *movement,
                    menu_label(&snapshot.commits, head, Some(commit), *movement),
                    available,
                )
            })
            .collect();
        menu.submenu(
            t!("navigation.go_to").to_string(),
            window,
            cx,
            move |submenu, _, _| {
                let mut submenu = submenu;
                for &(movement, ref label, available) in &entries {
                    let entity = entity.clone();
                    submenu = submenu.item(
                        PopupMenuItem::new(label.clone())
                            .disabled(!available)
                            .on_click(move |_, window, cx| {
                                entity.update(cx, |view, cx| view.graph_move(movement, window, cx));
                            }),
                    );
                }
                submenu
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use strata_core::CommitInfo;
    use strata_core::graph::GraphRows;

    use super::*;

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn commit(index: u8, parents: &[u8]) -> CommitInfo {
        CommitInfo {
            id: oid(index),
            parents: parents.iter().map(|byte| oid(*byte)).collect(),
            summary: format!("commit {index}"),
            author_name: "Strata Test".into(),
            author_email: "test@example.invalid".into(),
            author_time: i64::from(index),
            committer_time: i64::from(index),
            shallow_boundary: false,
        }
    }

    /// Newest first, like the history list: a merge, its two parents and a
    /// child that kept the first parent's line.
    fn list() -> CommitList {
        let mut rows = GraphRows::new();
        for _ in 0..6 {
            rows.push_row(0, 0, 1, &[], &[]);
        }
        let mut commits = CommitList::default();
        commits.extend(
            vec![
                commit(5, &[4, 3]),
                commit(4, &[2]),
                commit(3, &[2]),
                commit(2, &[1]),
                commit(1, &[]),
                commit(0, &[5]),
            ],
            rows,
        );
        commits.complete = true;
        commits
    }

    fn place(index: usize) -> Place {
        Place {
            commit: Some(oid(index as u8)),
            index: Some(index),
        }
    }

    #[test]
    fn back_and_forward_walk_the_same_path_in_both_directions() {
        let mut history = NavigationHistory::default();
        history.record(place(0));
        history.record(place(1));
        history.record(place(2));

        assert_eq!(history.lengths(), (3, 0));
        assert_eq!(history.back(place(3)), Some(place(2)));
        assert_eq!(history.back(place(2)), Some(place(1)));
        assert_eq!(history.lengths(), (1, 2));

        assert_eq!(history.forward(place(1)), Some(place(2)));
        assert_eq!(history.forward(place(2)), Some(place(3)));
        assert_eq!(history.forward(place(3)), None);
        assert_eq!(history.lengths(), (3, 0));
    }

    #[test]
    fn a_new_jump_drops_the_forward_path() {
        let mut history = NavigationHistory::default();
        history.record(place(0));
        history.back(place(1));
        assert!(history.can_forward());

        history.record(place(3));
        assert!(!history.can_forward(), "a new jump forgets the old branch");
        assert_eq!(history.back(place(4)), Some(place(3)));
    }

    #[test]
    fn the_same_place_is_not_recorded_twice_in_a_row() {
        let mut history = NavigationHistory::default();
        history.record(place(1));
        history.record(place(1));
        assert_eq!(history.lengths(), (1, 0));
    }

    #[test]
    fn an_empty_history_offers_neither_direction() {
        let mut history = NavigationHistory::default();
        assert!(!history.can_back());
        assert!(!history.can_forward());
        assert_eq!(history.back(place(0)), None);
        assert_eq!(history.forward(place(0)), None);
    }

    #[test]
    fn parents_and_the_first_child_come_from_the_loaded_rows() {
        let commits = list();
        let merge = Some(oid(5));
        assert_eq!(
            graph_target(&commits, None, merge, GraphMove::Parent),
            Some(oid(4))
        );
        assert_eq!(
            graph_target(&commits, None, merge, GraphMove::SecondParent),
            Some(oid(3))
        );
        assert_eq!(
            graph_target(&commits, None, Some(oid(4)), GraphMove::Child),
            Some(oid(5)),
            "the newest child is the first row in display order"
        );
        assert_eq!(
            graph_target(&commits, Some(oid(4)), None, GraphMove::Head),
            Some(oid(4))
        );
    }

    #[test]
    fn a_root_commit_has_no_parent_and_a_tip_has_no_child() {
        let commits = list();
        assert_eq!(
            graph_target(&commits, None, Some(oid(1)), GraphMove::Parent),
            None
        );
        assert_eq!(
            graph_target(&commits, None, Some(oid(5)), GraphMove::Child),
            Some(oid(0)),
            "commit 0 kept the merge"
        );
        assert_eq!(
            graph_target(&commits, None, Some(oid(0)), GraphMove::Child),
            None
        );
        assert_eq!(
            graph_target(&commits, None, Some(oid(4)), GraphMove::SecondParent),
            None,
            "a non-merge has no second parent"
        );
    }

    #[test]
    fn the_menu_label_carries_the_reason_a_move_is_unavailable() {
        let commits = list();
        let leaf = Some(oid(1));
        let label = menu_label(&commits, None, leaf, GraphMove::Parent);
        assert!(
            label.starts_with(&t!("navigation.go_parent").to_string()),
            "{label}"
        );
        assert!(
            label.contains(&t!("navigation.no_parent").to_string()),
            "{label}"
        );

        let merge = Some(oid(5));
        let label = menu_label(&commits, None, merge, GraphMove::SecondParent);
        assert_eq!(label, t!("navigation.go_second_parent").to_string());
    }
}
