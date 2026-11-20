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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Subscription, Window, actions};
use gpui_component::input::{InputEvent, InputState};
use lamina_core::Oid;
use lamina_core::repo::{ContentSearchEntry, ContentSearchMode, ContentSearchQuery};
use lamina_core::session::{CommitQuery, QueryTicket};

use crate::file_history::SnapshotHeads;
use crate::repo_view::RepoView;

/// Context of the search bar, so a focused text field can be recognised while
/// resolving the arrow keys.
pub(crate) const SEARCH_BAR: &str = "CommitSearch";

/// Quiet window between the last keystroke and the query.
const DEBOUNCE: Duration = Duration::from_millis(120);

actions!(
    lamina_search,
    [
        FocusSearch,
        SearchNext,
        SearchPrevious,
        SearchSubmit,
        SearchDismiss
    ]
);

/// Binds search. `actions::bind` runs first, so these registrations sit later
/// in the keymap and win the tie against the list's own `up`/`down` bindings:
/// while a filter is active the same keys walk the result set.
pub fn bind(cx: &mut App) {
    crate::keymap::bind_group(cx, crate::keymap::Group::Search);
}

/// Which kind of history search runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SearchMode {
    /// Filter commits by summary, author, or commit id.
    #[default]
    Fields,
    /// `git log -S`: commits changing the number of substring occurrences.
    Content,
    /// `git log -G`: commits whose diff adds or removes a regex-matching line.
    ContentRegex,
}

impl SearchMode {
    pub(crate) fn is_content(self) -> bool {
        matches!(self, Self::Content | Self::ContentRegex)
    }

    pub(crate) fn toggle(self, target: Self) -> Self {
        if self == target { Self::Fields } else { target }
    }

    pub(crate) fn content_query(self, text: &str, path: &str) -> Option<ContentSearchQuery> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        let path = path.trim();
        let path = (!path.is_empty()).then(|| path.to_owned());
        let mode = match self {
            Self::Fields => return None,
            Self::Content => ContentSearchMode::Substring(trimmed.to_owned()),
            Self::ContentRegex => ContentSearchMode::Regex(trimmed.to_owned()),
        };
        Some(ContentSearchQuery { mode, path })
    }
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

    pub(crate) fn matches(&self, ticket: QueryTicket) -> bool {
        self.ticket == Some(ticket)
    }

    pub(crate) fn finish(&mut self, ticket: QueryTicket) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        self.pending = false;
        self.ticket = None;
        true
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
    path_input: Option<Entity<InputState>>,
    _path_events: Option<Subscription>,
    /// Trimmed text the current results belong to; empty means no filter.
    text: String,
    /// Optional trimmed path limit for content search.
    path: String,
    mode: SearchMode,
    fields: Fields,
    debounce: Debounce,
    slot: QuerySlot,
    /// Row indices into the snapshot's commits, in display order.
    filter: Option<Arc<Vec<usize>>>,
    /// Matching commit ids streamed from the active content search.
    content_matches: Vec<Oid>,
    /// First matching path per commit id, for selecting the right file on open.
    matched_paths: HashMap<Oid, String>,
    content_scanned: usize,
    content_total: usize,
    content_heads: Option<SnapshotHeads>,
    error: Option<String>,
}

impl SearchState {
    /// Creates the text fields once, the first time a repository view is built.
    pub(crate) fn init_input(&mut self, window: &mut Window, cx: &mut Context<RepoView>) {
        if self.input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(t!("title_bar.search_commits").to_string())
            });
            self._events = Some(
                cx.subscribe(&input, |view, _input, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        view.search_changed(cx);
                    }
                }),
            );
            self.input = Some(input);
        }
        if self.path_input.is_none() {
            let path_input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(t!("search.path_filter").to_string())
            });
            self._path_events = Some(cx.subscribe(
                &path_input,
                |view, _input, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        view.search_path_changed(cx);
                    }
                },
            ));
            self.path_input = Some(path_input);
        }
    }

    pub(crate) fn input(&self) -> Option<&Entity<InputState>> {
        self.input.as_ref()
    }

    pub(crate) fn path_input(&self) -> Option<&Entity<InputState>> {
        self.path_input.as_ref()
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn mode(&self) -> SearchMode {
        self.mode
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

    /// True while a streaming content search is actively scanning commits.
    pub(crate) fn is_scanning(&self) -> bool {
        self.mode.is_content() && self.is_active() && self.slot.is_pending()
    }

    pub(crate) fn content_query(&self) -> Option<ContentSearchQuery> {
        self.mode.content_query(&self.text, &self.path)
    }

    pub(crate) fn matched_path_for(&self, commit: Oid) -> Option<&str> {
        if !self.mode.is_content() {
            return None;
        }
        self.matched_paths.get(&commit).map(String::as_str)
    }

    pub(crate) fn content_heads(&self) -> Option<&SnapshotHeads> {
        self.content_heads.as_ref()
    }

    /// Trims the text, remembers it and returns the debounce epoch the caller
    /// must wait for. An empty box ends the filter without a round trip.
    pub(crate) fn edit(&mut self, text: &str) -> Option<u64> {
        self.text = text.trim().to_owned();
        self.error = None;
        if self.text.is_empty() {
            self.filter = None;
            self.clear_content_results();
            self.debounce.invalidate();
            return None;
        }
        Some(self.debounce.schedule())
    }

    /// Trims the path filter, remembers it and returns a debounce epoch when a
    /// content search should re-run.
    pub(crate) fn edit_path(&mut self, path: &str) -> Option<u64> {
        let trimmed = path.trim();
        if trimmed == self.path {
            return None;
        }
        self.path = trimmed.to_owned();
        self.error = None;
        if self.mode.is_content() && self.is_active() {
            Some(self.debounce.schedule())
        } else {
            None
        }
    }

    pub(crate) fn toggle_mode(&mut self, target: SearchMode) -> Option<QueryTicket> {
        self.mode = self.mode.toggle(target);
        self.debounce.invalidate();
        self.clear_content_results();
        self.filter = None;
        self.error = None;
        self.slot.cancel()
    }

    pub(crate) fn toggle_field(&mut self, field: Field) -> Option<QueryTicket> {
        if self.mode.is_content() {
            self.mode = SearchMode::Fields;
            if !self.fields.enabled(field) {
                self.fields.toggle(field);
            }
            self.debounce.invalidate();
            self.clear_content_results();
            self.filter = None;
            self.error = None;
            return self.slot.cancel();
        }
        self.fields.toggle(field);
        self.debounce.invalidate();
        None
    }

    pub(crate) fn should_query(&self, epoch: u64) -> bool {
        self.is_active() && self.debounce.is_current(epoch)
    }

    pub(crate) fn begin(&mut self, ticket: QueryTicket) -> Option<QueryTicket> {
        self.debounce.invalidate();
        self.error = None;
        self.clear_content_results();
        self.slot.begin(ticket)
    }

    pub(crate) fn begin_content(
        &mut self,
        ticket: QueryTicket,
        heads: SnapshotHeads,
    ) -> Option<QueryTicket> {
        self.debounce.invalidate();
        self.error = None;
        self.clear_content_results();
        self.content_heads = Some(heads);
        self.filter = Some(Arc::new(Vec::new()));
        self.slot.begin(ticket)
    }

    pub(crate) fn accept_content_batch(
        &mut self,
        ticket: QueryTicket,
        matches: &[ContentSearchEntry],
        scanned: usize,
        total: usize,
        commit_index: &HashMap<Oid, usize>,
    ) -> bool {
        if !self.slot.matches(ticket) {
            return false;
        }
        for entry in matches {
            self.content_matches.push(entry.commit);
            if let Some(path) = &entry.matched_path {
                self.matched_paths
                    .entry(entry.commit)
                    .or_insert_with(|| path.clone());
            }
        }
        self.content_scanned = scanned;
        self.content_total = total;
        self.remap_content_matches(commit_index);
        true
    }

    pub(crate) fn accept_content_progress(
        &mut self,
        ticket: QueryTicket,
        scanned: usize,
        total: usize,
    ) -> bool {
        if !self.slot.matches(ticket) {
            return false;
        }
        self.content_scanned = scanned;
        self.content_total = total;
        true
    }

    pub(crate) fn accept_content_done(
        &mut self,
        ticket: QueryTicket,
        scanned: usize,
        total: usize,
    ) -> bool {
        if !self.slot.finish(ticket) {
            return false;
        }
        self.content_scanned = scanned;
        self.content_total = total;
        true
    }

    pub(crate) fn remap_content_matches(&mut self, commit_index: &HashMap<Oid, usize>) {
        let mut rows: Vec<usize> = self
            .content_matches
            .iter()
            .filter_map(|oid| commit_index.get(oid).copied())
            .collect();
        rows.sort_unstable();
        rows.dedup();
        self.filter = Some(Arc::new(rows));
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
        self.path.clear();
        self.mode = SearchMode::Fields;
        self.filter = None;
        self.error = None;
        self.clear_content_results();
        self.debounce.invalidate();
        self.slot.cancel()
    }

    fn clear_content_results(&mut self) {
        self.content_matches.clear();
        self.matched_paths.clear();
        self.content_scanned = 0;
        self.content_total = 0;
        self.content_heads = None;
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
        if self.mode.is_content() {
            let matches = self
                .filter
                .as_ref()
                .map_or(self.content_matches.len(), |rows| rows.len());
            if self.slot.is_pending() {
                if self.content_total == 0 {
                    return t!("search.scanning").to_string();
                }
                if matches == 0 {
                    return t!(
                        "search.scanned",
                        scanned = self.content_scanned,
                        total = self.content_total
                    )
                    .to_string();
                }
                return t!(
                    "search.matches_scanned_running",
                    matches = matches,
                    scanned = self.content_scanned,
                    total = self.content_total
                )
                .to_string();
            }
            if self.content_total > 0 {
                return t!(
                    "search.matches_scanned",
                    matches = matches,
                    total = self.content_total
                )
                .to_string();
            }
            return t!("search.matches_of", matches = matches, total = total).to_string();
        }
        if self.slot.is_pending() {
            return t!("search.searching").to_string();
        }
        match &self.filter {
            Some(rows) => {
                t!("search.count_of_total", count = rows.len(), total = total).to_string()
            }
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
        let was_active = self.search.is_active();
        let Some(epoch) = self.search.edit(&text) else {
            self.clear_search_origin();
            if let (Some(session), Some(ticket)) =
                (self.session.as_ref(), self.search.slot.cancel())
            {
                session.cancel_query(ticket);
            }
            cx.notify();
            return;
        };

        // The keystroke that starts a query marks the commit the search will
        // jump away from, before the filter can move the selection; later
        // edits of the same query keep that place.
        if !was_active {
            self.mark_search_origin();
        }
        self.schedule_search_debounce(epoch, cx);
        cx.notify();
    }

    /// Reads the optional path filter and restarts the debounce when content
    /// search is active.
    pub(crate) fn search_path_changed(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.search.path_input().cloned() else {
            return;
        };
        let path = input.read(cx).value().to_string();
        if let Some(epoch) = self.search.edit_path(&path) {
            self.schedule_search_debounce(epoch, cx);
            cx.notify();
        }
    }

    fn schedule_search_debounce(&self, epoch: u64, cx: &mut Context<Self>) {
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
    }

    /// Asks the session worker to rescan the loaded history with the current
    /// text and toggles.
    pub(crate) fn start_search(&mut self, cx: &mut Context<Self>) {
        if !self.search.is_active() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if self.search.mode().is_content() {
            let Some(query) = self.search.content_query() else {
                return;
            };
            let heads = SnapshotHeads::from_snapshot(&session.snapshot());
            let ticket = session.content_search_streaming(query);
            if let Some(previous) = self.search.begin_content(ticket, heads) {
                session.cancel_query(previous);
            }
            self.notify_if_active(cx);
            return;
        }
        let query = self.search.fields().query(self.search.text());
        if query.text.is_empty() {
            return;
        }
        let ticket = session.search_commits(query);
        if let Some(previous) = self.search.begin(ticket) {
            session.cancel_query(previous);
        }
        self.notify_if_active(cx);
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
                self.sync_filtered_selection(cx);
                self.notify_if_active(cx);
                true
            }
        }
    }

    pub(crate) fn content_search_batch(
        &mut self,
        ticket: QueryTicket,
        matches: &[ContentSearchEntry],
        scanned: usize,
        total: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        let snapshot = session.snapshot();
        if !self.search.accept_content_batch(
            ticket,
            matches,
            scanned,
            total,
            &snapshot.commit_index,
        ) {
            return false;
        }
        self.sync_filtered_selection(cx);
        self.notify_if_active(cx);
        true
    }

    pub(crate) fn content_search_progress(
        &mut self,
        ticket: QueryTicket,
        scanned: usize,
        total: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.search.accept_content_progress(ticket, scanned, total) {
            return false;
        }
        self.notify_if_active(cx);
        true
    }

    pub(crate) fn content_search_done(
        &mut self,
        ticket: QueryTicket,
        scanned: usize,
        total: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.search.accept_content_done(ticket, scanned, total) {
            return false;
        }
        self.notify_if_active(cx);
        true
    }

    fn sync_filtered_selection(&mut self, cx: &mut Context<Self>) {
        let Some(rows) = self.search.filter().cloned() else {
            return;
        };
        let index = self.search.selection(&rows, self.selected);
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
        self.notify_if_active(cx);
        true
    }

    /// Re-runs the search when the history grew: rows computed against an older
    /// generation leave newly loaded commits out of the results.
    pub(crate) fn search_on_snapshot(&mut self, cx: &mut Context<Self>) {
        if !self.search.is_active() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        if self.search.mode().is_content() {
            if self
                .search
                .content_heads()
                .is_some_and(|heads| heads.changed(&snapshot))
            {
                self.start_search(cx);
            } else {
                self.search.remap_content_matches(&snapshot.commit_index);
                self.sync_filtered_selection(cx);
                self.notify_if_active(cx);
            }
            return;
        }
        if self.search.slot.needs_refresh(snapshot.generation) {
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
        // A search filters the commit list, which the maximized details hide.
        if self.details_maximized(cx) {
            self.toggle_details_maximized(cx);
        }
        let Some(input) = self.search.input().cloned() else {
            return;
        };
        crate::ui::focus_input(&input, window, cx);
        cx.notify();
    }

    /// Escapes the search: filter cleared, full list back, the selected commit
    /// kept and scrolled into view.
    pub(crate) fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.is_active() && !self.search.mode().is_content() {
            // An empty field is still a place the keys can be stuck in.
            self.focus_root(window, cx);
            return;
        }
        let ticket = self.search.reset();
        self.clear_search_origin();
        if let (Some(session), Some(ticket)) = (self.session.as_ref(), ticket) {
            session.cancel_query(ticket);
        }
        if let Some(input) = self.search.input().cloned() {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        if let Some(path_input) = self.search.path_input().cloned() {
            path_input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        if let Some(index) = self.selected {
            self.scroll_to_position(self.visual_commit_position(index, cx));
        }
        self.focus_root(window, cx);
        cx.notify();
    }

    /// Escape: leave the file-history view first, then clear search.
    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_file_history(window, cx) {
            return;
        }
        // Escape peels one layer at a time: an active filter first, then the
        // compare view that sits beside the list.
        if !self.search.is_active() && !self.search.mode().is_content() && self.compare.is_some() {
            self.close_compare(window, cx);
            return;
        }
        self.clear_search(window, cx);
    }

    /// Switches between field search, `-S` content search, and `-G` regex search.
    pub(crate) fn toggle_search_mode(&mut self, mode: SearchMode, cx: &mut Context<Self>) {
        let previous = self.search.toggle_mode(mode);
        if let (Some(session), Some(ticket)) = (self.session.as_ref(), previous) {
            session.cancel_query(ticket);
        }
        if self.search.is_active() {
            self.start_search(cx);
        } else {
            cx.notify();
        }
    }

    /// Turns a search toggle on or off and re-runs the query immediately.
    pub(crate) fn toggle_search_field(&mut self, field: Field, cx: &mut Context<Self>) {
        let previous = self.search.toggle_field(field);
        if let (Some(session), Some(ticket)) = (self.session.as_ref(), previous) {
            session.cancel_query(ticket);
        }
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

    /// Enter walks search results while active, or opens the working-tree view
    /// when pressed on the synthetic uncommitted row in the plain history list.
    pub(crate) fn submit_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_history.is_some() {
            // The file history owns its own rows; Enter there is not a jump in
            // the commit list.
            self.step_results(1, cx);
        } else if self.search.is_active() {
            self.jump_to_result(1, cx);
        } else if self.mode == crate::worktree_panel::MainMode::History && self.uncommitted_selected
        {
            self.open_worktree(window, cx);
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

    #[test]
    fn mode_toggles_switch_between_fields_content_and_regex() {
        let mut state = SearchState::default();
        assert_eq!(state.mode(), SearchMode::Fields);

        // Switching to Content (-S) and ContentRegex (-G).
        state.toggle_mode(SearchMode::Content);
        assert_eq!(state.mode(), SearchMode::Content);
        state.toggle_mode(SearchMode::ContentRegex);
        assert_eq!(state.mode(), SearchMode::ContentRegex);
        // Clicking the active content mode toggles back to field search.
        state.toggle_mode(SearchMode::ContentRegex);
        assert_eq!(state.mode(), SearchMode::Fields);

        // Clicking a field chip while in Content mode returns to Fields mode
        // and guarantees that field is enabled, cancelling any in-flight scan.
        state.fields = Fields {
            summary: false,
            author: true,
            hash: false,
        };
        state.toggle_mode(SearchMode::Content);
        state.slot.begin(QueryTicket(11));
        assert_eq!(
            state.toggle_field(Field::Summary),
            Some(QueryTicket(11)),
            "switching back from content mode cancels the active content ticket"
        );
        assert_eq!(state.mode(), SearchMode::Fields);
        assert!(state.fields().enabled(Field::Summary));
    }

    #[test]
    fn content_query_builds_from_trimmed_text_and_optional_path() {
        assert_eq!(SearchMode::Fields.content_query("foo", "src"), None);
        assert_eq!(SearchMode::Content.content_query("   ", "src"), None);
        assert_eq!(
            SearchMode::Content.content_query("  needle  ", "   "),
            Some(ContentSearchQuery {
                mode: ContentSearchMode::Substring("needle".into()),
                path: None,
            })
        );
        assert_eq!(
            SearchMode::ContentRegex.content_query("fn\\s+main", " crates/lamina-core/src "),
            Some(ContentSearchQuery {
                mode: ContentSearchMode::Regex("fn\\s+main".into()),
                path: Some("crates/lamina-core/src".into()),
            })
        );
    }

    #[test]
    fn editing_path_debounces_only_when_content_search_is_active() {
        let mut state = SearchState::default();
        state.edit("needle");
        assert_eq!(
            state.edit_path("src/lib.rs"),
            None,
            "fields mode ignores path filter changes"
        );
        state.toggle_mode(SearchMode::Content);
        assert_eq!(state.path(), "src/lib.rs");
        assert_eq!(
            state.edit_path("  src/lib.rs "),
            None,
            "whitespace-only edits do not restart search"
        );
        assert!(
            state.edit_path("src/main.rs").is_some(),
            "path change restarts active content search"
        );
        assert_eq!(state.path(), "src/main.rs");
    }

    #[test]
    fn streaming_content_batches_map_to_sorted_rows_and_track_matched_paths() {
        let mut state = SearchState::default();
        state.edit("needle");
        state.toggle_mode(SearchMode::Content);
        state.slot.begin(QueryTicket(7));

        let oid_a = Oid::from_bytes_or_panic(&[1; 20]);
        let oid_b = Oid::from_bytes_or_panic(&[2; 20]);
        let oid_c = Oid::from_bytes_or_panic(&[3; 20]);
        let mut commit_index = HashMap::new();
        commit_index.insert(oid_a, 12);
        commit_index.insert(oid_b, 3);

        // Stale ticket is ignored.
        assert!(!state.accept_content_batch(
            QueryTicket(6),
            &[ContentSearchEntry {
                commit: oid_a,
                matched_path: Some("a.rs".into()),
            }],
            10,
            100,
            &commit_index,
        ));

        assert!(state.accept_content_progress(QueryTicket(7), 20, 100));
        assert_eq!(state.count_label(100), "20 of 100 commits scanned");

        assert!(state.accept_content_batch(
            QueryTicket(7),
            &[
                ContentSearchEntry {
                    commit: oid_a,
                    matched_path: Some("src/a.rs".into()),
                },
                ContentSearchEntry {
                    commit: oid_b,
                    matched_path: Some("src/b.rs".into()),
                },
                ContentSearchEntry {
                    commit: oid_c,
                    matched_path: Some("src/c.rs".into()),
                },
            ],
            50,
            100,
            &commit_index,
        ));

        // Mapped filter rows must be sorted for binary_search helpers.
        assert_eq!(
            state.filter().map(|rows| rows.as_slice()),
            Some(&[3, 12][..])
        );
        assert_eq!(state.matched_path_for(oid_a), Some("src/a.rs"));
        assert_eq!(state.matched_path_for(oid_b), Some("src/b.rs"));
        assert_eq!(state.count_label(100), "2 · 50 of 100 commits scanned");

        // Later history chunk reveals oid_c at row 7 without restarting search.
        commit_index.insert(oid_c, 7);
        state.remap_content_matches(&commit_index);
        assert_eq!(
            state.filter().map(|rows| rows.as_slice()),
            Some(&[3, 7, 12][..])
        );

        assert!(state.accept_content_done(QueryTicket(7), 100, 100));
        assert!(!state.is_scanning());
        assert_eq!(state.count_label(100), "3 of 100 commits scanned");
    }
}
