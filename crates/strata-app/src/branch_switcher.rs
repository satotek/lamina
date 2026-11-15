//! The branch switcher: the popover under the title bar's branch chip.
//!
//! The chip used to draw a chevron and do nothing. It now opens a filterable
//! list of the branches the sidebar already knows about: local branches with
//! their upstream counts, remote-tracking branches that have no local branch
//! yet, and the create-branch flow at the bottom.
//!
//! The list is built from [`SidebarData`] and the snapshot's upstream map, not
//! by reading the repository again, and switching runs the same
//! [`Operation`] the sidebar's rows do. Nothing here runs git.
//!
//! The popover itself is `gpui-component`'s [`Popover`], so it is anchored to
//! the chip, painted above the window's content, dismissed by a click outside
//! it and returns the keyboard where it came from. Its own state lives here so
//! the command palette and the `Ctrl+B` binding can open it without the chip.

use std::collections::HashMap;

use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, Entity, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, actions, div, px,
};
use gpui_component::Sizable as _;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::popover::Popover;
use gpui_component::tooltip::Tooltip;
use strata_core::UpstreamInfo;
use strata_core::session::Operation;

use crate::data::{SidebarData, Source};
use crate::pull_requests::BranchPullRequests;
use crate::repo_view::RepoView;
use crate::style::{self, Design};
use crate::ui;
use crate::ui::{BadgeTone, RowState, RowTipExt as _, list_row, status_badge};

/// Key context of the popover's card. The text field sits inside it, with the
/// field's own `Input` context, so the card's keys only run while it is up.
const SWITCHER: &str = "BranchSwitcher";
/// The card's width: wide enough for `origin/feature/long-name` and its
/// ahead/behind label, narrow enough to sit under the chip.
const WIDTH: f32 = 360.0;
/// How much of the list is shown before it scrolls.
const LIST_MAX_HEIGHT: f32 = 372.0;
/// Height of one line, matching [`crate::ui::list_row`].
const ROW_HEIGHT: f32 = 28.0;

actions!(
    strata,
    [
        /// Moves the branch switcher's highlight up one entry.
        BranchSwitcherPrevious,
        /// Moves the branch switcher's highlight down one entry.
        BranchSwitcherNext,
        /// Switches to the highlighted branch.
        BranchSwitcherConfirm,
        /// Closes the branch switcher.
        BranchSwitcherDismiss,
    ]
);

/// Binds the switcher's own keys. `Ctrl+B` itself is bound in `actions::bind`
/// and handled by `RepoView`, so the palette and the chip take the same path.
pub fn bind(cx: &mut App) {
    crate::keymap::bind_group(cx, crate::keymap::Group::BranchSwitcher);
}

/// A branch the switcher can check out, as the sidebar sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalBranch {
    pub(crate) name: String,
    /// The branch HEAD is on. It is marked and selecting it is a no-op.
    pub(crate) current: bool,
    /// `↑2 ↓1`, `?`, or nothing when level with or missing the upstream.
    pub(crate) upstream: Option<String>,
    /// Whether the tracked upstream branch was deleted on the remote.
    pub(crate) upstream_gone: bool,
    /// Why the upstream shows a marker, for its tooltip.
    pub(crate) upstream_note: Option<String>,
}

/// A remote-tracking branch with no local branch of the same name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteBranch {
    /// `origin/feature`, as the snapshot lists it.
    pub(crate) name: String,
    /// `feature`: the local branch a checkout creates from it.
    pub(crate) local_name: String,
}

/// The two lists the switcher shows, built once when it opens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BranchChoices {
    pub(crate) local: Vec<LocalBranch>,
    pub(crate) remote: Vec<RemoteBranch>,
}

/// Builds the chooser from the data the sidebar already has.
///
/// Remote-tracking refs whose local branch exists are left out: the sidebar
/// already offers that branch, and a checkout from the remote would fail. The
/// remote's symbolic `origin/HEAD` maps to no local name and is left out too.
pub(crate) fn choices(
    data: &SidebarData,
    upstreams: &HashMap<String, UpstreamInfo>,
) -> BranchChoices {
    let head = data.head_branch.as_deref();
    let mut local: Vec<LocalBranch> = data
        .local
        .iter()
        .map(|name| {
            let upstream = upstreams.get(name);
            LocalBranch {
                name: name.clone(),
                current: head == Some(name.as_str()),
                upstream: upstream.and_then(crate::branches::upstream_label),
                upstream_gone: upstream.is_some_and(|info| info.gone),
                upstream_note: upstream.and_then(crate::branches::upstream_note),
            }
        })
        .collect();
    local.sort_by(|a, b| a.name.cmp(&b.name));

    let mut remote: Vec<RemoteBranch> = data
        .remote
        .iter()
        .filter_map(|name| {
            let local_name = crate::branches::local_name_for_remote(name)?;
            if data.local.iter().any(|branch| branch == local_name) {
                return None;
            }
            Some(RemoteBranch {
                name: name.clone(),
                local_name: local_name.to_owned(),
            })
        })
        .collect();
    remote.sort_by(|a, b| a.name.cmp(&b.name));

    BranchChoices { local, remote }
}

/// A section of the switcher's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Local,
    Remote,
}

impl Section {
    fn title(self) -> String {
        match self {
            Section::Local => t!("switcher.local"),
            Section::Remote => t!("switcher.remote"),
        }
        .to_string()
    }

    fn index(self) -> usize {
        match self {
            Section::Local => 0,
            Section::Remote => 1,
        }
    }
}

/// One line of the switcher's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    Header(Section),
    Local(usize),
    Remote(usize),
    Divider,
    NewBranch,
    /// No branch matches the typed text; the create entry is still offered.
    Empty,
}

/// The list for `query`: a heading for each non-empty section, then the
/// create-branch entry at the bottom.
///
/// The filter is the palette's matcher, so `sw` finds `feature/switch` and a
/// fragment in the middle of a slash-separated name finds it too. The list
/// keeps its alphabetical order while filtering, which is the order a person
/// scans branches in.
pub(crate) fn rows(choices: &BranchChoices, query: &str) -> Vec<Row> {
    let query = query.trim();
    let mut rows = Vec::with_capacity(choices.local.len() + choices.remote.len() + 5);

    let local: Vec<usize> = (0..choices.local.len())
        .filter(|&index| crate::palette::score(query, &choices.local[index].name).is_some())
        .collect();
    if !local.is_empty() {
        rows.push(Row::Header(Section::Local));
        rows.extend(local.into_iter().map(Row::Local));
    }

    let remote: Vec<usize> = (0..choices.remote.len())
        .filter(|&index| crate::palette::score(query, &choices.remote[index].name).is_some())
        .collect();
    if !remote.is_empty() {
        rows.push(Row::Header(Section::Remote));
        rows.extend(remote.into_iter().map(Row::Remote));
    }

    if rows.is_empty() {
        rows.push(Row::Empty);
    }
    rows.push(Row::Divider);
    rows.push(Row::NewBranch);
    rows
}

/// The positions in `rows` the keyboard can land on: everything but the
/// headings, the divider and the empty message.
pub(crate) fn selectable_positions(rows: &[Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| matches!(row, Row::Local(_) | Row::Remote(_) | Row::NewBranch))
        .map(|(index, _)| index)
        .collect()
}

/// The position the highlight starts on: the current branch when it is in the
/// filtered list, otherwise the first entry.
pub(crate) fn initial_selection(choices: &BranchChoices, rows: &[Row]) -> usize {
    let current = choices.local.iter().position(|branch| branch.current);
    let positions = selectable_positions(rows);
    current
        .and_then(|index| {
            positions
                .iter()
                .position(|&position| rows[position] == Row::Local(index))
        })
        .unwrap_or(0)
}

/// Everything the switcher owns between frames.
#[derive(Default)]
pub(crate) struct BranchSwitcher {
    /// Mirrors the popover's open state, which the palette's command and the
    /// binding control.
    open: bool,
    choices: BranchChoices,
    /// The branch badges' pull-request cache, copied when the popover opens.
    pull_requests: BranchPullRequests,
    query: String,
    /// Position in the selectable entries, not in `rows`.
    selected: usize,
    input: Option<Entity<InputState>>,
    scroll: ScrollHandle,
    /// The filter field's change notifications. Dropped with the field.
    _input_events: Option<Subscription>,
}

impl BranchSwitcher {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Opens (or refreshes) the popover, focused on a fresh filter field.
    fn open(&mut self, choices: BranchChoices, window: &mut Window, cx: &mut Context<RepoView>) {
        self.choices = choices;
        self.query.clear();
        self.open = true;
        self.scroll = ScrollHandle::new();
        self.install_input(window, cx);
        let rows = rows(&self.choices, &self.query);
        self.selected = initial_selection(&self.choices, &rows);
        if let Some(position) = selectable_positions(&rows).get(self.selected) {
            self.scroll.scroll_to_item(*position);
        }
    }

    /// Drops the filter field and the state hanging off it.
    fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.input = None;
        self._input_events = None;
    }

    /// Creates the field once per opening, so the text always starts empty and
    /// the focus handle is the one the current popover focuses.
    fn install_input(&mut self, window: &mut Window, cx: &mut Context<RepoView>) {
        if self.input.is_some() {
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("switcher.title").to_string()));
        let events = cx.subscribe(
            &input,
            |this: &mut RepoView, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.branch_switcher.query = this
                        .branch_switcher
                        .input
                        .as_ref()
                        .map(|input| input.read(cx).value().to_string())
                        .unwrap_or_default();
                    // The list narrowed, so the old position points at
                    // something the user is not looking at any more.
                    this.branch_switcher.selected = 0;
                    this.branch_switcher.scroll.scroll_to_item(0);
                    cx.notify();
                }
            },
        );
        let handle = input.read(cx).focus_handle(cx);
        self.input = Some(input);
        self._input_events = Some(events);
        // The popover takes the focus when it opens; the field needs it to
        // receive what the user types.
        window.defer(cx, move |window, cx| handle.focus(window, cx));
    }

    fn step(&mut self, delta: isize) {
        let rows = rows(&self.choices, &self.query);
        let positions = selectable_positions(&rows);
        if positions.is_empty() {
            return;
        }
        let last = positions.len() - 1;
        let next = (self.selected.min(last) as isize + delta).clamp(0, last as isize) as usize;
        self.selected = next;
        self.scroll.scroll_to_item(positions[next]);
    }

    fn selected_row(&self) -> Option<Row> {
        let rows = rows(&self.choices, &self.query);
        let positions = selectable_positions(&rows);
        positions.get(self.selected).map(|&position| rows[position])
    }
}

impl RepoView {
    /// The `Switch Branch…` command and the chip's `Ctrl+B`.
    pub(crate) fn open_branch_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The synthetic repository and the error screen have no branches to
        // switch. A running operation does not hide the list, the way the
        // sidebar keeps its rows visible, but activating one refuses to start
        // a second operation.
        if self.session.is_none() {
            return;
        }
        let choices = self.branch_choices();
        self.branch_switcher.pull_requests = self.pull_requests.branches();
        self.branch_switcher.open(choices, window, cx);
        cx.notify();
    }

    /// The same, from the data the sidebar last published.
    fn branch_choices(&self) -> BranchChoices {
        let Some(source) = self.source() else {
            return BranchChoices::default();
        };
        let data = source.sidebar();
        match &source {
            Source::Repo(snapshot) => choices(&data, &snapshot.upstreams),
            Source::Fake(_) => BranchChoices::default(),
        }
    }

    /// The popover reports its own open state, from a click on the chip, a
    /// click outside it or `Escape` in its card.
    pub(crate) fn branch_switcher_opened(
        &mut self,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if open {
            if self.session.is_none() {
                return;
            }
            let choices = self.branch_choices();
            self.branch_switcher.pull_requests = self.pull_requests.branches();
            self.branch_switcher.open(choices, window, cx);
        } else {
            self.branch_switcher.close();
            self.focus_root(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn step_branch_switcher(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.branch_switcher.step(delta);
        cx.stop_propagation();
        cx.notify();
    }

    /// `Enter` and a click on a row both land here.
    pub(crate) fn activate_branch_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.branch_switcher.selected_row() else {
            return;
        };
        self.activate_branch_switcher_row(row, window, cx);
    }

    /// Switches to the branch `row` names, or opens the create dialog.
    ///
    /// The local checkout is the sidebar's own [`Operation::Checkout`]; a
    /// remote branch goes through the same helper the sidebar's remote rows
    /// use, which creates the local tracking branch. A dirty worktree is
    /// whatever that operation already does about it.
    pub(crate) fn activate_branch_switcher_row(
        &mut self,
        row: Row,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The sidebar keeps its rows visible while an operation runs and
        // refuses the action; the popover does the same rather than closing
        // with nothing to show for the key.
        if self.is_busy() {
            return;
        }
        self.branch_switcher.close();
        match row {
            Row::Local(index) => {
                let Some(branch) = self.branch_switcher.choices.local.get(index) else {
                    return;
                };
                if branch.current {
                    self.focus_root(window, cx);
                    cx.notify();
                    return;
                }
                let name = branch.name.clone();
                let label = t!("branch.op.checkout", branch = name).to_string();
                self.run_operation(Operation::Checkout { branch: name }, label, window, cx);
            }
            Row::Remote(index) => {
                let Some(branch) = self.branch_switcher.choices.remote.get(index) else {
                    return;
                };
                let remote = branch.name.clone();
                let local = branch.local_name.clone();
                crate::branches::checkout_remote(self, &remote, &local, false, window, cx);
            }
            Row::NewBranch => {
                // The create dialog owns the keyboard from here.
                self.create_branch_from_head(window, cx);
                return;
            }
            Row::Header(_) | Row::Divider | Row::Empty => return,
        }
        self.focus_root(window, cx);
        cx.notify();
    }

    pub(crate) fn dismiss_branch_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.branch_switcher.close();
        self.focus_root(window, cx);
        cx.notify();
    }
}

/// Wraps the title bar's chip in the popover that the chip opens.
///
/// `chip` is the [`crate::ui`] button the title bar draws; `Popover` adds the
/// open state, the anchor and the outside-click dismissal, so the chip keeps
/// its own look.
pub(crate) fn chip(view: &RepoView, chip: crate::ui::UiButton, design: &Design) -> AnyElement {
    let open = view.branch_switcher.is_open();
    let entity = view.entity();
    let change_entity = entity.clone();

    let input = view.branch_switcher.input.clone();
    let choices = view.branch_switcher.choices.clone();
    let pull_requests = view.branch_switcher.pull_requests.clone();
    let query = view.branch_switcher.query.clone();
    let selected = view.branch_switcher.selected;
    let scroll = view.branch_switcher.scroll.clone();
    let card_design = design.clone();
    let busy = view.is_busy();

    Popover::new("branch-switcher")
        .anchor(Anchor::TopLeft)
        .offset(px(6.0))
        .open(open)
        .on_open_change(move |open, window, cx| {
            let open = *open;
            change_entity.update(cx, |view, cx| view.branch_switcher_opened(open, window, cx));
        })
        .trigger(chip)
        .content(move |_, window, cx| {
            let Some(input) = input.clone() else {
                return div().into_any_element();
            };
            // The popover moves the keyboard to its own card as it opens. The
            // field has to take it back after the card is in the tree, or the
            // arrows go to the view underneath; a re-render is the moment
            // that is true.
            let focus = input.read(cx).focus_handle(cx);
            if !focus.is_focused(window) {
                crate::ui::focus_input(&input, window, cx);
            }
            card(
                &input,
                &choices,
                &pull_requests,
                &query,
                selected,
                &scroll,
                &card_design,
                &entity,
                busy,
            )
        })
        .into_any_element()
}

/// The popover's contents: the filter field, the sections and the create
/// entry.
#[allow(clippy::too_many_arguments)]
fn card(
    input: &Entity<InputState>,
    choices: &BranchChoices,
    pull_requests: &BranchPullRequests,
    query: &str,
    selected: usize,
    scroll: &ScrollHandle,
    design: &Design,
    entity: &Entity<RepoView>,
    busy: bool,
) -> AnyElement {
    let rows = rows(choices, query);

    let step_up = entity.clone();
    let step_down = entity.clone();
    let confirm_entity = entity.clone();
    let dismiss_entity = entity.clone();
    let card = div()
        .id("branch-switcher-card")
        .flex()
        .flex_col()
        .gap(px(2.0))
        .w(px(WIDTH))
        .key_context(SWITCHER)
        .on_action(move |_: &BranchSwitcherPrevious, _, cx| {
            step_up.update(cx, |view, cx| view.step_branch_switcher(-1, cx));
            cx.stop_propagation();
        })
        .on_action(move |_: &BranchSwitcherNext, _, cx| {
            step_down.update(cx, |view, cx| view.step_branch_switcher(1, cx));
            cx.stop_propagation();
        })
        .on_action(move |_: &BranchSwitcherConfirm, window, cx| {
            confirm_entity.update(cx, |view, cx| view.activate_branch_switcher(window, cx));
            cx.stop_propagation();
        })
        .on_action(move |_: &BranchSwitcherDismiss, window, cx| {
            dismiss_entity.update(cx, |view, cx| view.dismiss_branch_switcher(window, cx));
            cx.stop_propagation();
        })
        .child(Input::new(input).small())
        .child(
            div()
                .px_1()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(if busy {
                    t!("switcher.busy_hint").to_string()
                } else {
                    t!("switcher.hint").to_string()
                }),
        )
        .child(list(
            &rows,
            choices,
            pull_requests,
            selected,
            scroll,
            design,
            entity,
        ));

    card.into_any_element()
}

/// The scrolling list of rows.
fn list(
    rows: &[Row],
    choices: &BranchChoices,
    pull_requests: &BranchPullRequests,
    selected: usize,
    scroll: &ScrollHandle,
    design: &Design,
    entity: &Entity<RepoView>,
) -> AnyElement {
    let mut column = div()
        .id("branch-switcher-list")
        .flex()
        .flex_col()
        .max_h(px(LIST_MAX_HEIGHT))
        .overflow_y_scroll()
        .track_scroll(scroll);

    let mut position = 0usize;
    for row in rows {
        match *row {
            Row::Header(section) => {
                column = column.child(
                    ui::section_header(
                        ("branch-switcher-section", section.index()),
                        section.title(),
                        design,
                    )
                    .h(px(24.0)),
                );
            }
            Row::Divider => {
                column = column.child(div().h(px(1.0)).mx_2p5().my(px(4.0)).bg(design.border));
            }
            Row::Empty => {
                column = column.child(
                    ui::empty_state(t!("switcher.no_match").to_string(), design)
                        .h(px(52.0))
                        .bg(design.panel),
                );
            }
            Row::Local(index) => {
                let highlight = position == selected;
                position += 1;
                column = column.child(local_row(
                    index,
                    &choices.local[index],
                    pull_requests,
                    highlight,
                    design,
                    entity,
                ));
            }
            Row::Remote(index) => {
                let highlight = position == selected;
                position += 1;
                column = column.child(remote_row(
                    index,
                    &choices.remote[index],
                    highlight,
                    design,
                    entity,
                ));
            }
            Row::NewBranch => {
                let highlight = position == selected;
                position += 1;
                column = column.child(new_branch_row(highlight, design, entity));
            }
        }
    }

    column.into_any_element()
}

fn local_row(
    index: usize,
    branch: &LocalBranch,
    pull_requests: &BranchPullRequests,
    highlight: bool,
    design: &Design,
    entity: &Entity<RepoView>,
) -> AnyElement {
    let click_entity = entity.clone();
    let mut row = list_row(
        ("branch-switcher-local", index),
        RowState::NORMAL.selected(highlight).head(branch.current),
        design,
    )
    .w_full()
    .h(px(ROW_HEIGHT))
    .cursor_pointer()
    .on_click(move |_, window, cx| {
        click_entity.update(cx, |view, cx| {
            view.activate_branch_switcher_row(Row::Local(index), window, cx)
        });
    })
    .child(
        div()
            .flex_none()
            .child(style::glyph_icon(style::glyph::BRANCH, 13.0, design.muted)),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .child(branch.name.clone()),
    );

    if branch.upstream_gone {
        let mut marker = div()
            .id(("branch-switcher-upstream-gone", index))
            .flex_none()
            .flex()
            .items_center()
            .child(
                gpui_component::Icon::new(gpui_kit_assets::IconName::Unlink)
                    .with_size(px(12.0))
                    .text_color(design.tag_text),
            );
        if let Some(note) = branch.upstream_note.clone() {
            marker =
                marker.row_tooltip(move |window, cx| Tooltip::new(note.clone()).build(window, cx));
        }
        row = row.child(marker);
    } else if let Some(upstream) = &branch.upstream {
        let mut label = div()
            .id(("branch-switcher-upstream", index))
            .flex_none()
            .text_size(px(design.tiny()))
            .text_color(design.muted)
            .child(upstream.clone());
        if let Some(note) = branch.upstream_note.clone() {
            label =
                label.row_tooltip(move |window, cx| Tooltip::new(note.clone()).build(window, cx));
        }
        row = row.child(label);
    }
    if branch.current {
        row = row.child(status_badge(
            t!("switcher.current").to_string(),
            BadgeTone::Accent,
            design,
        ));
    }
    if let Some(pull_request) = pull_requests.get(&branch.name) {
        row = row.child(crate::pull_requests::badge(
            pull_request,
            pull_requests.note(),
            design,
        ));
    }
    row.into_any_element()
}

fn remote_row(
    index: usize,
    branch: &RemoteBranch,
    highlight: bool,
    design: &Design,
    entity: &Entity<RepoView>,
) -> AnyElement {
    let click_entity = entity.clone();
    list_row(
        ("branch-switcher-remote", index),
        RowState::NORMAL.selected(highlight),
        design,
    )
    .w_full()
    .h(px(ROW_HEIGHT))
    .cursor_pointer()
    .on_click(move |_, window, cx| {
        click_entity.update(cx, |view, cx| {
            view.activate_branch_switcher_row(Row::Remote(index), window, cx)
        });
    })
    .child(
        div()
            .flex_none()
            .child(style::glyph_icon(style::glyph::CLOUD, 13.0, design.muted)),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .child(branch.name.clone()),
    )
    .child(
        div()
            .flex_none()
            .text_size(px(design.tiny()))
            .text_color(design.muted)
            .child(format!("→ {}", branch.local_name)),
    )
    .into_any_element()
}

fn new_branch_row(highlight: bool, design: &Design, entity: &Entity<RepoView>) -> AnyElement {
    let click_entity = entity.clone();
    list_row(
        "branch-switcher-new",
        RowState::NORMAL.selected(highlight),
        design,
    )
    .w_full()
    .h(px(ROW_HEIGHT))
    .cursor_pointer()
    .on_click(move |_, window, cx| {
        click_entity.update(cx, |view, cx| {
            view.activate_branch_switcher_row(Row::NewBranch, window, cx)
        });
    })
    .child(
        div()
            .flex_none()
            .w(px(14.0))
            .text_color(design.muted)
            .child("+"),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .child(t!("switcher.new").to_string()),
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use gpui::{TestAppContext, VisualTestContext};

    use super::*;
    use crate::repo_view::Launch;

    fn upstream(name: &str, ahead: u32, behind: u32) -> UpstreamInfo {
        UpstreamInfo {
            name: name.to_owned(),
            ahead: Some(ahead),
            behind: Some(behind),
            gone: false,
        }
    }

    fn data(local: &[&str], remote: &[&str], head: &str) -> SidebarData {
        SidebarData {
            head_branch: Some(head.to_owned()),
            working_tree: (0, 0),
            local: local.iter().map(|name| (*name).to_owned()).collect(),
            remote: remote.iter().map(|name| (*name).to_owned()).collect(),
            tags: Vec::new(),
            stashes: Vec::new(),
        }
    }

    #[test]
    fn local_branches_keep_their_upstream_and_mark_the_current_one() {
        let data = data(&["main", "feature"], &[], "feature");
        let upstreams = HashMap::from([
            ("main".to_owned(), upstream("origin/main", 2, 1)),
            ("feature".to_owned(), upstream("origin/feature", 0, 0)),
        ]);
        let choices = choices(&data, &upstreams);
        assert_eq!(
            choices.local,
            vec![
                LocalBranch {
                    name: "feature".to_owned(),
                    current: true,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                LocalBranch {
                    name: "main".to_owned(),
                    current: false,
                    upstream: Some("↑2 ↓1".to_owned()),
                    upstream_gone: false,
                    upstream_note: None,
                },
            ]
        );
    }

    #[test]
    fn remote_branches_with_a_local_branch_or_head_are_left_out() {
        let data = data(
            &["main", "feature"],
            &[
                "origin/HEAD",
                "origin/feature",
                "origin/main",
                "origin/topic",
            ],
            "main",
        );
        let choices = choices(&data, &HashMap::new());
        assert_eq!(
            choices
                .remote
                .iter()
                .map(|branch| (branch.name.as_str(), branch.local_name.as_str()))
                .collect::<Vec<_>>(),
            vec![("origin/topic", "topic")],
            "origin/feature is the local feature branch and origin/HEAD is symbolic"
        );
    }

    #[test]
    fn the_empty_query_lists_both_sections_and_the_create_entry() {
        let choices = BranchChoices {
            local: vec![
                LocalBranch {
                    name: "main".to_owned(),
                    current: true,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                LocalBranch {
                    name: "topic".to_owned(),
                    current: false,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
            ],
            remote: vec![RemoteBranch {
                name: "origin/other".to_owned(),
                local_name: "other".to_owned(),
            }],
        };
        assert_eq!(
            rows(&choices, ""),
            vec![
                Row::Header(Section::Local),
                Row::Local(0),
                Row::Local(1),
                Row::Header(Section::Remote),
                Row::Remote(0),
                Row::Divider,
                Row::NewBranch,
            ]
        );
        assert_eq!(
            selectable_positions(&rows(&choices, "")),
            vec![1, 2, 4, 6],
            "headings and the divider are not keyboard targets"
        );
    }

    #[test]
    fn typing_filters_both_sections_by_fuzzy_subsequence() {
        let choices = BranchChoices {
            local: vec![
                LocalBranch {
                    name: "fix/login".to_owned(),
                    current: false,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                LocalBranch {
                    name: "main".to_owned(),
                    current: true,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
            ],
            remote: vec![
                RemoteBranch {
                    name: "origin/feature/login".to_owned(),
                    local_name: "feature/login".to_owned(),
                },
                RemoteBranch {
                    name: "origin/release".to_owned(),
                    local_name: "release".to_owned(),
                },
            ],
        };

        let filtered = rows(&choices, "lgn");
        assert_eq!(
            filtered,
            vec![
                Row::Header(Section::Local),
                Row::Local(0),
                Row::Header(Section::Remote),
                Row::Remote(0),
                Row::Divider,
                Row::NewBranch,
            ],
            "letters in order match across slashes"
        );
        assert_eq!(
            rows(&choices, "nomatch"),
            vec![Row::Empty, Row::Divider, Row::NewBranch],
            "the create entry stays reachable with no match"
        );
        assert_eq!(
            rows(&choices, "MAIN"),
            vec![
                Row::Header(Section::Local),
                Row::Local(1),
                Row::Divider,
                Row::NewBranch,
            ],
            "matching ignores case and drops empty sections"
        );
    }

    #[test]
    fn the_highlight_starts_on_the_current_branch_when_it_is_listed() {
        let choices = BranchChoices {
            local: vec![
                LocalBranch {
                    name: "alpha".to_owned(),
                    current: false,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                LocalBranch {
                    name: "main".to_owned(),
                    current: true,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
            ],
            remote: Vec::new(),
        };
        let all = rows(&choices, "");
        assert_eq!(initial_selection(&choices, &all), 1);
        let filtered = rows(&choices, "alpha");
        assert_eq!(
            initial_selection(&choices, &filtered),
            0,
            "another branch is the first entry when the current one is filtered out"
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test User")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test User")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("run git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The popover takes the keyboard once it opens.
    ///
    /// This is the regression the render-time focus exists for: the popover
    /// moves focus to its own card, so an input focused only once, before the
    /// card is in the tree, leaves the arrows to the commit list underneath.
    #[gpui::test]
    fn the_field_keeps_the_arrows_and_escape_while_the_popover_is_open(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("repo");
        std::fs::create_dir_all(&dir).expect("repo dir");
        git(&dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "hello\n").expect("write README");
        git(&dir, &["add", "README.md"]);
        git(&dir, &["commit", "-m", "initial commit"]);
        git(&dir, &["branch", "topic"]);

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(crate::settings::Settings::default());
            crate::actions::bind(cx);
            bind(cx);
        });

        let window =
            cx.add_window(|window, cx| RepoView::new(Launch::open_quiet(&dir, None), window, cx));
        let view = window.root(cx).expect("the window's root view");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        // The list is the test's own, so the keyboard does not depend on how
        // much of the repository's snapshot the session has loaded.
        let choices = BranchChoices {
            local: vec![
                LocalBranch {
                    name: "main".to_owned(),
                    current: true,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                LocalBranch {
                    name: "topic".to_owned(),
                    current: false,
                    upstream: Some("↑2".to_owned()),
                    upstream_gone: false,
                    upstream_note: None,
                },
            ],
            remote: vec![one_remote()],
        };
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.branch_switcher.open(choices, window, cx);
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();

        let selected =
            |cx: &mut VisualTestContext| cx.update(|_, cx| view.read(cx).branch_switcher.selected);
        assert_eq!(
            selected(&mut cx),
            0,
            "the current branch is highlighted when the popover opens"
        );
        assert!(cx.update(|_, cx| view.read(cx).branch_switcher.is_open()));

        cx.simulate_keystrokes("down");
        assert_eq!(selected(&mut cx), 1, "down moves the highlight");

        cx.simulate_keystrokes("up");
        assert_eq!(selected(&mut cx), 0, "up moves it back");

        cx.simulate_keystrokes("escape");
        assert!(
            !cx.update(|_, cx| view.read(cx).branch_switcher.is_open()),
            "escape closes the popover"
        );
    }

    fn one_remote() -> RemoteBranch {
        RemoteBranch {
            name: "origin/other".to_owned(),
            local_name: "other".to_owned(),
        }
    }
}
