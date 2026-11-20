//! Stacked working-tree diff pane for the synthetic "uncommitted changes"
//! row in History.
//!
//! Renders conflicted, unstaged, and staged files in one virtualized scroll
//! (`gpui::list`), fetching diffs lazily for sections near the viewport with a
//! bounded LRU cache invalidated by snapshot generation or diff options.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ElementId, FontWeight, IntoElement, ListAlignment, ListOffset, ListState,
    ParentElement, Styled, UniformListScrollHandle, div, list, px, uniform_list,
};
use gpui_component::scroll::Scrollbar;
use gpui_kit_assets::IconName;
use lamina_core::diff::GapAction;
use lamina_core::session::{
    DiffOptions, DiffRequest, DiffTicket, DiffView, RepoSnapshot, RepositorySession,
};
use lamina_core::{ChangeKind, ConflictKind};

use super::RepoView;
use crate::conflicts;
use crate::diff_view::{self, DiffColors, DiffContext, DiffStyle, GapHandler, Row as DiffRow};
use crate::scrollbar::WithScrollbar as _;
use crate::style::{self, Design};
use crate::ui::{self, BadgeTone, ButtonKind, ButtonSize};
use crate::worktree_panel::visible_unstaged;

/// Initial row cap per file section before offering "Show more" / "Show all".
pub(crate) const DEFAULT_ROW_CAP: usize = 150;
/// Additional rows revealed per click on "Show more".
pub(crate) const ROW_CAP_STEP: usize = 150;
/// Maximum cached per-file diffs retained in memory for the stacked view.
const MAX_CACHED_DIFFS: usize = 64;
/// Number of sections beyond each rendered section to prefetch.
const LOOKAHEAD_SECTIONS: usize = 2;
/// Estimated initial height for unmeasured sections in `ListState`.
const ESTIMATED_SECTION_HEIGHT: f32 = 180.0;
/// Overdraw buffer above/below the viewport for `gpui::list`.
const STACK_OVERDRAW: f32 = 400.0;
/// Row height for the left-column grouped file index.
const LEFT_ROW_HEIGHT: f32 = 26.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SectionSide {
    Conflicted,
    Unstaged,
    Staged,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DiffKey {
    pub side: SectionSide,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UncommittedSection {
    pub side: SectionSide,
    pub path: String,
    pub change_kind: ChangeKind,
    pub conflict_kind: Option<ConflictKind>,
}

impl UncommittedSection {
    pub(crate) fn key(&self) -> DiffKey {
        DiffKey {
            side: self.side,
            path: self.path.clone(),
        }
    }

    fn diff_request(&self) -> DiffRequest {
        match self.side {
            SectionSide::Staged => DiffRequest::Staged {
                path: self.path.clone(),
            },
            SectionSide::Unstaged | SectionSide::Conflicted => DiffRequest::Unstaged {
                path: self.path.clone(),
            },
        }
    }
}

/// Builds the ordered section list for the uncommitted details/diff pane:
/// conflicted paths first, then unstaged (excluding conflicted duplicates),
/// then staged.
pub(crate) fn build_sections(snapshot: &RepoSnapshot) -> Vec<UncommittedSection> {
    let unstaged = visible_unstaged(&snapshot.status);
    let mut sections = Vec::with_capacity(
        snapshot.conflicts.len() + unstaged.len() + snapshot.status.staged.len(),
    );
    for entry in snapshot.conflicts.iter() {
        sections.push(UncommittedSection {
            side: SectionSide::Conflicted,
            path: entry.path.clone(),
            change_kind: ChangeKind::Conflicted,
            conflict_kind: Some(entry.kind),
        });
    }
    for change in unstaged {
        sections.push(UncommittedSection {
            side: SectionSide::Unstaged,
            path: change.path.clone(),
            change_kind: change.kind,
            conflict_kind: None,
        });
    }
    for change in snapshot.status.staged.iter() {
        sections.push(UncommittedSection {
            side: SectionSide::Staged,
            path: change.path.clone(),
            change_kind: change.kind,
            conflict_kind: None,
        });
    }
    sections
}

/// Returns the visible prefix of `rows` under `visible_cap` and how many rows
/// remain hidden.
pub(crate) fn capped_slice(rows: &[DiffRow], visible_cap: usize) -> (&[DiffRow], usize) {
    let shown = rows.len().min(visible_cap);
    (&rows[..shown], rows.len() - shown)
}

#[derive(Clone)]
struct CachedEntry {
    generation: u64,
    options: DiffOptions,
    outcome: Result<Arc<DiffView>, String>,
    context: DiffContext,
    rows: Rc<Vec<DiffRow>>,
    visible_cap: usize,
    last_used: u64,
}

struct DiffCache {
    entries: HashMap<DiffKey, CachedEntry>,
    clock: u64,
    max_entries: usize,
}

impl Default for DiffCache {
    fn default() -> Self {
        Self::new(MAX_CACHED_DIFFS)
    }
}

impl DiffCache {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            max_entries: max_entries.max(1),
        }
    }

    fn get_fresh(
        &self,
        key: &DiffKey,
        generation: u64,
        options: DiffOptions,
    ) -> Option<&CachedEntry> {
        self.entries
            .get(key)
            .filter(|entry| entry.generation == generation && entry.options == options)
    }

    fn get_fresh_mut(
        &mut self,
        key: &DiffKey,
        generation: u64,
        options: DiffOptions,
    ) -> Option<&mut CachedEntry> {
        let entry = self
            .entries
            .get_mut(key)
            .filter(|entry| entry.generation == generation && entry.options == options)?;
        self.clock = self.clock.wrapping_add(1);
        entry.last_used = self.clock;
        Some(entry)
    }

    fn insert(&mut self, key: DiffKey, mut entry: CachedEntry) {
        self.clock = self.clock.wrapping_add(1);
        entry.last_used = self.clock;
        self.entries.insert(key, entry);
        while self.entries.len() > self.max_entries {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.entries.remove(&oldest_key);
        }
    }

    fn retain_keys(&mut self, active: &HashSet<DiffKey>) {
        self.entries.retain(|key, _| active.contains(key));
    }
}

/// State for the stacked working-tree diff pane.
pub(crate) struct UncommittedDiffState {
    sections: Vec<UncommittedSection>,
    generation: u64,
    options: DiffOptions,
    zoom: f32,
    list: ListState,
    left_scroll: UniformListScrollHandle,
    cache: DiffCache,
    caps: HashMap<DiffKey, usize>,
    in_flight: HashMap<DiffTicket, (DiffKey, u64, DiffOptions)>,
    pending_indices: Rc<RefCell<Vec<usize>>>,
    defer_scheduled: Rc<Cell<bool>>,
}

impl Default for UncommittedDiffState {
    fn default() -> Self {
        Self {
            sections: Vec::new(),
            generation: 0,
            options: DiffOptions::default(),
            zoom: 1.0,
            list: ListState::new(0, ListAlignment::Top, px(STACK_OVERDRAW)),
            left_scroll: UniformListScrollHandle::default(),
            cache: DiffCache::default(),
            caps: HashMap::new(),
            in_flight: HashMap::new(),
            pending_indices: Rc::new(RefCell::new(Vec::new())),
            defer_scheduled: Rc::new(Cell::new(false)),
        }
    }
}

impl UncommittedDiffState {
    #[cfg(test)]
    fn with_cache_capacity(max_entries: usize) -> Self {
        Self {
            cache: DiffCache::new(max_entries),
            ..Self::default()
        }
    }

    pub(crate) fn sync(
        &mut self,
        snapshot: &RepoSnapshot,
        options: DiffOptions,
        zoom: f32,
        session: Option<&dyn RepositorySession>,
    ) {
        let new_sections = build_sections(snapshot);
        let zoom_changed = (self.zoom - zoom).abs() > 1e-4;
        let context_changed =
            self.generation != snapshot.generation || self.options != options || zoom_changed;
        let sections_changed = self.sections != new_sections;

        let active_keys: HashSet<DiffKey> =
            new_sections.iter().map(UncommittedSection::key).collect();
        self.caps.retain(|key, _| active_keys.contains(key));
        self.cache.retain_keys(&active_keys);

        let stale_tickets: Vec<DiffTicket> = self
            .in_flight
            .iter()
            .filter_map(|(&ticket, (key, ticket_gen, opts))| {
                let valid = *ticket_gen == snapshot.generation
                    && *opts == options
                    && active_keys.contains(key);
                (!valid).then_some(ticket)
            })
            .collect();
        for ticket in stale_tickets {
            self.in_flight.remove(&ticket);
            if let Some(session) = session {
                session.cancel_diff(ticket);
            }
        }

        if sections_changed {
            let top = self.list.logical_scroll_top();
            self.sections = new_sections;
            self.list.reset_with_uniform_height(
                self.sections.len(),
                px(ESTIMATED_SECTION_HEIGHT * zoom),
            );
            if !self.sections.is_empty() {
                self.list.scroll_to(ListOffset {
                    item_ix: top.item_ix.min(self.sections.len()),
                    offset_in_item: top.offset_in_item,
                });
            }
        } else if context_changed {
            self.list.remeasure();
        }

        self.generation = snapshot.generation;
        self.options = options;
        self.zoom = zoom;
        if context_changed || sections_changed {
            self.pending_indices.borrow_mut().clear();
        }
    }

    pub(crate) fn cancel_all(&mut self, session: Option<&dyn RepositorySession>) {
        if let Some(session) = session {
            for ticket in self.in_flight.keys().copied() {
                session.cancel_diff(ticket);
            }
        }
        self.in_flight.clear();
        self.pending_indices.borrow_mut().clear();
        self.defer_scheduled.set(false);
    }

    pub(crate) fn scroll_to_section(&mut self, index: usize) {
        if index < self.sections.len() {
            self.list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.0),
            });
        }
    }

    pub(crate) fn on_diff_ready(&mut self, ticket: DiffTicket, view: &Arc<DiffView>) -> bool {
        let Some((key, ticket_gen, opts)) = self.in_flight.remove(&ticket) else {
            return false;
        };
        if ticket_gen != self.generation || opts != self.options {
            return true;
        }
        let Some(index) = self
            .sections
            .iter()
            .position(|section| section.key() == key)
        else {
            return true;
        };
        let context = DiffContext::default();
        let rows = Rc::new(context.rows(view));
        let visible_cap = self.caps.get(&key).copied().unwrap_or(DEFAULT_ROW_CAP);
        self.cache.insert(
            key,
            CachedEntry {
                generation: ticket_gen,
                options: opts,
                outcome: Ok(Arc::clone(view)),
                context,
                rows,
                visible_cap,
                last_used: 0,
            },
        );
        self.list.splice(index..index + 1, 1);
        true
    }

    pub(crate) fn on_diff_failed(&mut self, ticket: DiffTicket, error: &str) -> bool {
        let Some((key, ticket_gen, opts)) = self.in_flight.remove(&ticket) else {
            return false;
        };
        if ticket_gen != self.generation || opts != self.options {
            return true;
        }
        let Some(index) = self
            .sections
            .iter()
            .position(|section| section.key() == key)
        else {
            return true;
        };
        self.cache.insert(
            key,
            CachedEntry {
                generation: ticket_gen,
                options: opts,
                outcome: Err(error.to_owned()),
                context: DiffContext::default(),
                rows: Rc::new(Vec::new()),
                visible_cap: DEFAULT_ROW_CAP,
                last_used: 0,
            },
        );
        self.list.splice(index..index + 1, 1);
        true
    }

    fn flush_requests(&mut self, session: &dyn RepositorySession, smoke: bool) {
        let indices: Vec<usize> = self.pending_indices.borrow_mut().drain(..).collect();
        if smoke || indices.is_empty() || self.sections.is_empty() {
            return;
        }
        let mut requested = HashSet::new();
        for base in indices {
            let end = (base + 1 + LOOKAHEAD_SECTIONS).min(self.sections.len());
            for idx in base..end {
                if !requested.insert(idx) {
                    continue;
                }
                let Some(section) = self.sections.get(idx) else {
                    continue;
                };
                let key = section.key();
                if self
                    .cache
                    .get_fresh(&key, self.generation, self.options)
                    .is_some()
                {
                    continue;
                }
                let already_pending = self.in_flight.values().any(|(k, ticket_gen, opts)| {
                    *k == key && *ticket_gen == self.generation && *opts == self.options
                });
                if already_pending {
                    continue;
                }
                let ticket = session.request_diff_with(section.diff_request(), self.options);
                self.in_flight
                    .insert(ticket, (key, self.generation, self.options));
            }
        }
    }

    fn expand_gap(&mut self, index: usize, gap: usize, action: GapAction) -> bool {
        let Some(key) = self.sections.get(index).map(UncommittedSection::key) else {
            return false;
        };
        let Some(entry) = self
            .cache
            .get_fresh_mut(&key, self.generation, self.options)
        else {
            return false;
        };
        let Ok(view) = entry.outcome.as_ref().map(Arc::clone) else {
            return false;
        };
        let old_len = entry.rows.len();
        let changed = entry.context.expand(gap, action, &view);
        if changed {
            let rows = entry.context.rows(&view);
            let added = rows.len().saturating_sub(old_len);
            entry.rows = Rc::new(rows);
            if entry.visible_cap != usize::MAX {
                entry.visible_cap = entry.visible_cap.saturating_add(added);
                self.caps.insert(key, entry.visible_cap);
            }
        }
        if changed || entry.context.notice().is_some() {
            self.list.splice(index..index + 1, 1);
            return true;
        }
        false
    }

    fn show_more(&mut self, index: usize) -> bool {
        let Some(key) = self.sections.get(index).map(UncommittedSection::key) else {
            return false;
        };
        let Some(entry) = self
            .cache
            .get_fresh_mut(&key, self.generation, self.options)
        else {
            return false;
        };
        entry.visible_cap = entry.visible_cap.saturating_add(ROW_CAP_STEP);
        self.caps.insert(key, entry.visible_cap);
        self.list.splice(index..index + 1, 1);
        true
    }

    fn show_all(&mut self, index: usize) -> bool {
        let Some(key) = self.sections.get(index).map(UncommittedSection::key) else {
            return false;
        };
        let Some(entry) = self
            .cache
            .get_fresh_mut(&key, self.generation, self.options)
        else {
            return false;
        };
        entry.visible_cap = usize::MAX;
        self.caps.insert(key, usize::MAX);
        self.list.splice(index..index + 1, 1);
        true
    }

    fn section_ready_flags(&self) -> Vec<bool> {
        self.sections
            .iter()
            .map(|section| {
                let key = section.key();
                self.cache
                    .get_fresh(&key, self.generation, self.options)
                    .is_some()
                    || self.in_flight.values().any(|(k, ticket_gen, opts)| {
                        *k == key && *ticket_gen == self.generation && *opts == self.options
                    })
            })
            .collect()
    }
}

enum LeftRow {
    GroupHeader {
        side: SectionSide,
        count: usize,
    },
    FileItem {
        section_index: usize,
        path: String,
        change_kind: ChangeKind,
    },
}

fn build_left_rows(sections: &[UncommittedSection]) -> Vec<LeftRow> {
    let mut rows = Vec::new();
    for side in [
        SectionSide::Conflicted,
        SectionSide::Unstaged,
        SectionSide::Staged,
    ] {
        let matching: Vec<(usize, &UncommittedSection)> = sections
            .iter()
            .enumerate()
            .filter(|(_, section)| section.side == side)
            .collect();
        if matching.is_empty() {
            continue;
        }
        rows.push(LeftRow::GroupHeader {
            side,
            count: matching.len(),
        });
        for (section_index, section) in matching {
            rows.push(LeftRow::FileItem {
                section_index,
                path: section.path.clone(),
                change_kind: section.change_kind,
            });
        }
    }
    rows
}

fn side_badge(side: SectionSide, design: &Design) -> AnyElement {
    match side {
        SectionSide::Conflicted => ui::status_badge(
            t!("worktree.side.conflict").to_string(),
            BadgeTone::Danger,
            design,
        ),
        SectionSide::Unstaged => ui::status_badge(
            t!("worktree.side.unstaged").to_string(),
            BadgeTone::Warning,
            design,
        ),
        SectionSide::Staged => ui::status_badge(
            t!("worktree.side.staged").to_string(),
            BadgeTone::Accent,
            design,
        ),
    }
}

impl RepoView {
    pub(super) fn sync_uncommitted_diff(&mut self, cx: &App) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        // Wait for an active operation's final snapshot before re-computing diffs.
        if snapshot.busy.is_some() {
            return;
        }
        let options = super::history_diff_options(cx);
        let zoom = crate::settings::Settings::get(cx).appearance.zoom();
        self.uncommitted_diff
            .sync(&snapshot, options, zoom, Some(session.as_ref()));
    }

    pub(super) fn flush_uncommitted_diff_requests(&mut self, cx: &mut Context<Self>) {
        if !self.uncommitted_selected || self.mode == crate::worktree_panel::MainMode::Reflog {
            self.uncommitted_diff.pending_indices.borrow_mut().clear();
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.uncommitted_diff
            .flush_requests(session.as_ref(), self.smoke);
        cx.notify();
    }

    pub(super) fn uncommitted_diff_ready(
        &mut self,
        ticket: DiffTicket,
        view: &Arc<DiffView>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.uncommitted_diff.on_diff_ready(ticket, view) {
            return false;
        }
        self.notify_if_active(cx);
        true
    }

    pub(super) fn uncommitted_diff_failed(
        &mut self,
        ticket: DiffTicket,
        error: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.uncommitted_diff.on_diff_failed(ticket, error) {
            return false;
        }
        self.notify_if_active(cx);
        true
    }

    pub(super) fn scroll_to_uncommitted_section(&mut self, index: usize, cx: &mut Context<Self>) {
        self.uncommitted_diff.scroll_to_section(index);
        cx.notify();
    }

    pub(super) fn expand_uncommitted_gap(
        &mut self,
        index: usize,
        gap: usize,
        action: GapAction,
        cx: &mut Context<Self>,
    ) {
        if self.uncommitted_diff.expand_gap(index, gap, action) {
            self.notify_if_active(cx);
        }
    }

    pub(super) fn show_more_uncommitted_rows(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.uncommitted_diff.show_more(index) {
            self.notify_if_active(cx);
        }
    }

    pub(super) fn show_all_uncommitted_rows(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.uncommitted_diff.show_all(index) {
            self.notify_if_active(cx);
        }
    }

    pub(super) fn uncommitted_details_column(&self, design: &Design) -> AnyElement {
        let summary = self
            .uncommitted_summary()
            .unwrap_or_else(|| t!("history.uncommitted.summary").to_string());
        let counts = self.uncommitted_counts();
        let entity = self.entity();

        let header = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(design.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(gpui::relative(1.35))
                    .child(summary),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(design.px(6.0))
                    .when(counts.conflicts > 0, |row| {
                        row.child(ui::status_badge(
                            t!("history.uncommitted.conflicts", count = counts.conflicts)
                                .to_string(),
                            BadgeTone::Danger,
                            design,
                        ))
                    })
                    .when(counts.unstaged > 0, |row| {
                        row.child(ui::status_badge(
                            t!("history.uncommitted.unstaged", count = counts.unstaged).to_string(),
                            BadgeTone::Warning,
                            design,
                        ))
                    })
                    .when(counts.staged > 0, |row| {
                        row.child(ui::status_badge(
                            t!("history.uncommitted.staged", count = counts.staged).to_string(),
                            BadgeTone::Accent,
                            design,
                        ))
                    }),
            )
            .child(
                div().flex().items_center().gap_2().child(
                    ui::button(
                        "uncommitted-open-worktree",
                        t!("history.uncommitted.open_working_tree").to_string(),
                    )
                    .icon(IconName::ListChecks)
                    .size(ButtonSize::Small)
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| this.open_worktree(window, cx));
                    }),
                ),
            );

        let left_rows = Rc::new(build_left_rows(&self.uncommitted_diff.sections));
        let body = if left_rows.is_empty() {
            div()
                .p_3()
                .text_size(design.px(12.0))
                .text_color(design.muted)
                .child(t!("history.uncommitted.no_file_changes").to_string())
                .into_any_element()
        } else {
            let list_design = design.clone();
            let click_entity = self.entity();
            let rows = Rc::clone(&left_rows);
            uniform_list(
                "uncommitted-left-files",
                rows.len(),
                move |range, _window, _cx| {
                    range
                        .map(|row_index| match &rows[row_index] {
                            LeftRow::GroupHeader { side, count } => {
                                let title = match side {
                                    SectionSide::Conflicted => t!("worktree.section.conflicts"),
                                    SectionSide::Unstaged => t!("worktree.section.unstaged"),
                                    SectionSide::Staged => t!("worktree.section.staged"),
                                };
                                let mut header = ui::section_header(
                                    ElementId::named_usize("uncommitted-group", row_index),
                                    title,
                                    &list_design,
                                )
                                .count(*count)
                                .h(list_design.px(LEFT_ROW_HEIGHT))
                                .px_3()
                                .rounded(px(0.0));
                                if *side == SectionSide::Conflicted {
                                    header = header.danger();
                                }
                                header.into_any_element()
                            }
                            LeftRow::FileItem {
                                section_index,
                                path,
                                change_kind,
                            } => {
                                let (name, dir) = style::path_parts(path);
                                let target = *section_index;
                                let click_entity = click_entity.clone();
                                ui::list_row(
                                    ElementId::named_usize("uncommitted-left-file", target),
                                    ui::RowState::NORMAL,
                                    &list_design,
                                )
                                .h(list_design.px(LEFT_ROW_HEIGHT))
                                .cursor_pointer()
                                .on_click(move |_, _, cx| {
                                    click_entity.update(cx, |this, cx| {
                                        this.scroll_to_uncommitted_section(target, cx);
                                    });
                                })
                                .child(ui::change_badge(*change_kind, &list_design))
                                .child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
                                .when_some(dir, |row, dir| {
                                    row.child(
                                        div()
                                            .max_w(list_design.px(110.0))
                                            .flex_none()
                                            .truncate()
                                            .text_size(list_design.px(11.0))
                                            .text_color(list_design.muted)
                                            .child(dir.to_owned()),
                                    )
                                })
                                .into_any_element()
                            }
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.uncommitted_diff.left_scroll)
            .with_scrollbar(&self.uncommitted_diff.left_scroll)
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(header)
            .child(body)
            .into_any_element()
    }

    pub(super) fn uncommitted_diff_pane(&self, design: &Design, cx: &mut App) -> AnyElement {
        let header = self.uncommitted_diff_header(design, cx);
        let sections = &self.uncommitted_diff.sections;
        let body = if sections.is_empty() {
            ui::empty_state(
                t!("history.uncommitted.no_file_changes").to_string(),
                design,
            )
            .into_any_element()
        } else {
            let diff_colors = DiffColors::from_design(design);
            let diff_style = DiffStyle::from_settings(cx, &design.mono);
            let render_design = design.clone();
            let entity = self.entity();
            let generation = self.uncommitted_diff.generation;
            let options = self.uncommitted_diff.options;
            let sections: Rc<[UncommittedSection]> = Rc::from(sections.as_slice());
            let entries: Rc<[Option<CachedEntry>]> = sections
                .iter()
                .map(|section| {
                    self.uncommitted_diff
                        .cache
                        .get_fresh(&section.key(), generation, options)
                        .cloned()
                })
                .collect();
            let ready_flags: Rc<[bool]> = Rc::from(self.uncommitted_diff.section_ready_flags());
            let pending_indices = Rc::clone(&self.uncommitted_diff.pending_indices);
            let defer_scheduled = Rc::clone(&self.uncommitted_diff.defer_scheduled);
            let list_state = self.uncommitted_diff.list.clone();

            div()
                .relative()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(
                    list(list_state.clone(), move |index, window, cx| {
                        let Some(section) = sections.get(index) else {
                            return div().into_any_element();
                        };

                        let end = (index + 1 + LOOKAHEAD_SECTIONS).min(ready_flags.len());
                        let needs_fetch =
                            (index..end).any(|idx| ready_flags.get(idx).copied() == Some(false));
                        if needs_fetch {
                            pending_indices.borrow_mut().push(index);
                            if !defer_scheduled.replace(true) {
                                let defer_entity = entity.clone();
                                let defer_flag = Rc::clone(&defer_scheduled);
                                window.defer(cx, move |_window, cx| {
                                    defer_flag.set(false);
                                    defer_entity.update(cx, |this, cx| {
                                        this.flush_uncommitted_diff_requests(cx);
                                    });
                                });
                            }
                        }

                        render_section(
                            index,
                            section,
                            entries.get(index).and_then(|entry| entry.as_ref()),
                            &render_design,
                            &diff_colors,
                            &diff_style,
                            entity.clone(),
                        )
                    })
                    .size_full(),
                )
                .child(Scrollbar::vertical(&list_state).id("uncommitted-diff-list"))
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn uncommitted_diff_header(&self, design: &Design, cx: &App) -> AnyElement {
        let title = self
            .uncommitted_summary()
            .unwrap_or_else(|| t!("history.uncommitted.summary").to_string());
        let file_count = self.uncommitted_diff.sections.len();
        let right = if file_count == 0 {
            String::new()
        } else {
            t!("diff.header.changed", count = file_count).to_string()
        };
        let maximized = self.details_maximized(cx);
        let entity = self.entity();

        div()
            .flex()
            .items_center()
            .gap(design.px(10.0))
            .h(design.px(40.0))
            .flex_none()
            .px_3p5()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .text_size(design.px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(design.px(11.0))
                    .text_color(design.muted)
                    .child(right),
            )
            .child(
                ui::icon_button(
                    "toggle-details-maximized",
                    if maximized {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    },
                )
                .size(ButtonSize::Tiny)
                .ghost()
                .tooltip(t!("action.toggle_details_maximized").to_string())
                .on_click(move |_event, _window, cx| {
                    entity.update(cx, |this, cx| this.toggle_details_maximized(cx));
                }),
            )
            .into_any_element()
    }
}

fn render_section(
    index: usize,
    section: &UncommittedSection,
    entry: Option<&CachedEntry>,
    design: &Design,
    colors: &DiffColors,
    style: &DiffStyle,
    entity: gpui::Entity<RepoView>,
) -> AnyElement {
    let (name, dir) = style::path_parts(&section.path);
    let encoding = entry
        .and_then(|entry| entry.outcome.as_ref().ok())
        .and_then(|view| style::encoding_label(view));

    let header = div()
        .flex()
        .items_center()
        .gap(design.px(8.0))
        .min_h(design.px(34.0))
        .px_3()
        .py_1p5()
        .bg(design.panel)
        .border_t_1()
        .border_b_1()
        .border_color(design.border)
        .text_size(design.px(12.0))
        .child(ui::change_badge(section.change_kind, design))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(name.to_owned()),
                )
                .when_some(dir, |row, dir| {
                    row.child(
                        div()
                            .ml_2()
                            .min_w_0()
                            .truncate()
                            .text_size(design.px(11.5))
                            .text_color(design.muted)
                            .child(dir.to_owned()),
                    )
                }),
        )
        .child(side_badge(section.side, design))
        .when_some(section.conflict_kind, |row, kind| {
            row.child(ui::status_badge(
                conflicts::kind_label(kind),
                BadgeTone::Warning,
                design,
            ))
        })
        .when_some(encoding, |row, label| {
            row.child(ui::status_badge(label, BadgeTone::Neutral, design))
        })
        .child(render_section_actions(index, section, entity.clone()));

    let body = match entry {
        None => div()
            .px_3()
            .py_2()
            .text_size(design.px(12.0))
            .text_color(design.muted)
            .child(t!("worktree.computing_diff").to_string())
            .into_any_element(),
        Some(entry) => match &entry.outcome {
            Err(error) => div()
                .px_3()
                .py_2()
                .text_size(design.px(12.0))
                .text_color(design.remove_text)
                .child(error.clone())
                .into_any_element(),
            Ok(view) => {
                let mut container = div().flex().flex_col().w_full().overflow_hidden();
                if let Some(notice) = entry.context.notice() {
                    container =
                        container.child(ui::banner(ui::BannerKind::Warning, notice, design));
                }
                if let Some(summary) = view.lfs.clone() {
                    let missing = summary.old.as_ref().is_some_and(|side| !side.present)
                        || summary.new.as_ref().is_some_and(|side| !side.present);
                    let mut card = ui::lfs_card(summary, design);
                    if missing {
                        let path = section.path.clone();
                        let lfs_entity = entity.clone();
                        card = card.action(
                            ui::button(
                                ElementId::named_usize("uncommitted-lfs-download", index),
                                t!("worktree.download").to_string(),
                            )
                            .size(ButtonSize::Tiny)
                            .on_click(move |_, window, cx| {
                                let path = path.clone();
                                lfs_entity.update(cx, |this, cx| {
                                    this.run_operation(
                                        lamina_core::session::Operation::LfsPull { path },
                                        t!("worktree.download_lfs").to_string(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                        );
                    }
                    container = container.child(card);
                }
                if view.diff.binary {
                    container = container.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_size(design.px(12.0))
                            .text_color(design.muted)
                            .child(t!("worktree.binary_file").to_string()),
                    );
                } else if entry.rows.is_empty() {
                    if view.lfs.is_none() {
                        container = container.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_size(design.px(12.0))
                                .text_color(design.muted)
                                .child(t!("worktree.no_text_changes").to_string()),
                        );
                    }
                } else {
                    let (visible_rows, hidden) = capped_slice(&entry.rows, entry.visible_cap);
                    let gap_entity = entity.clone();
                    let on_gap: GapHandler = Rc::new(move |gap, action, _window, cx| {
                        gap_entity.update(cx, |this, cx| {
                            this.expand_uncommitted_gap(index, gap, action, cx);
                        });
                    });
                    for element in diff_view::render_stacked_rows(
                        index,
                        visible_rows,
                        Some(view),
                        colors,
                        style,
                        Some(&on_gap),
                    ) {
                        container = container.child(element);
                    }
                    if hidden > 0 {
                        let more_count = hidden.min(ROW_CAP_STEP);
                        let reveal_entity = entity.clone();
                        container = container.child(diff_view::hidden_lines_bar(
                            index,
                            hidden,
                            t!("history.uncommitted.show_more_lines", count = more_count)
                                .to_string(),
                            t!("history.uncommitted.show_all_lines").to_string(),
                            colors,
                            style,
                            move |mode, _, cx| {
                                reveal_entity.update(cx, |this, cx| match mode {
                                    diff_view::CapReveal::Step => {
                                        this.show_more_uncommitted_rows(index, cx);
                                    }
                                    diff_view::CapReveal::All => {
                                        this.show_all_uncommitted_rows(index, cx);
                                    }
                                });
                            },
                        ));
                    }
                }
                container.into_any_element()
            }
        },
    };

    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .pb_3()
        .child(header)
        .child(body)
        .into_any_element()
}

fn render_section_actions(
    index: usize,
    section: &UncommittedSection,
    entity: gpui::Entity<RepoView>,
) -> AnyElement {
    let mut actions = div().flex().flex_none().items_center().gap_1();
    match section.side {
        SectionSide::Unstaged => {
            let stage_path = section.path.clone();
            let stage_entity = entity.clone();
            actions = actions.child(
                ui::button(
                    ElementId::named_usize("uncommitted-stage", index),
                    t!("common.stage").to_string(),
                )
                .size(ButtonSize::Tiny)
                .on_click(move |_, _window, cx| {
                    let path = stage_path.clone();
                    stage_entity.update(cx, |this, cx| this.stage_path(path, cx));
                }),
            );

            let discard_path = section.path.clone();
            let discard_label = if section.change_kind == ChangeKind::Untracked {
                t!("common.delete").to_string()
            } else {
                t!("common.discard").to_string()
            };
            actions = actions.child(
                ui::button(
                    ElementId::named_usize("uncommitted-discard-unstaged", index),
                    discard_label,
                )
                .size(ButtonSize::Tiny)
                .kind(ButtonKind::Danger)
                .on_click(move |_, window, cx| {
                    let path = discard_path.clone();
                    entity.update(cx, |this, cx| {
                        this.discard_unstaged_path(path, window, cx);
                    });
                }),
            );
        }
        SectionSide::Staged => {
            let unstage_path = section.path.clone();
            let unstage_entity = entity.clone();
            actions = actions.child(
                ui::button(
                    ElementId::named_usize("uncommitted-unstage", index),
                    t!("common.unstage").to_string(),
                )
                .size(ButtonSize::Tiny)
                .on_click(move |_, _window, cx| {
                    let path = unstage_path.clone();
                    unstage_entity.update(cx, |this, cx| this.unstage_path(path, cx));
                }),
            );

            let discard_path = section.path.clone();
            actions = actions.child(
                ui::button(
                    ElementId::named_usize("uncommitted-discard-staged", index),
                    t!("common.discard").to_string(),
                )
                .size(ButtonSize::Tiny)
                .kind(ButtonKind::Danger)
                .on_click(move |_, window, cx| {
                    let path = discard_path.clone();
                    entity.update(cx, |this, cx| {
                        this.discard_staged_path(path, window, cx);
                    });
                }),
            );
        }
        SectionSide::Conflicted => {
            if let Some(kind) = section.conflict_kind {
                for (choice_ix, choice) in conflicts::choices(kind).into_iter().enumerate() {
                    let resolve_path = section.path.clone();
                    let resolve_entity = entity.clone();
                    let side = choice.side;
                    let mut button = ui::button(
                        ElementId::named_usize(
                            format!("uncommitted-conflict-choice-{choice_ix}"),
                            index,
                        ),
                        choice.label,
                    )
                    .size(ButtonSize::Tiny);
                    if choice.destructive {
                        button = button.kind(ButtonKind::Danger);
                    }
                    actions = actions.child(button.on_click(move |_, _window, cx| {
                        let path = resolve_path.clone();
                        resolve_entity.update(cx, |this, cx| {
                            this.resolve_conflict_path(path, side, cx);
                        });
                    }));
                }
            }

            let editor_path = section.path.clone();
            actions = actions.child(
                ui::button(
                    ElementId::named_usize("uncommitted-open-merge-editor", index),
                    t!("history.uncommitted.open_merge_editor").to_string(),
                )
                .size(ButtonSize::Tiny)
                .on_click(move |_, window, cx| {
                    let path = editor_path.clone();
                    entity.update(cx, |this, cx| {
                        this.open_conflict_in_merge_editor(path, window, cx);
                    });
                }),
            );
        }
    }
    actions.into_any_element()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use lamina_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
    use lamina_core::encoding::TextEncoding;
    use lamina_core::graph::{GraphLayout, GraphRows};
    use lamina_core::session::{CommitList, DiffOptions, DiffRequest, DiffView, RepoSnapshot};
    use lamina_core::test_support::TestRepo;
    use lamina_core::{RepoState, WorktreeStatus};

    use super::*;

    fn snapshot_from_repo(repo: &TestRepo) -> RepoSnapshot {
        let opened = repo.open();
        let head = opened.head().ok();
        let status = opened.status().expect("status");
        let conflicts = opened.conflicts().expect("conflicts");
        let state = opened.state().expect("state");
        let refs = opened.refs().unwrap_or_default();

        let mut layout = GraphLayout::new();
        let mut commit_vec = Vec::new();
        let mut rows = GraphRows::default();
        let mut commit_index = HashMap::new();
        for (i, item) in opened.walk_commits().expect("walk").enumerate() {
            let commit = item.expect("commit");
            layout.push(commit.id, &commit.parents, &mut rows);
            commit_index.insert(commit.id, i);
            commit_vec.push(commit);
        }
        let mut commits = CommitList::default();
        commits.extend(commit_vec, rows);
        commits.complete = true;

        RepoSnapshot {
            generation: 1,
            head,
            refs: Arc::new(refs),
            refs_by_commit: Arc::new(HashMap::new()),
            commits,
            commit_index: Arc::new(commit_index),
            status: Arc::new(status),
            stashes: Arc::new(Vec::new()),
            upstreams: Arc::new(HashMap::new()),
            remotes: Vec::new(),
            state,
            conflicts: Arc::new(conflicts),
            busy: None,
        }
    }

    fn dummy_diff_view(path: &str, line_count: usize) -> Arc<DiffView> {
        let lines = (0..line_count)
            .map(|i| DiffLine {
                kind: LineKind::Added,
                old_lineno: None,
                new_lineno: Some((i + 1) as u32),
                content: format!("line {i}"),
                emphasis: Vec::new(),
                no_newline_at_eof: false,
            })
            .collect();
        Arc::new(DiffView {
            request: DiffRequest::Unstaged {
                path: path.to_owned(),
            },
            diff: FileDiff {
                binary: false,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_len: 0,
                    new_start: 1,
                    new_len: line_count as u32,
                    lines,
                }],
            },
            old_highlights: Arc::new(Vec::new()),
            new_highlights: Arc::new(Vec::new()),
            lfs: None,
            old_text: Arc::from(""),
            new_text: Arc::from(""),
            old_encoding: TextEncoding::UTF_8,
            new_encoding: TextEncoding::UTF_8,
            gaps: Arc::from(Vec::new()),
            image: None,
        })
    }

    fn snapshot_with_paths(
        conflicts: &[&str],
        unstaged: &[&str],
        staged: &[&str],
        generation: u64,
    ) -> RepoSnapshot {
        let mut status = WorktreeStatus::default();
        for path in unstaged {
            status.unstaged.push(lamina_core::FileChange {
                path: (*path).to_owned(),
                old_path: None,
                kind: ChangeKind::Modified,
            });
        }
        for path in staged {
            status.staged.push(lamina_core::FileChange {
                path: (*path).to_owned(),
                old_path: None,
                kind: ChangeKind::Added,
            });
        }
        let conflict_entries = conflicts
            .iter()
            .map(|path| lamina_core::ConflictEntry {
                path: (*path).to_owned(),
                kind: ConflictKind::BothModified,
            })
            .collect();
        RepoSnapshot {
            generation,
            head: None,
            refs: Arc::new(Vec::new()),
            refs_by_commit: Arc::new(HashMap::new()),
            commits: CommitList::default(),
            commit_index: Arc::new(HashMap::new()),
            status: Arc::new(status),
            stashes: Arc::new(Vec::new()),
            upstreams: Arc::new(HashMap::new()),
            remotes: Vec::new(),
            state: if conflicts.is_empty() {
                RepoState::Normal
            } else {
                RepoState::Merging
            },
            conflicts: Arc::new(conflict_entries),
            busy: None,
        }
    }

    #[test]
    fn section_ordering_puts_conflicts_then_unstaged_then_staged() {
        let repo = TestRepo::new();
        repo.write("shared.txt", "base\n");
        repo.write("staged.txt", "base\n");
        repo.write("unstaged.txt", "base\n");
        repo.commit("base");

        repo.git(&["checkout", "-b", "side"]);
        repo.write("shared.txt", "side\n");
        repo.commit("side");

        repo.git(&["checkout", "main"]);
        repo.write("shared.txt", "main\n");
        repo.commit("main");

        let _ = repo.git_output(&["merge", "side"]);
        repo.write("unstaged.txt", "edited\n");
        repo.write("staged.txt", "staged edit\n");
        repo.git(&["add", "staged.txt"]);

        let snap = snapshot_from_repo(&repo);
        let sections = build_sections(&snap);

        let summary: Vec<(SectionSide, &str)> = sections
            .iter()
            .map(|section| (section.side, section.path.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (SectionSide::Conflicted, "shared.txt"),
                (SectionSide::Unstaged, "unstaged.txt"),
                (SectionSide::Staged, "staged.txt"),
            ],
            "sections must be ordered Conflicted -> Unstaged -> Staged without duplicating conflicted paths"
        );
    }

    #[test]
    fn cache_invalidates_on_generation_and_options_and_evicts_lru() {
        let mut state = UncommittedDiffState::with_cache_capacity(2);
        let opts_a = DiffOptions::default();
        let opts_b = DiffOptions {
            ignore_whitespace: true,
            ..DiffOptions::default()
        };

        let snap1 = snapshot_with_paths(&[], &["a.rs", "b.rs", "c.rs"], &[], 1);
        state.sync(&snap1, opts_a, 1.0, None);

        for (i, path) in ["a.rs", "b.rs", "c.rs"].iter().enumerate() {
            let ticket = DiffTicket((i + 1) as u64);
            let key = DiffKey {
                side: SectionSide::Unstaged,
                path: (*path).to_owned(),
            };
            state.in_flight.insert(ticket, (key, 1, opts_a));
            assert!(state.on_diff_ready(ticket, &dummy_diff_view(path, 5)));
        }

        // Cache capacity 2 evicted oldest ("a.rs"), retaining "b.rs" and "c.rs".
        assert!(
            state
                .cache
                .get_fresh(
                    &DiffKey {
                        side: SectionSide::Unstaged,
                        path: "a.rs".into(),
                    },
                    1,
                    opts_a,
                )
                .is_none()
        );
        assert!(
            state
                .cache
                .get_fresh(
                    &DiffKey {
                        side: SectionSide::Unstaged,
                        path: "b.rs".into(),
                    },
                    1,
                    opts_a,
                )
                .is_some()
        );
        assert!(
            state
                .cache
                .get_fresh(
                    &DiffKey {
                        side: SectionSide::Unstaged,
                        path: "c.rs".into(),
                    },
                    1,
                    opts_a,
                )
                .is_some()
        );

        // Changing options or generation invalidates cached entries.
        let snap2 = snapshot_with_paths(&[], &["a.rs", "b.rs", "c.rs"], &[], 2);
        state.sync(&snap2, opts_b, 1.0, None);
        assert!(
            state
                .cache
                .get_fresh(
                    &DiffKey {
                        side: SectionSide::Unstaged,
                        path: "b.rs".into(),
                    },
                    2,
                    opts_b,
                )
                .is_none()
        );
    }

    #[test]
    fn row_cap_truncates_and_expands_stepwise_and_fully() {
        let mut state = UncommittedDiffState::default();
        let opts = DiffOptions::default();
        let snap = snapshot_with_paths(&[], &["big.txt"], &[], 1);
        state.sync(&snap, opts, 1.0, None);

        let key = DiffKey {
            side: SectionSide::Unstaged,
            path: "big.txt".into(),
        };
        let ticket = DiffTicket(42);
        state.in_flight.insert(ticket, (key.clone(), 1, opts));
        // 1 hunk header + 400 diff lines = 401 rows.
        assert!(state.on_diff_ready(ticket, &dummy_diff_view("big.txt", 400)));

        let entry = state.cache.get_fresh(&key, 1, opts).unwrap();
        let (shown, hidden) = capped_slice(&entry.rows, entry.visible_cap);
        assert_eq!(shown.len(), DEFAULT_ROW_CAP);
        assert_eq!(hidden, 401 - DEFAULT_ROW_CAP);

        assert!(state.show_more(0));
        let entry = state.cache.get_fresh(&key, 1, opts).unwrap();
        let (shown, hidden) = capped_slice(&entry.rows, entry.visible_cap);
        assert_eq!(shown.len(), DEFAULT_ROW_CAP + ROW_CAP_STEP);
        assert_eq!(hidden, 401 - (DEFAULT_ROW_CAP + ROW_CAP_STEP));

        assert!(state.show_all(0));
        let entry = state.cache.get_fresh(&key, 1, opts).unwrap();
        let (shown, hidden) = capped_slice(&entry.rows, entry.visible_cap);
        assert_eq!(shown.len(), 401);
        assert_eq!(hidden, 0);
    }
}
