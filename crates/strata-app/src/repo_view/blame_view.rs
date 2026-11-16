//! Blame pane: every line of one file annotated with the commit that last
//! changed it.
//!
//! The core attributes the lines on the session's blame worker; this module
//! keeps the answer that still belongs to the target on screen. Formatting the
//! annotation rows and highlighting the file happen on the background executor
//! as well, so a file with tens of thousands of lines arrives as a visible
//! loading state instead of a frozen window.
//!
//! A blame request is keyed by ticket. Opening another target, closing the pane
//! or stepping to a commit's parent replaces the ticket and tells the session
//! to cancel the one it replaced, so a slow answer for a file the user has left
//! can never replace the view.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, FontWeight, IntoElement, KeyBinding,
    ParentElement, UniformListScrollHandle, Window, actions, div, px, uniform_list,
};
use gpui_component::menu::PopupMenuItem;
use gpui_component::tooltip::Tooltip;
use gpui_kit_assets::IconName;
use strata_core::Oid;
use strata_core::encoding::TextEncoding;
use strata_core::highlight::{Highlighter, LineHighlights, language_for_path};
use strata_core::session::{Blame, BlameCommit, BlameHunk, QueryTicket};

use super::RepoView;
use crate::data;
use crate::diff_view::{DiffColors, DiffStyle, styled_line};
use crate::scrollbar::WithScrollbar as _;
use crate::style::{self, Design};

/// Key context of the blame pane. Escape is an action here because macOS sends
/// it to the text input context first, where a raw key listener never sees it.
const BLAME_VIEW: &str = "BlameView";

actions!(strata_blame_view, [CloseBlameView]);

/// Width of the line-number gutter, matching the tree's file viewer.
const LINE_NUMBER_WIDTH: f32 = 52.0;
/// Width of the age bar between the line numbers and the annotation.
const BAR_WIDTH: f32 = 3.0;
/// Width of the commit annotation shown on a hunk's first line.
const ANNOTATION_WIDTH: f32 = 356.0;

/// What the pane is showing, so a closed pane draws nothing and a request in
/// flight draws a loading message or incremental lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum BlameState {
    /// Not open.
    #[default]
    Closed,
    /// The request is with the session; no file lines have arrived yet.
    Loading,
    /// File lines have arrived and hunks are filling in progressively.
    Streaming,
    /// The complete answer arrived without streaming; its rows are being
    /// formatted off the UI thread.
    Formatting,
    Ready,
    Failed,
}

/// The file and commit a request was made for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    commit: Oid,
    path: String,
}

/// What the attribution pane needs from one commit, prepared once per answer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitMeta {
    short: String,
    author: String,
    email: String,
    /// Author time in seconds since the Unix epoch, for the relative date.
    time: i64,
    /// Absolute author date, for the gutter's tooltip.
    absolute: String,
    summary: String,
}

/// One rendered line of the blamed file.
#[derive(Debug, Clone, PartialEq)]
struct BlameRow {
    /// 1-based line number in the blamed file.
    line: u32,
    text: String,
    highlights: LineHighlights,
    /// Set on the first line of a hunk: the commit whose annotation is drawn.
    gutter: Option<Oid>,
    /// Path the lines had in `commit`, used when blaming its parent.
    orig_path: String,
    /// How recent this line's commit is within the file, from 0 to 1.
    age: f32,
}

/// Per-line attribution slot while hunks arrive incrementally.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PartialLine {
    /// Set on the first line of an attributed hunk.
    gutter: Option<Oid>,
    /// Commit owning this line once attributed, for the age bar and count.
    commit: Option<Oid>,
    orig_path: String,
}

/// Incremental blame state while `git blame --incremental` is still running.
#[derive(Debug, Default)]
struct PartialBlame {
    lines: Arc<Vec<String>>,
    highlights: Arc<Vec<LineHighlights>>,
    slots: Vec<PartialLine>,
    commits: HashMap<Oid, CommitMeta>,
    time_range: Option<(i64, i64)>,
    attributed_lines: usize,
}

impl PartialBlame {
    fn new(lines: Arc<Vec<String>>) -> Self {
        let len = lines.len();
        Self {
            lines,
            highlights: Arc::new(Vec::new()),
            slots: vec![PartialLine::default(); len],
            commits: HashMap::new(),
            time_range: None,
            attributed_lines: 0,
        }
    }

    fn merge(&mut self, hunks: &[BlameHunk], commits: &HashMap<Oid, BlameCommit>) {
        for (oid, commit) in commits {
            self.time_range = Some(match self.time_range {
                Some((oldest, newest)) => (
                    oldest.min(commit.author_time),
                    newest.max(commit.author_time),
                ),
                None => (commit.author_time, commit.author_time),
            });
            self.commits.insert(*oid, commit_meta(*oid, commit));
        }

        for hunk in hunks {
            let Some(first_index) = hunk.start.checked_sub(1).map(|index| index as usize) else {
                continue;
            };
            for offset in 0..hunk.len as usize {
                let Some(slot) = self.slots.get_mut(first_index + offset) else {
                    break;
                };
                if slot.commit.is_none() {
                    self.attributed_lines += 1;
                }
                slot.commit = Some(hunk.commit);
                if offset == 0 {
                    slot.gutter = Some(hunk.commit);
                    slot.orig_path.clone_from(&hunk.orig_path);
                } else {
                    slot.gutter = None;
                    slot.orig_path.clear();
                }
            }
        }
    }

    fn percent(&self) -> u8 {
        attribution_percent(self.attributed_lines, self.lines.len())
    }
}

/// Borrowed view of one row for rendering both streaming and completed states.
struct RowView<'a> {
    line: u32,
    text: &'a str,
    highlights: Option<&'a LineHighlights>,
    gutter: Option<Oid>,
    meta: Option<&'a CommitMeta>,
    orig_path: &'a str,
    /// `None` when the line has not been attributed yet.
    age: Option<f32>,
}

/// The rows and commit details of one blame answer, built as a unit so a late
/// answer cannot mix its rows with another file's metadata.
#[derive(Debug, Default)]
struct BlameData {
    rows: Vec<BlameRow>,
    commits: HashMap<Oid, CommitMeta>,
    encoding: TextEncoding,
}

/// The blame pane's state: the target, the ticket that owns it and the rows
/// once they are ready.
pub(super) struct BlameView {
    target: Option<Target>,
    ticket: Option<QueryTicket>,
    state: BlameState,
    partial: Option<PartialBlame>,
    rows: Arc<Vec<BlameRow>>,
    commits: Arc<HashMap<Oid, CommitMeta>>,
    encoding: TextEncoding,
    error: Option<String>,
    scroll: UniformListScrollHandle,
    /// Shared with every row build so a language's query is compiled once.
    highlighter: Arc<Mutex<Highlighter>>,
}

impl Default for BlameView {
    fn default() -> Self {
        Self {
            target: None,
            ticket: None,
            state: BlameState::Closed,
            partial: None,
            rows: Arc::new(Vec::new()),
            commits: Arc::new(HashMap::new()),
            encoding: TextEncoding::UTF_8,
            error: None,
            scroll: UniformListScrollHandle::default(),
            highlighter: Arc::new(Mutex::new(Highlighter::new())),
        }
    }
}

impl BlameView {
    pub(super) fn is_open(&self) -> bool {
        self.target.is_some()
    }

    fn target(&self) -> Option<&Target> {
        self.target.as_ref()
    }

    fn path(&self) -> Option<&str> {
        self.target().map(|target| target.path.as_str())
    }

    /// Points the pane at `commit`/`path` and returns the ticket to cancel.
    fn begin(&mut self, commit: Oid, path: String, ticket: QueryTicket) -> Option<QueryTicket> {
        self.target = Some(Target { commit, path });
        self.state = BlameState::Loading;
        self.partial = None;
        self.rows = Arc::new(Vec::new());
        self.commits = Arc::new(HashMap::new());
        self.error = None;
        self.scroll = UniformListScrollHandle::default();
        self.ticket.replace(ticket)
    }

    /// Applies an incremental update for `ticket`. Returns `false` if `ticket`
    /// is stale.
    fn progress(
        &mut self,
        ticket: QueryTicket,
        lines: Option<Arc<Vec<String>>>,
        hunks: &[BlameHunk],
        commits: &HashMap<Oid, BlameCommit>,
    ) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        if let Some(lines) = lines {
            self.partial = Some(PartialBlame::new(lines));
            self.state = BlameState::Streaming;
        }
        if let Some(partial) = self.partial.as_mut() {
            partial.merge(hunks, commits);
        }
        true
    }

    fn set_partial_highlights(
        &mut self,
        ticket: QueryTicket,
        highlights: Vec<LineHighlights>,
    ) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        let Some(partial) = self.partial.as_mut() else {
            return false;
        };
        partial.highlights = Arc::new(highlights);
        true
    }

    fn cached_highlights(&self) -> Option<Arc<Vec<LineHighlights>>> {
        let partial = self.partial.as_ref()?;
        (partial.highlights.len() == partial.lines.len()).then(|| Arc::clone(&partial.highlights))
    }

    /// True when `ticket` is the request on screen; transitions a non-streaming
    /// request to formatting, while keeping an already-streaming list mounted so
    /// scroll stays stable.
    fn accept(&mut self, ticket: QueryTicket) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        if self.state != BlameState::Streaming {
            self.state = BlameState::Formatting;
        }
        true
    }

    /// Stores the formatted answer. A ticket replaced while it was building is
    /// dropped here.
    fn ready(&mut self, ticket: QueryTicket, data: BlameData) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        self.partial = None;
        self.rows = Arc::new(data.rows);
        self.commits = Arc::new(data.commits);
        self.encoding = data.encoding;
        self.state = BlameState::Ready;
        self.ticket = None;
        true
    }

    /// True when the failure belongs to the request on screen.
    fn fail(&mut self, ticket: QueryTicket, error: String) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        self.state = BlameState::Failed;
        self.partial = None;
        self.error = Some(error);
        self.rows = Arc::new(Vec::new());
        self.commits = Arc::new(HashMap::new());
        self.ticket = None;
        true
    }

    /// Closes the pane and returns the ticket the session should cancel.
    fn cancel(&mut self) -> Option<QueryTicket> {
        self.target = None;
        self.state = BlameState::Closed;
        self.partial = None;
        self.rows = Arc::new(Vec::new());
        self.commits = Arc::new(HashMap::new());
        self.error = None;
        self.ticket.take()
    }

    fn row_count(&self) -> usize {
        match self.state {
            BlameState::Streaming => self
                .partial
                .as_ref()
                .map_or(0, |partial| partial.lines.len()),
            BlameState::Ready => self.rows.len(),
            _ => 0,
        }
    }

    fn progress_percent(&self) -> Option<u8> {
        (self.state == BlameState::Streaming)
            .then(|| self.partial.as_ref().map(PartialBlame::percent))
            .flatten()
    }

    fn row_view(&self, index: usize) -> Option<RowView<'_>> {
        match self.state {
            BlameState::Streaming => {
                let partial = self.partial.as_ref()?;
                let text = partial.lines.get(index)?.as_str();
                let slot = partial.slots.get(index)?;
                let meta = slot.gutter.and_then(|commit| partial.commits.get(&commit));
                let age = slot.commit.map(|commit| {
                    let time = partial.commits.get(&commit).map_or(0, |entry| entry.time);
                    let (oldest, newest) = partial.time_range.unwrap_or((0, 0));
                    age_strength(time, oldest, newest)
                });
                Some(RowView {
                    line: index as u32 + 1,
                    text,
                    highlights: partial.highlights.get(index),
                    gutter: slot.gutter,
                    meta,
                    orig_path: slot.orig_path.as_str(),
                    age,
                })
            }
            BlameState::Ready => {
                let row = self.rows.get(index)?;
                let meta = row.gutter.and_then(|commit| self.commits.get(&commit));
                Some(RowView {
                    line: row.line,
                    text: row.text.as_str(),
                    highlights: Some(&row.highlights),
                    gutter: row.gutter,
                    meta,
                    orig_path: row.orig_path.as_str(),
                    age: Some(row.age),
                })
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers.
// ---------------------------------------------------------------------------

/// One line's place in the attribution, before its text is attached.
#[derive(Debug, Clone, PartialEq)]
struct RowPlan {
    line: u32,
    commit: Oid,
    /// True on the first line of a hunk, which carries the annotation.
    first: bool,
    orig_path: String,
    age: f32,
}

/// Percentage of lines attributed so far, floored to `0..=100` so 100% only
/// shows once every line has a commit.
fn attribution_percent(attributed_lines: usize, total_lines: usize) -> u8 {
    if total_lines == 0 {
        return 0;
    }
    ((attributed_lines.min(total_lines) as u64 * 100) / total_lines as u64) as u8
}

fn progress_label(percent: u8) -> String {
    t!("blame.attributing", percent = percent).to_string()
}

/// The oldest and newest author times among the commits the hunks reference.
fn commit_time_range(blame: &Blame) -> (i64, i64) {
    let mut range: Option<(i64, i64)> = None;
    for hunk in &blame.hunks {
        let Some(commit) = blame.commits.get(&hunk.commit) else {
            continue;
        };
        range = Some(match range {
            Some((oldest, newest)) => (
                oldest.min(commit.author_time),
                newest.max(commit.author_time),
            ),
            None => (commit.author_time, commit.author_time),
        });
    }
    range.unwrap_or((0, 0))
}

/// How strongly a commit's age bar is tinted within the file: the newest
/// commit is 1, the oldest 0, and a file with one commit (or several written at
/// the same second) stays at full strength.
fn age_strength(time: i64, oldest: i64, newest: i64) -> f32 {
    if newest <= oldest {
        return 1.0;
    }
    let span = (newest - oldest) as f32;
    (((time - oldest) as f32) / span).clamp(0.0, 1.0)
}

/// The opacity the age bar is painted with. The alpha keeps even the oldest
/// commit visible while the newest reads as a clear accent stripe.
fn bar_opacity(age: f32) -> f32 {
    0.16 + 0.6 * age.clamp(0.0, 1.0)
}

/// Walks the hunks in order and gives every line the commit that owns it, with
/// only a hunk's first line carrying an annotation.
fn row_plan(blame: &Blame) -> Vec<RowPlan> {
    let (oldest, newest) = commit_time_range(blame);
    let mut plan = Vec::with_capacity(blame.lines.len());
    for hunk in &blame.hunks {
        let time = blame
            .commits
            .get(&hunk.commit)
            .map_or(0, |commit| commit.author_time);
        let age = age_strength(time, oldest, newest);
        for offset in 0..hunk.len {
            plan.push(RowPlan {
                line: hunk.start + offset,
                commit: hunk.commit,
                first: offset == 0,
                orig_path: hunk.orig_path.clone(),
                age,
            });
        }
    }
    plan
}

fn commit_meta(oid: Oid, commit: &BlameCommit) -> CommitMeta {
    CommitMeta {
        short: data::short_oid(oid),
        author: commit.author_name.clone(),
        email: commit.author_email.clone(),
        time: commit.author_time,
        absolute: data::format_time(commit.author_time),
        summary: commit.summary.clone(),
    }
}

fn highlight_lines(
    path: &str,
    lines: &[String],
    highlighter: &Mutex<Highlighter>,
) -> Vec<LineHighlights> {
    let Some(language) = language_for_path(path) else {
        return Vec::new();
    };
    let source = lines.join("\n");
    let Ok(mut highlighter) = highlighter.lock() else {
        return Vec::new();
    };
    highlighter.highlight(language, &source)
}

/// Formats the attribution and highlights the file, off the UI thread.
fn build_data(
    path: &str,
    blame: &Blame,
    highlighter: &Mutex<Highlighter>,
    cached_highlights: Option<&[LineHighlights]>,
) -> BlameData {
    let owned_highlight;
    let highlight: &[LineHighlights] = match cached_highlights {
        Some(cached) if cached.len() == blame.lines.len() => cached,
        _ => {
            owned_highlight = highlight_lines(path, &blame.lines, highlighter);
            &owned_highlight
        }
    };

    let commits = blame
        .hunks
        .iter()
        .filter_map(|hunk| {
            blame
                .commits
                .get(&hunk.commit)
                .map(|commit| (hunk.commit, commit))
        })
        .map(|(oid, commit)| (oid, commit_meta(oid, commit)))
        .collect();

    let rows = row_plan(blame)
        .into_iter()
        .map(|plan| {
            let index = plan.line.saturating_sub(1) as usize;
            BlameRow {
                line: plan.line,
                text: blame.lines.get(index).cloned().unwrap_or_default(),
                highlights: highlight.get(index).cloned().unwrap_or_default(),
                gutter: plan.first.then_some(plan.commit),
                orig_path: plan.orig_path,
                age: plan.age,
            }
        })
        .collect();

    BlameData {
        rows,
        commits,
        encoding: blame.encoding,
    }
}

// ---------------------------------------------------------------------------
// RepoView glue.
// ---------------------------------------------------------------------------

impl RepoView {
    /// Opens the blame of `path` at `commit`, replacing any target on screen.
    pub(super) fn open_blame(
        &mut self,
        commit: Oid,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_blame_target(commit, path, cx);
        cx.bind_keys([KeyBinding::new("escape", CloseBlameView, Some(BLAME_VIEW))]);
        self.focus_root(window, cx);
    }

    fn open_blame_target(&mut self, commit: Oid, path: String, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let ticket = session.request_blame(commit, path.clone());
        if let Some(previous) = self.blame.begin(commit, path, ticket) {
            session.cancel_query(previous);
        }
        cx.notify();
    }

    pub(super) fn close_blame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.blame.is_open() {
            return;
        }
        self.discard_blame();
        self.focus_root(window, cx);
        cx.notify();
    }

    /// Closes the pane without moving focus, for another view taking its place.
    pub(crate) fn discard_blame(&mut self) {
        let ticket = self.blame.cancel();
        if let (Some(session), Some(ticket)) = (self.session.as_ref(), ticket) {
            session.cancel_query(ticket);
        }
    }

    /// Blames the selected entry of the open file history at its own commit.
    pub(crate) fn blame_file_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((commit, path)) = self
            .file_history
            .as_ref()
            .and_then(crate::file_history::FileHistory::selected_commit_and_path)
        else {
            return;
        };
        self.discard_file_history();
        self.open_blame(commit, path, window, cx);
    }

    /// Early file lines and incremental hunk batches for the target on screen.
    pub(super) fn blame_progress(
        &mut self,
        ticket: QueryTicket,
        lines: Option<Arc<Vec<String>>>,
        hunks: Vec<BlameHunk>,
        commits: HashMap<Oid, BlameCommit>,
        cx: &mut Context<Self>,
    ) {
        let highlight_lines_arg = lines.as_ref().map(Arc::clone);
        if !self.blame.progress(ticket, lines, &hunks, &commits) {
            return;
        }
        self.notify_if_active(cx);

        let Some(lines) = highlight_lines_arg else {
            return;
        };
        let Some(path) = self.blame.path().map(str::to_owned) else {
            return;
        };
        if language_for_path(&path).is_none() {
            return;
        }
        let highlighter = Arc::clone(&self.blame.highlighter);
        cx.spawn(async move |this, cx| {
            let highlights = cx
                .background_executor()
                .spawn(async move { highlight_lines(&path, &lines, &highlighter) })
                .await;
            this.update(cx, |this, cx| {
                if this.blame.set_partial_highlights(ticket, highlights) {
                    this.notify_if_active(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The session's attribution for the target on screen.
    pub(super) fn blame_ready(
        &mut self,
        ticket: QueryTicket,
        blame: &Arc<Blame>,
        cx: &mut Context<Self>,
    ) {
        if !self.blame.accept(ticket) {
            return;
        }
        self.notify_if_active(cx);
        let Some(path) = self.blame.path().map(str::to_owned) else {
            return;
        };
        let blame = Arc::clone(blame);
        let highlighter = Arc::clone(&self.blame.highlighter);
        let cached_highlights = self.blame.cached_highlights();
        cx.spawn(async move |this, cx| {
            let data = cx
                .background_executor()
                .spawn(async move {
                    build_data(
                        &path,
                        &blame,
                        &highlighter,
                        cached_highlights.as_deref().map(Vec::as_slice),
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                if this.blame.ready(ticket, data) {
                    this.notify_if_active(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// A failure for the ticket the blame pane is waiting for.
    pub(super) fn blame_failed(
        &mut self,
        ticket: QueryTicket,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.blame.fail(ticket, error.to_owned()) {
            return false;
        }
        self.notify_if_active(cx);
        true
    }

    /// Walks one step back: attributes the hunk's parent under the path the
    /// lines had there, so a change can be traced past the commit that made it.
    fn blame_parent(&mut self, commit: Oid, path: String, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let repo = session.reader();
        cx.spawn(async move |this, cx| {
            let parents = cx
                .background_executor()
                .spawn(async move { repo.parents(commit) })
                .await;
            this.update(cx, |this, cx| match parents {
                Ok(parents) => match parents.first().copied() {
                    Some(parent) => this.open_blame_target(parent, path, cx),
                    None => {
                        this.status_error = Some(t!("blame.no_parent").to_string().to_owned());
                        this.notify_if_active(cx);
                    }
                },
                Err(error) => {
                    this.status_error = Some(error.to_string());
                    this.notify_if_active(cx);
                }
            })
            .ok();
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

impl RepoView {
    /// The blame pane, filling the area the diff pane would use.
    pub(super) fn blame_pane(&self, design: &Design, cx: &mut App) -> AnyElement {
        let key_entity = self.entity();
        let action_entity = self.entity();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(design.background)
            .track_focus(&self.view_focus())
            .key_context(BLAME_VIEW)
            .on_action(move |_: &CloseBlameView, window, cx| {
                let entity = action_entity.clone();
                entity.update(cx, |this, cx| this.close_blame(window, cx));
            })
            .on_key_down(move |event, window, cx| {
                if event.keystroke.key == "escape" {
                    let entity = key_entity.clone();
                    entity.update(cx, |this, cx| this.close_blame(window, cx));
                }
            })
            .child(self.blame_header(design))
            .child(self.blame_body(design, cx))
            .into_any_element()
    }

    fn blame_header(&self, design: &Design) -> AnyElement {
        let entity = self.entity();
        let back = crate::ui::button("blame-back", t!("blame.back").to_string())
            .icon(IconName::ArrowLeft)
            .size(crate::ui::ButtonSize::Tiny)
            .ghost()
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.close_blame(window, cx));
            });

        let path = self.blame.path().unwrap_or_default().to_owned();
        let commit = self.blame.target().map(|target| target.commit);
        let progress = self.blame.progress_percent();
        div()
            .relative()
            .flex()
            .items_center()
            .gap_2()
            .h(design.px(40.0))
            .flex_none()
            .px_3p5()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .child(back)
            .child(
                div()
                    .flex_none()
                    .text_size(design.px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t!("blame.title").to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(design.mono.clone())
                    .text_size(design.px(11.5))
                    .text_color(design.accent)
                    .child(path),
            )
            .when_some(progress, |header, pct| {
                header.child(
                    div()
                        .flex_none()
                        .font_family(design.mono.clone())
                        .text_size(px(design.tiny()))
                        .text_color(design.muted)
                        .child(progress_label(pct)),
                )
            })
            .when_some(commit, |header, commit| {
                header.child(
                    div()
                        .flex_none()
                        .font_family(design.mono.clone())
                        .text_size(px(design.tiny()))
                        .text_color(design.muted)
                        .child(data::short_oid(commit)),
                )
            })
            .when_some(
                (!self.blame.encoding.is_utf8()).then(|| self.blame.encoding.label()),
                |header, label| {
                    header.child(crate::ui::status_badge(
                        label,
                        crate::ui::BadgeTone::Neutral,
                        design,
                    ))
                },
            )
            .child(crate::ui::kbd("Esc", design))
            .when_some(progress, |header, pct| {
                header.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .h(design.px(2.0))
                        .w(gpui::relative(f32::from(pct) / 100.0))
                        .bg(design.accent),
                )
            })
            .into_any_element()
    }

    fn blame_body(&self, design: &Design, cx: &mut App) -> AnyElement {
        match self.blame.state {
            BlameState::Failed => crate::ui::message(
                self.blame.error.clone().unwrap_or_default(),
                design.remove_text,
                design,
            )
            .into_any_element(),
            BlameState::Loading => crate::ui::message(
                t!("blame.attributing_lines").to_string(),
                design.muted,
                design,
            )
            .into_any_element(),
            BlameState::Formatting => crate::ui::message(
                t!("blame.preparing_lines").to_string(),
                design.muted,
                design,
            )
            .into_any_element(),
            BlameState::Streaming if self.blame.row_count() == 0 => {
                crate::ui::message(t!("blame.empty").to_string(), design.muted, design)
                    .into_any_element()
            }
            BlameState::Ready if self.blame.rows.is_empty() => {
                crate::ui::message(t!("blame.empty").to_string(), design.muted, design)
                    .into_any_element()
            }
            BlameState::Streaming | BlameState::Ready => self.blame_rows(design, cx),
            BlameState::Closed => div().into_any_element(),
        }
    }

    fn blame_rows(&self, design: &Design, cx: &mut App) -> AnyElement {
        let count = self.blame.row_count();
        let entity = self.entity();
        let colors = DiffColors::from_design(design);
        let style = DiffStyle::from_settings(cx, &design.mono);
        let design = design.clone();
        let target_path = self.blame.path().map(str::to_owned);
        let row_height = style.row_height();
        let bar_width = design.px(BAR_WIDTH);
        let annotation_width = design.px(ANNOTATION_WIDTH);
        let line_number_width = design.px(LINE_NUMBER_WIDTH);
        let gutter_radius = design.px(4.0);
        let short_w = design.px(52.0);
        let author_w = design.px(88.0);
        let date_w = design.px(72.0);
        uniform_list("blame-lines", count, move |range, _window, cx| {
            let view = entity.read(cx);
            range
                .map(|index| {
                    let Some(row) = view.blame.row_view(index) else {
                        return div().into_any_element();
                    };
                    let bar = match row.age {
                        Some(age) => div()
                            .flex_none()
                            .w(bar_width)
                            .h(px(row_height))
                            .bg(design.accent.opacity(bar_opacity(age))),
                        None => div()
                            .flex_none()
                            .w(bar_width)
                            .h(px(row_height))
                            .bg(design.border.opacity(0.25)),
                    };

                    let annotation: AnyElement = match (row.gutter, row.meta) {
                        (Some(commit), Some(meta)) => {
                            let date = style::relative_time(meta.time, style::now_epoch());
                            let tooltip =
                                format!("{}\n{}\n{}", meta.summary, meta.email, meta.absolute);
                            let short = meta.short.clone();
                            let author = meta.author.clone();
                            let summary = meta.summary.clone();
                            let orig_path = row.orig_path.to_owned();
                            let line = row.line;
                            let editor_path = target_path.clone();
                            let click_entity = entity.clone();
                            let menu_entity = entity.clone();
                            let copy_id = commit.to_string();
                            let hover = design.field;
                            let gutter = div()
                                .id(("blame-gutter", index))
                                .flex()
                                .items_center()
                                .gap_2()
                                .flex_none()
                                .w(annotation_width)
                                // The wrapper that carries the menu now sits
                                // between this row and the mono line, so the
                                // row states its own family.
                                .font_family(design.mono.clone())
                                // The menu wrapper around this row is sized by
                                // its content, so a percentage height here would
                                // have no definite parent to resolve against.
                                .h(px(row_height))
                                .px_1()
                                .pr_2()
                                .rounded(gutter_radius)
                                .overflow_hidden()
                                .cursor_pointer()
                                .hover(move |s| s.bg(hover))
                                .tooltip(move |_window, cx| {
                                    cx.new(|_| Tooltip::new(tooltip.clone())).into()
                                })
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    click_entity.update(cx, |this, cx| {
                                        this.reveal_commit(Some(commit), window, cx);
                                    });
                                })
                                .child(
                                    div()
                                        .flex_none()
                                        .w(short_w)
                                        .truncate()
                                        .font_family(design.mono.clone())
                                        .text_color(design.accent)
                                        .child(short),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .w(author_w)
                                        .truncate()
                                        .text_color(design.text)
                                        .child(author),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .w(date_w)
                                        .truncate()
                                        .text_color(design.muted)
                                        .child(date),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(design.muted)
                                        .child(summary),
                                );
                            crate::ui::context_menu(
                                ("blame-gutter-menu", index),
                                &design,
                                gutter,
                                move |menu, _window, cx| {
                                    let parent_entity = menu_entity.clone();
                                    let parent_path = orig_path.clone();
                                    let copy = copy_id.clone();
                                    let mut menu = menu;
                                    if let Some(path) = editor_path.clone() {
                                        let editor_entity = menu_entity.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(t!("history.open_in_editor").to_string()).on_click(
                                                move |_: &ClickEvent,
                                                      _window: &mut Window,
                                                      cx: &mut App| {
                                                    let path = path.clone();
                                                    editor_entity.update(cx, |this, cx| {
                                                        this.open_file_in_editor(
                                                            &path,
                                                            Some(line),
                                                            cx,
                                                        );
                                                    });
                                                },
                                            ),
                                        );
                                    }
                                    menu.item(
                                        PopupMenuItem::new(t!("blame.parent").to_string()).on_click(
                                            move |_: &ClickEvent,
                                                  _window: &mut Window,
                                                  cx: &mut App| {
                                                let path = parent_path.clone();
                                                parent_entity.update(cx, |this, cx| {
                                                    this.blame_parent(commit, path, cx);
                                                });
                                            },
                                        ),
                                    )
                                    .item(PopupMenuItem::new(t!("blame.copy_id").to_string()).on_click(
                                        move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                        },
                                    ))
                                    .when_some(
                                        crate::forge::file_item(
                                            &menu_entity,
                                            commit,
                                            &orig_path,
                                            Some((line, line)),
                                            cx,
                                        ),
                                        |menu, item| menu.item(item),
                                    )
                                },
                            )
                            .into_any_element()
                        }
                        _ => div()
                            .flex_none()
                            .w(annotation_width)
                            .h_full()
                            .into_any_element(),
                    };

                    div()
                        .flex()
                        .items_center()
                        .h(px(row_height))
                        .w_full()
                        .flex_none()
                        .font_family(design.mono.clone())
                        .text_size(px(design.tiny()))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .flex_none()
                                .w(line_number_width)
                                .pr_2()
                                .text_color(colors.muted)
                                .child(row.line.to_string()),
                        )
                        .child(bar)
                        .child(annotation)
                        .child(
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .pl_2()
                                .font_family(style.mono().clone())
                                .text_size(px(style.font_size()))
                                .text_color(colors.text)
                                .child(styled_line(row.text, &style, row.highlights, &colors)),
                        )
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.blame.scroll)
        .with_scrollbar(&self.blame.scroll)
        .flex_1()
        .min_h_0()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::session::BlameHunk;

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn commit(time: i64) -> BlameCommit {
        BlameCommit {
            author_name: "Mio Tanaka".to_owned(),
            author_email: "mio@example.invalid".to_owned(),
            author_time: time,
            summary: "a change".to_owned(),
        }
    }

    fn blame(hunks: Vec<BlameHunk>, lines: &[&str]) -> Blame {
        let mut commits = HashMap::new();
        for (index, hunk) in hunks.iter().enumerate() {
            commits.insert(hunk.commit, commit(1_000 + index as i64 * 1_000));
        }
        Blame {
            hunks,
            commits,
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
            encoding: strata_core::encoding::TextEncoding::UTF_8,
        }
    }

    fn hunk(commit: Oid, start: u32, len: u32) -> BlameHunk {
        BlameHunk {
            commit,
            orig_path: "src/main.rs".to_owned(),
            orig_start: start,
            start,
            len,
        }
    }

    #[test]
    fn only_the_first_line_of_a_hunk_carries_an_annotation() {
        let first = oid(1);
        let second = oid(2);
        let blame = blame(
            vec![hunk(first, 1, 2), hunk(second, 3, 1)],
            &["one", "two", "three"],
        );
        let plan = row_plan(&blame);
        assert_eq!(
            plan.iter().map(|row| row.line).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            plan.iter().map(|row| row.commit).collect::<Vec<_>>(),
            [first, first, second]
        );
        assert_eq!(
            plan.iter().map(|row| row.first).collect::<Vec<_>>(),
            [true, false, true],
            "the gutter is drawn once per hunk"
        );
        assert!(
            plan.iter().all(|row| row.orig_path == "src/main.rs"),
            "every line knows the path it had in its commit"
        );
    }

    #[test]
    fn age_bars_run_from_the_oldest_to_the_newest_commit() {
        assert_eq!(age_strength(2_000, 1_000, 2_000), 1.0);
        assert_eq!(age_strength(1_000, 1_000, 2_000), 0.0);
        assert_eq!(age_strength(1_500, 1_000, 2_000), 0.5);
        assert_eq!(
            age_strength(2_000, 2_000, 2_000),
            1.0,
            "a file with one commit date stays at full strength"
        );
        assert!(bar_opacity(1.0) > bar_opacity(0.5));
        assert!(bar_opacity(0.0) > 0.0, "the oldest commit stays visible");
    }

    #[test]
    fn the_plan_ages_each_hunk_against_the_whole_file() {
        let oldest = oid(1);
        let newest = oid(2);
        let mut blame = blame(
            vec![hunk(oldest, 1, 1), hunk(newest, 2, 1)],
            &["old", "new"],
        );
        blame.commits.get_mut(&oldest).expect("oldest").author_time = 1_000;
        blame.commits.get_mut(&newest).expect("newest").author_time = 3_000;

        let plan = row_plan(&blame);
        assert_eq!(plan[0].age, 0.0);
        assert_eq!(plan[1].age, 1.0);
    }

    #[test]
    fn a_late_answer_for_a_replaced_target_is_dropped() {
        let mut view = BlameView::default();
        assert!(!view.is_open());
        assert!(!view.accept(QueryTicket(1)));

        assert_eq!(view.begin(oid(1), "a.rs".to_owned(), QueryTicket(1)), None);
        assert!(view.is_open());
        assert!(view.accept(QueryTicket(1)));
        assert!(!view.accept(QueryTicket(2)));

        assert_eq!(
            view.begin(oid(2), "b.rs".to_owned(), QueryTicket(2)),
            Some(QueryTicket(1)),
            "the replaced request is handed back for cancellation"
        );
        assert!(!view.ready(QueryTicket(1), BlameData::default()));
        assert!(view.ready(QueryTicket(2), BlameData::default()));
        assert_eq!(view.state, BlameState::Ready);
        assert!(!view.fail(QueryTicket(2), "late".to_owned()));
    }

    #[test]
    fn closing_the_pane_cancels_the_request_and_rejects_its_answer() {
        let mut view = BlameView::default();
        view.begin(oid(1), "a.rs".to_owned(), QueryTicket(7));
        assert_eq!(view.cancel(), Some(QueryTicket(7)));
        assert!(!view.is_open());
        assert!(!view.accept(QueryTicket(7)));
        assert!(!view.ready(QueryTicket(7), BlameData::default()));
        assert_eq!(view.cancel(), None);
    }

    #[test]
    fn attribution_percentage_floors_until_all_lines_are_attributed() {
        assert_eq!(attribution_percent(0, 0), 0);
        assert_eq!(attribution_percent(0, 100), 0);
        assert_eq!(attribution_percent(42, 100), 42);
        assert_eq!(attribution_percent(999, 1000), 99);
        assert_eq!(attribution_percent(1000, 1000), 100);
        assert_eq!(attribution_percent(1200, 1000), 100);
        assert_eq!(progress_label(42), "attributing… 42%");
    }

    #[test]
    fn partial_hunks_merge_incrementally_and_ready_replaces_them() {
        let first = oid(1);
        let second = oid(2);
        let mut view = BlameView::default();
        let ticket = QueryTicket(10);
        view.begin(oid(9), "src/main.rs".to_owned(), ticket);

        let lines = Arc::new(vec![
            "one".to_owned(),
            "two".to_owned(),
            "three".to_owned(),
            "four".to_owned(),
        ]);
        assert!(view.progress(ticket, Some(lines), &[], &HashMap::new()));
        assert_eq!(view.state, BlameState::Streaming);
        assert_eq!(view.row_count(), 4);
        assert_eq!(view.progress_percent(), Some(0));
        assert_eq!(view.row_view(0).expect("row 0").age, None);
        assert_eq!(view.row_view(0).expect("row 0").gutter, None);

        // First hunk batch arrives with metadata for `first`.
        let mut batch1_commits = HashMap::new();
        batch1_commits.insert(first, commit(1_000));
        assert!(view.progress(ticket, None, &[hunk(first, 1, 2)], &batch1_commits,));
        assert_eq!(view.progress_percent(), Some(50));
        let row0 = view.row_view(0).expect("row 0");
        assert_eq!(row0.gutter, Some(first));
        assert!(row0.meta.is_some());
        assert_eq!(row0.age, Some(1.0));
        let row1 = view.row_view(1).expect("row 1");
        assert_eq!(row1.gutter, None);
        assert_eq!(row1.age, Some(1.0));
        assert_eq!(view.row_view(2).expect("row 2").age, None);

        // Second batch adds `second` (newer) and another hunk for `first`
        // without repeating `first`'s metadata.
        let mut batch2_commits = HashMap::new();
        batch2_commits.insert(second, commit(3_000));
        assert!(view.progress(
            ticket,
            None,
            &[hunk(second, 3, 1), hunk(first, 4, 1)],
            &batch2_commits,
        ));
        assert_eq!(view.progress_percent(), Some(100));
        assert_eq!(view.row_view(0).expect("row 0").age, Some(0.0));
        assert_eq!(view.row_view(2).expect("row 2").age, Some(1.0));
        let row3 = view.row_view(3).expect("row 3");
        assert_eq!(row3.gutter, Some(first));
        assert!(row3.meta.is_some());

        // Accepting the final BlameReady keeps Streaming mounted until ready()
        // swaps in the final formatted rows so scroll does not reset.
        assert!(view.accept(ticket));
        assert_eq!(view.state, BlameState::Streaming);

        let final_data = build_data(
            "src/main.rs",
            &blame(
                vec![hunk(first, 1, 2), hunk(second, 3, 1), hunk(first, 4, 1)],
                &["one", "two", "three", "four"],
            ),
            &view.highlighter,
            None,
        );
        assert!(view.ready(ticket, final_data));
        assert_eq!(view.state, BlameState::Ready);
        assert!(view.partial.is_none());
        assert_eq!(view.progress_percent(), None);
        assert_eq!(view.row_count(), 4);
    }

    #[test]
    fn stale_progress_events_for_replaced_ticket_are_ignored() {
        let mut view = BlameView::default();
        view.begin(oid(1), "a.rs".to_owned(), QueryTicket(1));
        view.begin(oid(2), "b.rs".to_owned(), QueryTicket(2));

        let lines = Arc::new(vec!["hello".to_owned()]);
        assert!(!view.progress(QueryTicket(1), Some(lines), &[], &HashMap::new()));
        assert_eq!(view.state, BlameState::Loading);
        assert!(!view.set_partial_highlights(QueryTicket(1), vec![Vec::new()]));
    }
}
