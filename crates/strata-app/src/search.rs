//! Commit search over the loaded history.
//!
//! The list is filtered by the session's query worker, never on the UI thread:
//! an edit starts a short debounce (one scan per typing pause instead of one
//! per keystroke) and the answer arrives as `SessionEvent::QueryReady` with row
//! indices into the snapshot generation the worker saw. A late answer for an
//! older generation is thrown away and the query is asked again, because a
//! reload can replace the commits those row indices point into.
//!
//! `--fake` has no session, so the search field is shown disabled rather than
//! filtering 200k synthetic rows on the draw path.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, KeyBinding, Subscription, Window, actions};
use gpui_component::input::{InputEvent, InputState};
use strata_core::session::{CommitQuery, QueryTicket};

use crate::actions::REPO_VIEW;
use crate::repo_view::RepoView;

/// Context of the search bar, so a focused text field can be recognised while
/// resolving the arrow keys.
pub(crate) const SEARCH_BAR: &str = "CommitSearch";

/// Quiet window between the last keystroke and the query.
const DEBOUNCE: Duration = Duration::from_millis(120);

actions!(
    strata_search,
    [
        FocusSearch,
        SearchNext,
        SearchPrevious,
        SearchSubmit,
        SearchDismiss
    ]
);

// `KeyBinding::new` parses without an OS, so the platform modifier is chosen at
// compile time instead of using `secondary-`.
#[cfg(target_os = "macos")]
const SEARCH_KEY: &str = "cmd-f";
#[cfg(not(target_os = "macos"))]
const SEARCH_KEY: &str = "ctrl-f";

/// Binds search. `actions::bind` runs first, so these registrations sit later
/// in the keymap and win the tie against the list's own `up`/`down` bindings:
/// while a filter is active the same keys walk the result set.
pub fn bind(cx: &mut App) {
    let list = Some(REPO_VIEW);
    cx.bind_keys([
        KeyBinding::new(SEARCH_KEY, FocusSearch, list),
        KeyBinding::new("escape", SearchDismiss, list),
        KeyBinding::new("down", SearchNext, list),
        KeyBinding::new("up", SearchPrevious, list),
        KeyBinding::new("enter", SearchSubmit, list),
    ]);
    // A focused input resolves its own `up`/`down` at a deeper context than
    // any wrapper. Matching at the same depth and being registered later makes
    // these run first; the input's single-line handlers are no-ops afterwards.
    let field = Some("CommitSearch > Input");
    cx.bind_keys([
        KeyBinding::new("down", SearchNext, field),
        KeyBinding::new("up", SearchPrevious, field),
    ]);
}

/// Which commit fields the search text is compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Summary,
    Author,
    Hash,
}

/// The three match toggles, all on by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fields {
    pub summary: bool,
    pub author: bool,
    pub hash: bool,
}

impl Default for Fields {
    fn default() -> Self {
        Self {
            summary: true,
            author: true,
            hash: true,
        }
    }
}

impl Fields {
    pub(crate) fn enabled(self, field: Field) -> bool {
        match field {
            Field::Summary => self.summary,
            Field::Author => self.author,
            Field::Hash => self.hash,
        }
    }

    pub(crate) fn toggle(&mut self, field: Field) {
        let slot = match field {
            Field::Summary => &mut self.summary,
            Field::Author => &mut self.author,
            Field::Hash => &mut self.hash,
        };
        *slot = !*slot;
    }

    /// The query the session worker should run. Trimming here keeps the
    /// debounce guard honest: trailing spaces are not a new search.
    pub(crate) fn query(self, text: &str) -> CommitQuery {
        CommitQuery {
            text: text.trim().to_owned(),
            in_summary: self.summary,
            in_author: self.author,
            in_hash: self.hash,
        }
    }
}

/// Epoch counter for debounced edits: only the timer holding the newest epoch
/// may start a query, so a fast typist runs one scan, not one per key.
#[derive(Debug, Default)]
pub(crate) struct Debounce {
    latest: u64,
}

impl Debounce {
    pub(crate) fn schedule(&mut self) -> u64 {
        self.latest += 1;
        self.latest
    }

    /// Drops every timer already scheduled.
    pub(crate) fn invalidate(&mut self) {
        self.latest += 1;
    }

    pub(crate) fn is_current(&self, epoch: u64) -> bool {
        epoch == self.latest
    }
}

/// What happened to a query answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ready {
    /// Rows were computed against the latest snapshot and can be shown.
    Applied,
    /// Not the ticket currently waited for; drop it.
    Ignored,
    /// The snapshot moved on while the query ran; ask again.
    Stale,
}

/// The one query a view has outstanding, plus the generation of its answer.
#[derive(Debug, Default)]
pub(crate) struct QuerySlot {
    ticket: Option<QueryTicket>,
    pending: bool,
    /// Snapshot generation of the rows currently shown.
    generation: u64,
}

impl QuerySlot {
    /// Makes `ticket` the one being waited for, returning the ticket it
    /// replaced so the caller can cancel it in the session.
    pub(crate) fn begin(&mut self, ticket: QueryTicket) -> Option<QueryTicket> {
        self.pending = true;
        self.ticket.replace(ticket)
    }

    pub(crate) fn accept(
        &mut self,
        ticket: QueryTicket,
        generation: u64,
        current_generation: u64,
    ) -> Ready {
        if self.ticket != Some(ticket) {
            return Ready::Ignored;
        }
        self.pending = false;
        self.ticket = None;
        if generation != current_generation {
            return Ready::Stale;
        }
        self.generation = generation;
        Ready::Applied
    }

    /// True when the failure belongs to the query being waited for.
    pub(crate) fn fail(&mut self, ticket: QueryTicket) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        self.ticket = None;
        self.pending = false;
        true
    }

    pub(crate) fn cancel(&mut self) -> Option<QueryTicket> {
        self.pending = false;
        self.ticket.take()
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending
    }

    /// True when shown rows were computed before `current_generation`, so a
    /// history chunk that has just arrived is not part of them yet.
    pub(crate) fn needs_refresh(&self, current_generation: u64) -> bool {
        !self.pending && self.generation < current_generation
    }
}

/// All search state. Rendering reads it; [`RepoView`] updates it from input
/// events and session answers.
#[derive(Default)]
pub(crate) struct SearchState {
    input: Option<Entity<InputState>>,
    _events: Option<Subscription>,
    /// Trimmed text the current results belong to; empty means no filter.
    text: String,
    fields: Fields,
    debounce: Debounce,
    slot: QuerySlot,
    /// Row indices into the snapshot's commits, in display order.
    filter: Option<Arc<Vec<usize>>>,
    error: Option<String>,
}

impl SearchState {
    /// Creates the text field once, the first time a repository view is built.
    pub(crate) fn init_input(&mut self, window: &mut Window, cx: &mut Context<RepoView>) {
        if self.input.is_some() {
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search commits, authors, hashes"));
        self._events = Some(
            cx.subscribe(&input, |view, _input, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    view.search_changed(cx);
                }
            }),
        );
        self.input = Some(input);
    }

    pub(crate) fn input(&self) -> Option<&Entity<InputState>> {
        self.input.as_ref()
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn fields(&self) -> Fields {
        self.fields
    }

    pub(crate) fn filter(&self) -> Option<&Arc<Vec<usize>>> {
        self.filter.as_ref()
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// True while the list is filtered by a non-empty query.
    pub(crate) fn is_active(&self) -> bool {
        !self.text.is_empty()
    }

    /// Trims the text, remembers it and returns the debounce epoch the caller
    /// must wait for. An empty box ends the filter without a round trip.
    pub(crate) fn edit(&mut self, text: &str) -> Option<u64> {
        self.text = text.trim().to_owned();
        self.error = None;
        if self.text.is_empty() {
            self.filter = None;
            self.debounce.invalidate();
            return None;
        }
        Some(self.debounce.schedule())
    }

    pub(crate) fn should_query(&self, epoch: u64) -> bool {
        self.is_active() && self.debounce.is_current(epoch)
    }

    pub(crate) fn begin(&mut self, ticket: QueryTicket) -> Option<QueryTicket> {
        self.debounce.invalidate();
        self.error = None;
        self.slot.begin(ticket)
    }

    pub(crate) fn apply(&mut self, rows: &Arc<Vec<usize>>) {
        self.filter = Some(Arc::clone(rows));
        self.error = None;
    }

    pub(crate) fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.filter = None;
    }

    /// Cancels the outstanding query and drops the filter. Returns the ticket
    /// the session should cancel.
    pub(crate) fn reset(&mut self) -> Option<QueryTicket> {
        self.text.clear();
        self.filter = None;
        self.error = None;
        self.debounce.invalidate();
        self.slot.cancel()
    }

    /// The commit to keep selected after `rows` arrived: the current one when
    /// it is still a match, otherwise the first match.
    pub(crate) fn selection(&self, rows: &[usize], current: Option<usize>) -> Option<usize> {
        let Some(current) = current else {
            return rows.first().copied();
        };
        if rows.binary_search(&current).is_ok() {
            Some(current)
        } else {
            rows.first().copied()
        }
    }

    /// Next/previous match from `selected`, or `None` when there is nothing to
    /// walk (no filter, or no matches).
    pub(crate) fn step(&self, selected: Option<usize>, delta: isize) -> Option<usize> {
        let filter = self.filter.as_ref()?;
        if filter.is_empty() {
            return None;
        }
        let position = selected
            .and_then(|index| filter.binary_search(&index).ok())
            .map_or(0, |position| {
                (position as isize + delta).clamp(0, filter.len() as isize - 1) as usize
            });
        filter.get(position).copied()
    }

    /// Visible position of a commit while filtered, for scrolling the
    /// virtualised list to it.
    pub(crate) fn position(&self, index: usize) -> Option<usize> {
        self.filter.as_ref()?.binary_search(&index).ok()
    }

    /// Short count label for the bar.
    pub(crate) fn count_label(&self, total: usize) -> String {
        if !self.is_active() {
            return String::new();
        }
        if self.slot.is_pending() {
            return "searching…".to_owned();
        }
        match &self.filter {
            Some(rows) => format!("{} of {total}", rows.len()),
            None => String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// RepoView glue.
// ---------------------------------------------------------------------------

impl RepoView {
    /// Reads the field, restarts the debounce and, when it is empty, drops the
    /// filter immediately so the full list is back without a round trip.
    pub(crate) fn search_changed(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.search.input().cloned() else {
            return;
        };
        let text = input.read(cx).value().to_string();
        let Some(epoch) = self.search.edit(&text) else {
            if let (Some(session), Some(ticket)) =
                (self.session.as_ref(), self.search.slot.cancel())
            {
                session.cancel_query(ticket);
            }
            cx.notify();
            return;
        };

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |this, cx| {
                if this.search.should_query(epoch) {
                    this.start_search(cx);
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Asks the session worker to rescan the loaded history with the current
    /// text and toggles.
    pub(crate) fn start_search(&mut self, cx: &mut Context<Self>) {
        if !self.search.is_active() {
            return;
        }
        let query = self.search.fields().query(self.search.text());
        if query.text.is_empty() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let ticket = session.search_commits(query);
        if let Some(previous) = self.search.begin(ticket) {
            session.cancel_query(previous);
        }
        cx.notify();
    }

    /// Applies a `QueryReady` answer that belongs to search; returns false when
    /// the ticket is somebody else's.
    pub(crate) fn search_ready(
        &mut self,
        ticket: QueryTicket,
        generation: u64,
        rows: &Arc<Vec<usize>>,
        cx: &mut Context<Self>,
    ) -> bool {
        let current = self
            .session
            .as_ref()
            .map_or(0, |session| session.snapshot().generation);
        match self.search.slot.accept(ticket, generation, current) {
            Ready::Ignored => false,
            Ready::Stale => {
                self.start_search(cx);
                true
            }
            Ready::Applied => {
                self.search.apply(rows);
                let index = self.search.selection(rows, self.selected);
                if let Some(index) = index {
                    // Keep the selected commit in view as the result set moves
                    // under it while history keeps loading.
                    if let Some(position) = self.search.position(index) {
                        self.scroll_to_position(position);
                    }
                    if Some(index) != self.selected {
                        self.select(Some(index), cx);
                    }
                }
                cx.notify();
                true
            }
        }
    }

    pub(crate) fn search_failed(
        &mut self,
        ticket: QueryTicket,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.search.slot.fail(ticket) {
            return false;
        }
        self.search.set_error(error.to_owned());
        cx.notify();
        true
    }

    /// Re-runs the search when the history grew: rows computed against an older
    /// generation leave newly loaded commits out of the results.
    pub(crate) fn search_on_snapshot(&mut self, cx: &mut Context<Self>) {
        if !self.search.is_active() {
            return;
        }
        let current = self
            .session
            .as_ref()
            .map_or(0, |session| session.snapshot().generation);
        if self.search.slot.needs_refresh(current) {
            self.start_search(cx);
        }
    }

    /// Focuses the search field (`ctrl-f`/`cmd-f`), leaving the file-history
    /// view if it is open.
    pub(crate) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        self.close_file_history(window, cx);
        let Some(input) = self.search.input().cloned() else {
            return;
        };
        crate::ui::focus_input(&input, window, cx);
        cx.notify();
    }

    /// Escapes the search: filter cleared, full list back, the selected commit
    /// kept and scrolled into view.
    pub(crate) fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.is_active() {
            // An empty field is still a place the keys can be stuck in.
            self.focus_root(window, cx);
            return;
        }
        let ticket = self.search.reset();
        if let (Some(session), Some(ticket)) = (self.session.as_ref(), ticket) {
            session.cancel_query(ticket);
        }
        if let Some(input) = self.search.input().cloned() {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        if let Some(index) = self.selected {
            self.scroll_to_position(index);
        }
        self.focus_root(window, cx);
        cx.notify();
    }

    /// Escape: leave the file-history view first, then clear search.
    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_file_history(window, cx) {
            return;
        }
        self.clear_search(window, cx);
    }

    /// Turns a search toggle on or off and re-runs the query immediately.
    pub(crate) fn toggle_search_field(&mut self, field: Field, cx: &mut Context<Self>) {
        self.search.fields.toggle(field);
        self.search.debounce.invalidate();
        if self.search.is_active() {
            self.start_search(cx);
        } else {
            cx.notify();
        }
    }

    /// `↓`/`↑`/`Enter` in a filtered list move between matches instead of
    /// between rows. Without a filter the normal list action runs.
    pub(crate) fn step_results(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.file_history.is_some() {
            self.step_file_history(delta, cx);
            return;
        }
        if self.search.is_active() {
            if let Some(index) = self.search.step(self.selected, delta) {
                if let Some(position) = self.search.position(index) {
                    self.scroll_to_position(position);
                }
                self.select(Some(index), cx);
            }
            return;
        }
        self.move_selection(delta, cx);
    }

    /// Enter only means something while results are being walked; in the plain
    /// history list it must not move the selection.
    pub(crate) fn submit_search(&mut self, cx: &mut Context<Self>) {
        if self.file_history.is_some() || self.search.is_active() {
            self.step_results(1, cx);
        }
    }

    /// A click on a filtered row: reveal by visible position, not commit index.
    pub(crate) fn select_result(&mut self, index: usize, position: usize, cx: &mut Context<Self>) {
        self.scroll_to_position(position);
        self.select(Some(index), cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(values: &[usize]) -> Arc<Vec<usize>> {
        Arc::new(values.to_vec())
    }

    #[test]
    fn the_query_carries_every_enabled_field() {
        let all = Fields::default();
        assert_eq!(
            all.query("  ada  "),
            CommitQuery {
                text: "ada".into(),
                in_summary: true,
                in_author: true,
                in_hash: true,
            }
        );

        let mut author_only = Fields {
            summary: false,
            author: true,
            hash: false,
        };
        assert_eq!(
            author_only.query("ada"),
            CommitQuery {
                text: "ada".into(),
                in_summary: false,
                in_author: true,
                in_hash: false,
            }
        );
        author_only.toggle(Field::Hash);
        assert!(author_only.enabled(Field::Hash));
        author_only.toggle(Field::Summary);
        assert!(author_only.enabled(Field::Summary));
    }

    #[test]
    fn only_the_latest_debounce_epoch_may_query() {
        let mut debounce = Debounce::default();
        let first = debounce.schedule();
        assert!(debounce.is_current(first));
        let second = debounce.schedule();
        assert!(!debounce.is_current(first), "the older timer must not run");
        assert!(debounce.is_current(second));

        debounce.invalidate();
        assert!(!debounce.is_current(second));
    }

    #[test]
    fn a_new_ticket_replaces_the_previous_one_for_cancellation() {
        let mut slot = QuerySlot::default();
        assert_eq!(slot.begin(QueryTicket(1)), None);
        assert_eq!(slot.begin(QueryTicket(2)), Some(QueryTicket(1)));
        assert!(slot.is_pending());

        assert_eq!(slot.accept(QueryTicket(1), 4, 4), Ready::Ignored);
        assert!(slot.is_pending(), "a stale ticket must not clear pending");

        assert_eq!(slot.accept(QueryTicket(2), 4, 4), Ready::Applied);
        assert!(!slot.is_pending());
    }

    #[test]
    fn an_answer_from_an_older_generation_asks_again() {
        let mut slot = QuerySlot::default();
        slot.begin(QueryTicket(3));
        assert_eq!(slot.accept(QueryTicket(3), 7, 8), Ready::Stale);
        assert!(!slot.is_pending());
        assert_eq!(slot.generation, 0, "stale rows are never recorded as shown");

        slot.begin(QueryTicket(4));
        assert_eq!(slot.accept(QueryTicket(4), 8, 8), Ready::Applied);
        assert_eq!(slot.generation, 8);
        assert!(!slot.needs_refresh(8));
        assert!(slot.needs_refresh(9), "a new chunk means a new scan");
    }

    #[test]
    fn a_pending_query_is_not_requeued_for_the_same_generation() {
        let mut slot = QuerySlot::default();
        slot.begin(QueryTicket(5));
        assert!(!slot.needs_refresh(99), "wait for the answer first");
        assert_eq!(slot.cancel(), Some(QueryTicket(5)));
        assert!(slot.needs_refresh(99));
    }

    #[test]
    fn a_failure_only_belongs_to_the_current_ticket() {
        let mut slot = QuerySlot::default();
        slot.begin(QueryTicket(9));
        assert!(!slot.fail(QueryTicket(8)));
        assert!(slot.is_pending());
        assert!(slot.fail(QueryTicket(9)));
        assert!(!slot.is_pending());
        assert!(!slot.fail(QueryTicket(9)));
    }

    #[test]
    fn editing_an_empty_box_ends_the_filter_without_a_query() {
        let mut state = SearchState::default();
        assert_eq!(state.edit("  "), None);
        assert!(!state.is_active());
        assert!(state.filter().is_none());

        assert!(state.edit("fix").is_some());
        assert!(state.is_active());
        assert_eq!(state.edit(""), None);
        assert!(!state.is_active(), "clearing drops the filter");
    }

    #[test]
    fn selection_follows_the_matches_and_otherwise_the_list() {
        let state = SearchState::default();
        let matches = [2, 5, 9];
        assert_eq!(state.selection(&matches, Some(5)), Some(5));
        assert_eq!(
            state.selection(&matches, Some(6)),
            Some(2),
            "a commit that no longer matches selects the first match"
        );
        assert_eq!(state.selection(&matches, None), Some(2));
        assert_eq!(state.selection(&[], Some(6)), None);
    }

    #[test]
    fn stepping_walks_only_matches_and_clamps() {
        let mut state = SearchState::default();
        assert_eq!(state.step(Some(2), 1), None, "no filter, no results");

        state.apply(&rows(&[2, 5, 9]));
        assert_eq!(state.step(Some(2), 1), Some(5));
        assert_eq!(state.step(Some(9), 1), Some(9), "clamped at the end");
        assert_eq!(state.step(Some(2), -1), Some(2));
        assert_eq!(state.step(Some(5), -1), Some(2));
        assert_eq!(
            state.step(Some(6), 1),
            Some(2),
            "an unlisted selection restarts at the first match"
        );

        state.apply(&rows(&[]));
        assert_eq!(state.step(Some(2), 1), None);
    }

    #[test]
    fn the_count_label_names_the_match_total() {
        let mut state = SearchState::default();
        assert_eq!(state.count_label(100), "");
        state.apply(&rows(&[1, 4]));
        state.text = "fix".into();
        assert_eq!(state.count_label(100), "2 of 100");
        state.begin(QueryTicket(1));
        assert_eq!(state.count_label(100), "searching…");
    }
}
