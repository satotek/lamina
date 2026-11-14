//! Pull-request status on branches, and the "Check out Pull Request…" dialog.
//!
//! One `gh pr list` call fills a per-repository cache, refreshed when the
//! repository opens, after a successful fetch or push, and on demand. The
//! branch rows, the branch switcher and the title-bar chip read the cache and
//! never run anything themselves: the CLI call is a background task, so a slow
//! network never stalls a frame.
//!
//! Everything here is behind the existing gh setting and requires a GitHub
//! remote `gh` is logged in to. When any of that is missing the cache stays
//! empty and the badges simply do not appear: an optional integration must not
//! turn a repository with no pull requests into an error screen.
//!
//! Matching is deliberately conservative. A pull request shows on a branch
//! when the branch's upstream (or the branch itself when it has none) is the
//! pull request's head branch *and* the owner of that remote is the head
//! owner. That second half is what keeps a fork's pull request off a same-named
//! branch in the upstream repository.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, actions, div,
    px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::input::{Input, InputEvent, InputState};
use strata_core::UpstreamInfo;
use strata_core::forge::{ChecksState, PullRequestState, PullRequestSummary, ReviewState};
use strata_core::tools::GitHubRemote;

use crate::data::Source;
use crate::repo_view::RepoView;
use crate::settings::Settings;
use crate::style::Design;
use crate::tools::Tooling;
use crate::ui::{self, BadgeTone, RowState};

/// How many pull requests the checkout dialog's list shows at once.
const DIALOG_WIDTH: f32 = 520.0;
/// The list scrolls once it would push the dialog past this height.
const LIST_MAX_HEIGHT: f32 = 360.0;

actions!(
    strata,
    [
        /// Asks `gh` for the repository's pull requests again.
        RefreshPullRequests,
        /// Opens the dialog that checks out one of the cached pull requests.
        CheckoutPullRequest,
    ]
);

/// The whole pull-request cache of one repository view.
///
/// The cache belongs to the view rather than to the app: two tabs can show two
/// checkouts of the same repository, and a failed refresh of one must not
/// blank the badges of the other.
pub(crate) struct PullRequests {
    items: Vec<PullRequestSummary>,
    /// The pull request chosen for each local branch, rebuilt whenever the
    /// cache or the branch set changes.
    by_branch: Rc<HashMap<String, PullRequestSummary>>,
    /// Why the last refresh failed. Kept beside the old items, which stay on
    /// screen and mention this reason in their tooltip.
    error: Option<String>,
    refreshing: bool,
    checkout_running: bool,
    /// The one automatic refresh has started, so opening the view does not ask
    /// again every frame.
    started: bool,
    /// Whether the gh setting was on last time the settings changed, so
    /// turning it off clears the badges and turning it back on asks again.
    enabled: bool,
    /// Incremented per request; a slower answer from an older request must not
    /// replace a newer one.
    generation: u64,
}

impl Default for PullRequests {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            by_branch: Rc::new(HashMap::new()),
            error: None,
            refreshing: false,
            checkout_running: false,
            started: false,
            enabled: true,
            generation: 0,
        }
    }
}

impl PullRequests {
    /// The badge lookup a sidebar or switcher frame draws from.
    pub(crate) fn branches(&self) -> BranchPullRequests {
        BranchPullRequests {
            by_branch: Rc::clone(&self.by_branch),
            note: self.error.clone().map(Into::into),
        }
    }

    pub(crate) fn for_branch(&self, branch: &str) -> Option<&PullRequestSummary> {
        self.by_branch.get(branch)
    }

    pub(crate) fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    pub(crate) fn is_checking_out(&self) -> bool {
        self.checkout_running
    }

    /// The cache's open pull requests, newest first, for the checkout dialog.
    pub(crate) fn open_items(&self) -> Vec<PullRequestSummary> {
        let mut items: Vec<PullRequestSummary> = self
            .items
            .iter()
            .filter(|pull_request| pull_request.state.is_open())
            .cloned()
            .collect();
        items.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.number.cmp(&a.number))
        });
        items
    }

    fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.refreshing = true;
        self.generation
    }

    fn finish(&mut self, generation: u64, result: Result<Vec<PullRequestSummary>, String>) {
        if generation != self.generation {
            return;
        }
        self.refreshing = false;
        match result {
            Ok(items) => {
                self.items = items;
                self.error = None;
            }
            // The old items stay: a blip in the network should not erase a
            // badge the user was looking at.
            Err(error) => self.error = Some(error),
        }
    }

    fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.items.clear();
        self.by_branch = Rc::new(HashMap::new());
        self.error = None;
        self.refreshing = false;
    }

    fn set_by_branch(&mut self, map: HashMap<String, PullRequestSummary>) {
        self.by_branch = Rc::new(map);
    }
}

/// A cheap, owned snapshot of the branch-to-pull-request map for one frame.
///
/// The sidebar's list is built once and drawn many times per frame; sharing the
/// map through an `Rc` keeps a row from cloning anything.
#[derive(Clone, Default)]
pub(crate) struct BranchPullRequests {
    by_branch: Rc<HashMap<String, PullRequestSummary>>,
    note: Option<Rc<str>>,
}

impl BranchPullRequests {
    pub(crate) fn get(&self, branch: &str) -> Option<&PullRequestSummary> {
        self.by_branch.get(branch)
    }

    /// Why the last refresh failed, for the badge's tooltip.
    pub(crate) fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }
}

/// The pull request to show on one branch.
///
/// `head_branch` is the branch as the host names it, and `owner` the owner of
/// the remote that branch lives on. A pull request from a fork has a different
/// head owner, so it only matches when the branch's own remote is that fork.
pub(crate) fn select_for_branch<'a>(
    items: &'a [PullRequestSummary],
    head_branch: &str,
    owner: Option<&str>,
) -> Option<&'a PullRequestSummary> {
    let owner = owner?;
    let matches: Vec<&PullRequestSummary> = items
        .iter()
        .filter(|pull_request| pull_request.head_branch == head_branch)
        .filter(|pull_request| {
            pull_request
                .head_owner
                .as_deref()
                .is_some_and(|head_owner| head_owner.eq_ignore_ascii_case(owner))
        })
        .collect();

    let newest = |candidates: Vec<&'a PullRequestSummary>| {
        candidates.into_iter().max_by(|a, b| {
            a.updated_at
                .cmp(&b.updated_at)
                .then_with(|| a.number.cmp(&b.number))
        })
    };
    // A branch can carry an open pull request and an older merged one from a
    // previous attempt; the active one is what the row is about.
    newest(
        matches
            .iter()
            .copied()
            .filter(|pr| pr.state.is_open())
            .collect(),
    )
    .or_else(|| newest(matches))
}

/// Chooses the pull request for every local branch of one snapshot.
///
/// A branch with an upstream is matched by the upstream's remote and branch
/// name; a branch without one is matched against the primary remote's owner
/// and its own name.
pub(crate) fn branch_map(
    items: &[PullRequestSummary],
    branches: &[String],
    upstreams: &HashMap<String, UpstreamInfo>,
    remotes: &[strata_core::RemoteInfo],
    known_hosts: &[String],
    primary_owner: Option<&str>,
) -> HashMap<String, PullRequestSummary> {
    let mut map = HashMap::with_capacity(branches.len());
    for branch in branches {
        let (head_branch, owner) = match upstreams.get(branch) {
            Some(upstream) if !upstream.gone => match upstream.name.split_once('/') {
                Some((remote_name, head_branch)) => (
                    head_branch,
                    owner_of_remote(remotes, remote_name, known_hosts),
                ),
                None => (branch.as_str(), primary_owner.map(str::to_owned)),
            },
            _ => (branch.as_str(), primary_owner.map(str::to_owned)),
        };
        if let Some(pull_request) = select_for_branch(items, head_branch, owner.as_deref()) {
            map.insert(branch.clone(), pull_request.clone());
        }
    }
    map
}

fn owner_of_remote(
    remotes: &[strata_core::RemoteInfo],
    remote_name: &str,
    known_hosts: &[String],
) -> Option<String> {
    let remote = remotes.iter().find(|remote| remote.name == remote_name)?;
    [&remote.fetch_url, &remote.push_url]
        .into_iter()
        .flatten()
        .find_map(|url| GitHubRemote::parse(url, known_hosts))
        .map(|remote| remote.owner)
}

/// Whether the working tree would keep `gh pr checkout` from starting.
///
/// A checkout can only be undone by the user, so a tracked change refuses it
/// up front with a reason they can act on. Untracked files are left to git:
/// it refuses only the ones it would have to overwrite.
pub(crate) fn worktree_blocks_checkout(status: &strata_core::WorktreeStatus) -> bool {
    status
        .staged
        .iter()
        .chain(status.unstaged.iter())
        .any(|change| !matches!(change.kind, strata_core::ChangeKind::Untracked))
}

/// The badge tint for a pull request's state.
pub(crate) fn badge_tone(state: PullRequestState) -> BadgeTone {
    match state {
        PullRequestState::Open => BadgeTone::Accent,
        PullRequestState::Draft => BadgeTone::Neutral,
        PullRequestState::Merged => BadgeTone::Warning,
        PullRequestState::Closed => BadgeTone::Danger,
    }
}

/// The badge's text: the number the branch's pull request is known by.
pub(crate) fn badge_label(pull_request: &PullRequestSummary) -> String {
    format!("#{}", pull_request.number)
}

/// The badge's tooltip: what the pull request is, its review and its checks.
pub(crate) fn badge_tooltip(pull_request: &PullRequestSummary, note: Option<&str>) -> String {
    let state = match pull_request.state {
        PullRequestState::Open => "Open",
        PullRequestState::Draft => "Draft",
        PullRequestState::Merged => "Merged",
        PullRequestState::Closed => "Closed",
    };
    let review = match pull_request.review {
        ReviewState::Approved => "Approved",
        ReviewState::ChangesRequested => "Changes requested",
        ReviewState::ReviewRequired => "Review required",
        ReviewState::None => "No review",
    };
    let checks = match pull_request.checks.state {
        ChecksState::None => "No checks".to_owned(),
        _ => format!(
            "Checks: {}/{} passing",
            pull_request.checks.passing,
            pull_request.checks.total()
        ),
    };
    let mut tooltip = format!("{}\n{state} · {review}\n{checks}", pull_request.title);
    if let Some(note) = note {
        tooltip.push_str("\nLast refresh failed: ");
        tooltip.push_str(note);
    }
    tooltip
}

/// A compact `#123` badge with its check dot, for a branch row or chip.
pub(crate) fn badge(
    pull_request: &PullRequestSummary,
    note: Option<&str>,
    design: &Design,
) -> AnyElement {
    let mut cell = div()
        .id(("pull-request-badge", pull_request.number))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(3.0));
    if let Some(dot) = ui::check_dot(pull_request.checks.state, design) {
        cell = cell.child(dot);
    }
    cell.child(ui::status_badge(
        badge_label(pull_request),
        badge_tone(pull_request.state),
        design,
    ))
    .tooltip({
        let tooltip = SharedString::from(badge_tooltip(pull_request, note));
        move |window, cx| gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
    })
    .into_any_element()
}

impl RepoView {
    /// The pull request shown on `branch`, if the cache has one.
    pub(crate) fn pull_request_for_branch(&self, branch: &str) -> Option<&PullRequestSummary> {
        self.pull_requests.for_branch(branch)
    }

    /// Starts the one refresh a freshly opened repository gets.
    ///
    /// This runs from the first frames rather than from the constructor so it
    /// can wait for tool detection: a view opened while `gh` is still being
    /// discovered would otherwise record "no gh" for the rest of the session.
    pub(crate) fn start_pull_request_refresh(&mut self, cx: &mut Context<Self>) {
        if self.pull_requests.started || self.session.is_none() {
            return;
        }
        let Some(tooling) = Tooling::get(cx) else {
            return;
        };
        if tooling.detecting() {
            return;
        }
        self.pull_requests.started = true;
        self.refresh_pull_requests(cx);
    }

    /// Rebuilds the branch map after the cache or the branches changed.
    pub(crate) fn recompute_pull_request_map(&mut self, cx: &App) {
        let Some(source) = self.source() else {
            self.pull_requests.set_by_branch(HashMap::new());
            return;
        };
        let Source::Repo(snapshot) = &source else {
            self.pull_requests.set_by_branch(HashMap::new());
            return;
        };
        let branches = source.sidebar().local;
        let known_hosts = Tooling::get(cx)
            .and_then(Tooling::gh)
            .map(|gh| gh.hosts.clone())
            .unwrap_or_default();
        let primary_owner = crate::github::remote_for(self, cx).map(|remote| remote.owner);
        let map = branch_map(
            &self.pull_requests.items,
            &branches,
            &snapshot.upstreams,
            &snapshot.remotes,
            &known_hosts,
            primary_owner.as_deref(),
        );
        self.pull_requests.set_by_branch(map);
    }

    /// Asks `gh` for the repository's pull requests, off the UI thread.
    ///
    /// A missing program, a logged-out account, a non-GitHub remote or the
    /// setting being off all leave the cache empty quietly: the badges are an
    /// optional convenience, not a prerequisite for the repository.
    pub(crate) fn refresh_pull_requests(&mut self, cx: &mut Context<Self>) {
        let Some(remote) = crate::github::remote_for(self, cx) else {
            self.pull_requests.clear();
            self.recompute_pull_request_map(cx);
            cx.notify();
            return;
        };
        let Ok(gh) = crate::github::gh_for(&remote.host, cx) else {
            self.pull_requests.clear();
            self.recompute_pull_request_map(cx);
            cx.notify();
            return;
        };
        let Some(workdir) = self
            .session
            .as_ref()
            .map(|session| session.git().workdir().to_path_buf())
        else {
            return;
        };

        let generation = self.pull_requests.begin();
        self.notify_if_active(cx);
        let task = cx.background_executor().spawn(async move {
            strata_core::forge::pull_requests_for_branches_with(&gh, &workdir)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.pull_requests.finish(generation, result);
                view.recompute_pull_request_map(cx);
                view.notify_if_active(cx);
            });
        })
        .detach();
    }

    /// The "Refresh Pull Requests" command. Unlike the automatic refresh, this
    /// one says why it could not run.
    pub(crate) fn refresh_pull_requests_command(&mut self, cx: &mut Context<Self>) {
        let Some(remote) = crate::github::remote_for(self, cx) else {
            self.status_note = Some("Pull requests need a GitHub remote.".to_owned());
            cx.notify();
            return;
        };
        if let Err(reason) = crate::github::gh_for(&remote.host, cx) {
            self.status_note = Some(format!("Pull requests: {reason}"));
            cx.notify();
            return;
        }
        if self.pull_requests.is_refreshing() {
            return;
        }
        self.pull_requests.started = true;
        self.refresh_pull_requests(cx);
    }

    /// Reacts to the tools setting changing.
    pub(crate) fn pull_requests_setting_changed(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.pull_requests.enabled == enabled {
            return;
        }
        self.pull_requests.enabled = enabled;
        // Either direction gets a fresh chance: turning the integration off
        // removes the badges, turning it on asks again.
        self.pull_requests.started = false;
        if !enabled {
            self.pull_requests.clear();
        }
        self.recompute_pull_request_map(cx);
        cx.notify();
    }

    /// Opens the dialog listing the cache's open pull requests.
    pub(crate) fn open_checkout_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() || self.pull_requests.checkout_running {
            self.status_note = Some("An operation is already running.".to_owned());
            cx.notify();
            return;
        }
        let items = self.pull_requests.open_items();
        if items.is_empty() {
            self.status_note = Some(if self.pull_requests.is_refreshing() {
                "Refreshing pull requests…".to_owned()
            } else {
                "No open pull requests in the cache; refreshing them now.".to_owned()
            });
            self.pull_requests.started = true;
            self.refresh_pull_requests(cx);
            cx.notify();
            return;
        }

        let repo = self.entity.clone();
        let dialog = cx.new(|cx| CheckoutDialog::new(items, repo, window, cx));
        let focus = dialog.read(cx).focus.clone();
        window.open_dialog(cx, move |card, _, _| {
            let content = dialog.clone();
            card.title("Check out Pull Request")
                .w(px(DIALOG_WIDTH))
                .close_button(false)
                .on_ok(|_, _, _| false)
                .content(move |body, _, _| body.child(content.clone()))
        });
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    /// Checks out pull request `number` through `gh`, as a tracked operation
    /// whose outcome lands in the status line.
    pub(crate) fn checkout_pull_request(&mut self, number: u64, cx: &mut Context<Self>) {
        if self.pull_requests.checkout_running {
            return;
        }
        if self.is_busy() {
            self.status_note = Some("An operation is already running.".to_owned());
            cx.notify();
            return;
        }
        let Some(remote) = crate::github::remote_for(self, cx) else {
            self.status_note = Some("Pull requests need a GitHub remote.".to_owned());
            cx.notify();
            return;
        };
        let Ok(gh) = crate::github::gh_for(&remote.host, cx) else {
            self.status_note = Some(format!(
                "Check out pull request #{number}: gh is unavailable"
            ));
            cx.notify();
            return;
        };
        let Some(workdir) = self
            .session
            .as_ref()
            .map(|session| session.git().workdir().to_path_buf())
        else {
            return;
        };
        // `gh pr checkout` is `git fetch` plus a checkout: with local changes
        // it can fail halfway through the files git touches, so the refusal
        // here is the one case the user can act on before anything moves.
        if let Some(session) = &self.session
            && worktree_blocks_checkout(&session.snapshot().status)
        {
            self.status_note = Some(format!(
                "Check out pull request #{number}: commit or stash your changes first."
            ));
            cx.notify();
            return;
        }

        self.pull_requests.checkout_running = true;
        self.status_error = None;
        self.status_note = Some(format!("Checking out pull request #{number}…"));
        cx.notify();

        let task = cx.background_executor().spawn(async move {
            strata_core::forge::checkout_pull_request_with(&gh, &workdir, number)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.pull_requests.checkout_running = false;
                match result {
                    Ok(()) => {
                        view.status_note = Some(format!("Checked out pull request #{number}"));
                        view.pull_requests.started = true;
                        view.refresh_pull_requests(cx);
                    }
                    Err(error) => {
                        view.status_error =
                            Some(format!("Check out pull request #{number}: {error}"));
                    }
                }
                view.notify_if_active(cx);
            });
        })
        .detach();
    }
}

/// The modal that lists the cached open pull requests.
struct CheckoutDialog {
    /// Its own handle, for the row click handlers.
    entity: Entity<Self>,
    focus: FocusHandle,
    input: Entity<InputState>,
    pull_requests: Vec<PullRequestSummary>,
    repo: WeakEntity<RepoView>,
    selected: usize,
    _input_events: Subscription,
}

impl CheckoutDialog {
    fn new(
        pull_requests: Vec<PullRequestSummary>,
        repo: WeakEntity<RepoView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter pull requests"));
        let focus = input.focus_handle(cx);
        let _input_events =
            cx.subscribe(&input, |this, _input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    this.selected = 0;
                    cx.notify();
                }
                // The field owns Enter while it has focus; running from that
                // event keeps one Enter, one checkout.
                InputEvent::PressEnter {
                    secondary: false, ..
                } => {
                    if let Some(handle) = cx.active_window() {
                        let position = this.selected;
                        let _ = handle.update(cx, |_, window, cx| this.run(position, window, cx));
                    }
                }
                _ => {}
            });
        Self {
            entity: cx.entity(),
            focus,
            input,
            pull_requests,
            repo,
            selected: 0,
            _input_events,
        }
    }

    /// Indices into `pull_requests` for the query, best match first.
    fn filtered(&self, cx: &App) -> Vec<usize> {
        let query = self.input.read(cx).value().to_string();
        let query = query.trim();
        if query.is_empty() {
            return (0..self.pull_requests.len()).collect();
        }
        let mut rows: Vec<(usize, i32)> = self
            .pull_requests
            .iter()
            .enumerate()
            .filter_map(|(index, pull_request)| {
                let label = format!(
                    "#{} {} {}",
                    pull_request.number, pull_request.title, pull_request.head_branch
                );
                crate::palette::score(query, &label).map(|score| (index, score))
            })
            .collect();
        rows.sort_by_key(|&(index, score)| (std::cmp::Reverse(score), index));
        rows.into_iter().map(|(index, _)| index).collect()
    }

    fn run(&mut self, position: usize, window: &mut Window, cx: &mut App) {
        let Some(&index) = self.filtered(cx).get(position) else {
            return;
        };
        let Some(pull_request) = self.pull_requests.get(index) else {
            return;
        };
        let number = pull_request.number;
        let repo = self.repo.clone();
        window.close_dialog(cx);
        let _ = repo.update(cx, |view, cx| {
            view.checkout_pull_request(number, cx);
        });
    }

    fn row(&self, position: usize, index: usize, design: &Design) -> AnyElement {
        let pull_request = &self.pull_requests[index];
        let entity = self.entity.clone();
        let number = pull_request.number;
        let mut row = ui::list_row(
            ("checkout-pull-request", position),
            RowState::NORMAL.selected(position == self.selected),
            design,
        )
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            entity.update(cx, |dialog, cx| dialog.run(position, window, cx));
        })
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(format!("#{number}")),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(pull_request.title.clone()),
        )
        .child(
            div()
                .flex_none()
                .max_w(px(140.0))
                .truncate()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(pull_request.head_branch.clone()),
        );
        if let Some(dot) = ui::check_dot(pull_request.checks.state, design) {
            row = row.child(dot);
        }
        row.into_any_element()
    }
}

impl Render for CheckoutDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let filtered = self.filtered(cx);
        let mut list = div()
            .id("checkout-pull-request-list")
            .flex()
            .flex_col()
            .w_full()
            .max_h(px(LIST_MAX_HEIGHT))
            .overflow_y_scroll();
        if filtered.is_empty() {
            list = list.child(ui::empty_state("No pull request matches.", &design));
        } else {
            list = list.children(
                filtered
                    .iter()
                    .enumerate()
                    .map(|(position, &index)| self.row(position, index, &design)),
            );
        }

        div()
            .id("checkout-pull-request-dialog")
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(Input::new(&self.input))
            .child(
                div()
                    .px_1()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(format!(
                        "Enter checks out the first match · {} open",
                        self.pull_requests.len()
                    )),
            )
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::forge::PullRequestChecks;
    use strata_core::{RemoteInfo, UpstreamInfo};

    fn pull_request(
        number: u64,
        state: PullRequestState,
        head: &str,
        owner: &str,
    ) -> PullRequestSummary {
        PullRequestSummary {
            number,
            title: format!("Pull request {number}"),
            state,
            head_branch: head.to_owned(),
            head_owner: Some(owner.to_owned()),
            base: "main".to_owned(),
            url: format!("https://github.com/me/strata/pull/{number}"),
            review: ReviewState::Approved,
            checks: PullRequestChecks {
                state: ChecksState::Passing,
                passing: 4,
                failing: 0,
                pending: 0,
            },
            updated_at: format!("2026-01-0{number}T10:00:00Z"),
        }
    }

    fn upstream(name: &str) -> UpstreamInfo {
        UpstreamInfo {
            name: name.to_owned(),
            ahead: Some(0),
            behind: Some(0),
            gone: false,
        }
    }

    #[test]
    fn a_branch_matches_its_own_pull_request() {
        let items = vec![
            pull_request(4, PullRequestState::Open, "feature", "me"),
            pull_request(5, PullRequestState::Open, "other", "me"),
        ];
        let found = select_for_branch(&items, "feature", Some("me")).unwrap();
        assert_eq!(found.number, 4);
        assert!(select_for_branch(&items, "feature", Some("someone-else")).is_none());
        assert!(select_for_branch(&items, "feature", None).is_none());
        assert!(select_for_branch(&items, "missing", Some("me")).is_none());
    }

    #[test]
    fn owner_matching_ignores_case() {
        let items = vec![pull_request(4, PullRequestState::Open, "feature", "Me")];
        assert_eq!(
            select_for_branch(&items, "feature", Some("me"))
                .unwrap()
                .number,
            4
        );
    }

    #[test]
    fn an_open_pull_request_wins_over_a_newer_closed_one() {
        let mut older_open = pull_request(4, PullRequestState::Open, "feature", "me");
        older_open.updated_at = "2026-01-01T10:00:00Z".into();
        let mut newer_merged = pull_request(9, PullRequestState::Merged, "feature", "me");
        newer_merged.updated_at = "2026-02-01T10:00:00Z".into();
        let items = vec![newer_merged, older_open];
        assert_eq!(
            select_for_branch(&items, "feature", Some("me"))
                .unwrap()
                .number,
            4,
            "the active pull request is what the branch row is about"
        );
    }

    #[test]
    fn without_an_open_one_the_most_recently_updated_wins() {
        let mut first = pull_request(4, PullRequestState::Merged, "feature", "me");
        first.updated_at = "2026-01-01T10:00:00Z".into();
        let mut second = pull_request(9, PullRequestState::Closed, "feature", "me");
        second.updated_at = "2026-03-01T10:00:00Z".into();
        let items = vec![first, second];
        assert_eq!(
            select_for_branch(&items, "feature", Some("me"))
                .unwrap()
                .number,
            9
        );
    }

    #[test]
    fn a_draft_counts_as_open() {
        let draft = pull_request(4, PullRequestState::Draft, "feature", "me");
        let newer_merged = pull_request(9, PullRequestState::Merged, "feature", "me");
        let items = vec![newer_merged, draft];
        assert_eq!(
            select_for_branch(&items, "feature", Some("me"))
                .unwrap()
                .number,
            4
        );
    }

    #[test]
    fn a_fork_pull_request_only_matches_when_the_upstream_is_that_fork() {
        let items = vec![pull_request(
            7,
            PullRequestState::Open,
            "fix",
            "contributor",
        )];
        let remotes = vec![
            RemoteInfo {
                name: "origin".into(),
                fetch_url: Some("https://github.com/me/strata.git".into()),
                push_url: None,
            },
            RemoteInfo {
                name: "fork".into(),
                fetch_url: Some("git@github.com:contributor/strata.git".into()),
                push_url: None,
            },
        ];
        let known = vec!["github.com".to_owned()];

        let upstreams = HashMap::from([("fix".to_owned(), upstream("fork/fix"))]);
        let map = branch_map(
            &items,
            &["fix".to_owned()],
            &upstreams,
            &remotes,
            &known,
            Some("me"),
        );
        assert_eq!(map.get("fix").unwrap().number, 7);

        let upstreams = HashMap::from([("fix".to_owned(), upstream("origin/fix"))]);
        let map = branch_map(
            &items,
            &["fix".to_owned()],
            &upstreams,
            &remotes,
            &known,
            Some("me"),
        );
        assert!(
            map.is_empty(),
            "the upstream remote's owner does not own the head branch"
        );
    }

    #[test]
    fn a_branch_without_an_upstream_uses_its_own_name_and_the_primary_owner() {
        let items = vec![pull_request(3, PullRequestState::Open, "topic", "me")];
        let remotes = vec![RemoteInfo {
            name: "origin".into(),
            fetch_url: Some("https://github.com/me/strata.git".into()),
            push_url: None,
        }];
        let map = branch_map(
            &items,
            &["topic".to_owned(), "untracked".to_owned()],
            &HashMap::new(),
            &remotes,
            &["github.com".to_owned()],
            Some("me"),
        );
        assert_eq!(map.get("topic").unwrap().number, 3);
        assert!(!map.contains_key("untracked"));
    }

    #[test]
    fn a_gone_upstream_falls_back_to_the_branch_name() {
        let items = vec![pull_request(3, PullRequestState::Open, "topic", "me")];
        let mut gone = upstream("origin/topic");
        gone.gone = true;
        let upstreams = HashMap::from([("topic".to_owned(), gone)]);
        let map = branch_map(
            &items,
            &["topic".to_owned()],
            &upstreams,
            &[],
            &[],
            Some("me"),
        );
        assert_eq!(map.get("topic").unwrap().number, 3);
    }

    #[test]
    fn a_dirty_tracked_file_blocks_a_checkout_but_untracked_files_do_not() {
        use strata_core::{ChangeKind, FileChange, WorktreeStatus};

        let change = |kind| FileChange {
            path: "src/lib.rs".into(),
            old_path: None,
            kind,
        };
        assert!(!worktree_blocks_checkout(&WorktreeStatus::default()));
        assert!(
            !worktree_blocks_checkout(&WorktreeStatus {
                staged: Vec::new(),
                unstaged: vec![change(ChangeKind::Untracked)],
            }),
            "git itself refuses only the untracked files it would overwrite"
        );
        assert!(worktree_blocks_checkout(&WorktreeStatus {
            staged: Vec::new(),
            unstaged: vec![change(ChangeKind::Modified)],
        }));
        assert!(worktree_blocks_checkout(&WorktreeStatus {
            staged: vec![change(ChangeKind::Added)],
            unstaged: Vec::new(),
        }));
    }

    #[test]
    fn the_badge_label_is_the_number() {
        let pull_request = pull_request(123, PullRequestState::Open, "feature", "me");
        assert_eq!(badge_label(&pull_request), "#123");
    }

    #[test]
    fn every_state_gets_its_own_tint() {
        assert_eq!(badge_tone(PullRequestState::Open), BadgeTone::Accent);
        assert_eq!(badge_tone(PullRequestState::Draft), BadgeTone::Neutral);
        assert_eq!(badge_tone(PullRequestState::Merged), BadgeTone::Warning);
        assert_eq!(badge_tone(PullRequestState::Closed), BadgeTone::Danger);
    }

    #[test]
    fn the_tooltip_names_the_pull_request_review_and_checks() {
        let mut pull_request = pull_request(12, PullRequestState::Open, "feature", "me");
        pull_request.title = "Add the forge layer".into();
        assert_eq!(
            badge_tooltip(&pull_request, None),
            "Add the forge layer\nOpen · Approved\nChecks: 4/4 passing"
        );

        pull_request.checks.pending = 1;
        assert_eq!(
            badge_tooltip(&pull_request, None),
            "Add the forge layer\nOpen · Approved\nChecks: 4/5 passing"
        );

        pull_request.review = ReviewState::ChangesRequested;
        pull_request.state = PullRequestState::Draft;
        pull_request.checks = PullRequestChecks::default();
        assert_eq!(
            badge_tooltip(&pull_request, Some("gh did not answer within 15 seconds")),
            "Add the forge layer\nDraft · Changes requested\nNo checks\n\
             Last refresh failed: gh did not answer within 15 seconds"
        );
    }

    #[test]
    fn a_merged_or_closed_tooltip_says_so() {
        let merged = pull_request(1, PullRequestState::Merged, "feature", "me");
        assert!(badge_tooltip(&merged, None).contains("Merged"));
        let closed = pull_request(2, PullRequestState::Closed, "feature", "me");
        assert!(badge_tooltip(&closed, None).contains("Closed"));
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_items_and_names_the_reason() {
        let mut cache = PullRequests::default();
        let generation = cache.begin();
        cache.finish(
            generation,
            Ok(vec![pull_request(1, PullRequestState::Open, "main", "me")]),
        );
        assert_eq!(cache.items.len(), 1);
        assert_eq!(cache.branches().note(), None);

        let generation = cache.begin();
        cache.finish(
            generation,
            Err("gh did not answer within 15 seconds".into()),
        );
        assert_eq!(cache.items.len(), 1, "a network blip keeps the badge");
        assert_eq!(
            cache.branches().note(),
            Some("gh did not answer within 15 seconds")
        );
        assert!(!cache.is_refreshing());
    }

    #[test]
    fn an_older_answer_cannot_replace_a_newer_one() {
        let mut cache = PullRequests::default();
        let older = cache.begin();
        let newer = cache.begin();
        cache.finish(
            newer,
            Ok(vec![pull_request(2, PullRequestState::Open, "main", "me")]),
        );
        cache.finish(
            older,
            Ok(vec![pull_request(1, PullRequestState::Open, "main", "me")]),
        );
        assert_eq!(cache.items[0].number, 2);
    }

    #[test]
    fn the_checkout_dialog_lists_open_pull_requests_newest_first() {
        let mut cache = PullRequests::default();
        let mut merged = pull_request(1, PullRequestState::Merged, "old", "me");
        merged.updated_at = "2026-05-01T10:00:00Z".into();
        let mut draft = pull_request(2, PullRequestState::Draft, "wip", "me");
        draft.updated_at = "2026-04-01T10:00:00Z".into();
        let mut open = pull_request(3, PullRequestState::Open, "ready", "me");
        open.updated_at = "2026-03-01T10:00:00Z".into();
        let generation = cache.begin();
        cache.finish(generation, Ok(vec![merged, draft, open]));

        let numbers: Vec<u64> = cache
            .open_items()
            .into_iter()
            .map(|pull_request| pull_request.number)
            .collect();
        assert_eq!(numbers, vec![2, 3], "merged ones are not offered");
    }
}
