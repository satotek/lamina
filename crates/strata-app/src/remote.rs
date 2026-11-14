//! Fetch, pull and push from the title bar.
//!
//! Nothing here runs git. Each command becomes an [`Operation`] on the window's
//! session, whose operations worker runs it and reports progress and outcome
//! through [`SessionEvent`]s; `RepoView::on_event` forwards those to [`Remote`].
//! This module owns only the UI state around them: which of its operations is
//! running, the progress the status bar shows, and the two decisions that need
//! a dialog (which remote to publish to, and whether to force a rejected push).
//!
//! Authentication is git's business: user credential helpers run first, and
//! when none answers, the session's askpass bridge asks the window for a
//! username, password or passphrase (`GIT_TERMINAL_PROMPT=0` still keeps git
//! off a terminal nobody can see). A failure after that is classified here and
//! reported with a hint about credential helpers and ssh-agent.

use std::collections::BTreeSet;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, IntoElement, Window, actions, div, px,
    relative,
};
use gpui_component::dialog::DialogFooter;
use gpui_component::notification::Notification;
use gpui_component::{Icon, Sizable as _, WindowExt as _};
use gpui_kit_assets::IconName;
use strata_core::session::{Operation, OperationId, RepoSnapshot};
use strata_core::{HeadInfo, RefInfo, RefKind, UpstreamInfo};

use crate::data::Source;
use crate::repo_view::RepoView;

/// Width of the slim progress bar in the status bar.
const PROGRESS_WIDTH: f32 = 90.0;

/// Width of a remote button with its icon and label. The title bar counts it
/// when it works out how much room the search field has left.
pub(crate) const LABELLED_BUTTON_WIDTH: f32 = 52.0;

/// The same button as an icon alone, with the label in the tooltip. The
/// narrowest title bar uses this form.
pub(crate) const ICON_BUTTON_WIDTH: f32 = 32.0;

const DETACHED_HEAD: &str = "Detached HEAD: check out a branch to pull or push.";
const BUSY: &str = "An operation is already running.";
const NO_SESSION: &str =
    "Remote operations need an open repository; `--fake` renders synthetic data.";

/// Shown with an authentication failure. Strata did ask for credentials, so
/// the fix is either correcting them or letting a helper store them.
const AUTH_HINT: &str = concat!(
    "Check the username and password, or configure a credential helper ",
    "or load your key into ssh-agent."
);

actions!(strata, [FetchRemotes, PullBranch, PushBranch]);

/// The platform's primary modifier, spelled the way its menus spell it.
#[cfg(target_os = "macos")]
const MODIFIER: &str = "Cmd";
#[cfg(not(target_os = "macos"))]
const MODIFIER: &str = "Ctrl";

/// Binds the remote commands to the view's key context. `RepoView`'s root
/// element handles them, like every other view command.
pub fn bind(cx: &mut App) {
    crate::keymap::bind_group(cx, crate::keymap::Group::Remote);
}

/// What a title-bar button, its key binding or the force offer asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Fetch,
    Pull,
    Push,
}

/// The operation a running command is; it names the progress line and the
/// notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Fetch,
    Pull,
    Push,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Fetch => "Fetch",
            Kind::Pull => "Pull",
            Kind::Push => "Push",
        }
    }

    /// Title of a failure notification.
    fn failed(self) -> &'static str {
        match self {
            Kind::Fetch => "Fetch failed",
            Kind::Pull => "Pull failed",
            Kind::Push => "Push failed",
        }
    }
}

/// What the remote controls may act on, read from one snapshot.
#[derive(Debug, Default)]
struct Targets {
    /// False before the first snapshot and in `--fake`, where there is no
    /// session to run anything on.
    ready: bool,
    detached: bool,
    branch: Option<String>,
    upstream: Option<UpstreamInfo>,
    /// Remote names, derived from `refs/remotes`.
    remotes: Vec<String>,
}

impl Targets {
    fn read(snapshot: &RepoSnapshot) -> Self {
        let (branch, detached) = match snapshot.head.as_ref() {
            // An unborn branch has no commit to push or pull.
            Some(HeadInfo::Branch { short_name, target }) => {
                (target.map(|_| short_name.clone()), false)
            }
            Some(HeadInfo::Detached { .. }) => (None, true),
            None => (None, false),
        };
        let upstream = branch
            .as_ref()
            .and_then(|branch| snapshot.upstreams.get(branch).cloned());
        Self {
            ready: true,
            detached,
            branch,
            upstream,
            remotes: remotes_of(&snapshot.refs),
        }
    }

    /// The branch the buttons act on, or why there is none.
    fn branch(&self) -> Result<&str, &'static str> {
        if !self.ready {
            return Err(NO_SESSION);
        }
        match &self.branch {
            Some(branch) => Ok(branch),
            None if self.detached => Err(DETACHED_HEAD),
            None => Err("This branch has no commits yet."),
        }
    }
}

/// The remotes a snapshot knows about, from its `refs/remotes` short names.
///
/// A remote that was configured but never fetched has no tracking ref, so it
/// does not appear here; the publish dialog keeps `origin` as its fallback for
/// exactly that case.
fn remotes_of(refs: &[RefInfo]) -> Vec<String> {
    let remotes: BTreeSet<&str> = refs
        .iter()
        .filter(|reference| reference.kind == RefKind::RemoteBranch)
        // `origin/HEAD` and `origin/feature/x` both name the remote `origin`.
        .filter_map(|reference| remote_of(&reference.short_name))
        .collect();
    remotes.into_iter().map(str::to_owned).collect()
}

/// `origin/main` -> `origin`.
fn remote_of(reference: &str) -> Option<&str> {
    reference
        .split_once('/')
        .map(|(remote, _)| remote)
        .filter(|remote| !remote.is_empty())
}

/// What the Push button will do for the current branch.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PushPlan {
    /// The branch tracks an upstream. `remote` is that upstream's remote when
    /// its name carries one, which is what a force push needs.
    Upstream {
        branch: String,
        remote: Option<String>,
    },
    /// The branch has no upstream: ask which remote to publish it to.
    Publish { branch: String },
}

fn push_plan(targets: &Targets) -> Result<PushPlan, &'static str> {
    let branch = targets.branch()?.to_owned();
    match &targets.upstream {
        // A gone upstream behaves like no upstream: pushing recreates it, and
        // that is exactly what publishing does.
        Some(upstream) if !upstream.gone => Ok(PushPlan::Upstream {
            remote: remote_of(&upstream.name).map(str::to_owned),
            branch,
        }),
        _ => Ok(PushPlan::Publish { branch }),
    }
}

/// `Pushed feat/x to origin`.
fn push_message(branch: &str, remote: &str) -> String {
    format!("Pushed {branch} to {remote}")
}

/// `Pulled origin/main`: the upstream is git's own name for what arrived.
fn pull_message(upstream: Option<&str>) -> String {
    match upstream {
        Some(upstream) => format!("Pulled {upstream}"),
        None => "Pulled the current branch".to_owned(),
    }
}

fn commits(count: u32) -> String {
    format!("{count} commit{}", if count == 1 { "" } else { "s" })
}

/// What git's failure means for the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    /// git could not authenticate, even after the askpass prompt.
    Auth,
    /// The remote has commits the local branch does not have.
    NonFastForward,
    /// The branch has no upstream, so there is nothing to pull from.
    NoUpstream,
    Other,
}

/// git's own words for the conditions the UI reacts to.
const AUTH: [&str; 7] = [
    "authentication failed",
    "could not read username",
    "could not read password",
    "terminal prompts disabled",
    "permission denied (publickey",
    "host key verification failed",
    "access denied",
];

fn classify(error: &str) -> Failure {
    let lower = error.to_ascii_lowercase();
    if AUTH.iter().any(|hint| lower.contains(hint)) {
        return Failure::Auth;
    }
    // A rejected lease (`stale info`) is not a non-fast-forward: offering the
    // same force again would fail the same way, so it stays an ordinary error.
    let rejected = lower.contains("[rejected]") && !lower.contains("stale info");
    if rejected || lower.contains("non-fast-forward") || lower.contains("fetch first") {
        return Failure::NonFastForward;
    }
    if lower.contains("has no upstream branch")
        || lower.contains("no upstream configured for branch")
    {
        return Failure::NoUpstream;
    }
    Failure::Other
}

/// The one line of git's stderr worth reporting.
///
/// Progress counters and `hint:` lines are noise in a notification. A push the
/// remote refused names the branch and the reason on its `! [rejected]` line,
/// which is more useful than the generic `error: failed to push some refs`;
/// for everything else git names the problem on a `fatal:`/`error:` line.
fn first_error_line(error: &str) -> String {
    let lines = || {
        error
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("remote:"))
    };
    for prefix in ["! [rejected]", "! [remote rejected]", "fatal:", "error:"] {
        if let Some(line) = lines().find(|line| line.starts_with(prefix)) {
            return squash(line);
        }
    }
    lines()
        .find(|line| !line.starts_with("hint:"))
        .map_or_else(|| "git failed".to_owned(), squash)
}

/// git pads the rejected-ref line into columns; a notification has one line.
fn squash(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The notification text for a failed operation.
fn failure_message(failure: Failure, line: &str) -> String {
    match failure {
        Failure::Auth => format!("{line} — {AUTH_HINT}"),
        Failure::NoUpstream => format!("{line} — publish the branch to create the upstream."),
        Failure::NonFastForward | Failure::Other => line.to_owned(),
    }
}

/// The retry that turns a rejected push into a force push with lease.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ForceOffer {
    remote: String,
    branch: String,
    set_upstream: bool,
}

/// One of this module's operations that has not reported back yet.
struct Running {
    id: OperationId,
    kind: Kind,
    /// What a success notification says.
    success: String,
    /// The push to retry with `--force-with-lease`, when one is possible.
    retry: Option<ForceOffer>,
    /// The branch a successful push may offer a pull request for.
    pull_request_branch: Option<String>,
    /// git's latest progress line.
    text: Option<String>,
    percent: Option<u8>,
}

/// Everything the remote UI remembers between frames.
#[derive(Default)]
pub(crate) struct Remote {
    running: Option<Running>,
}

/// Palette subset the remote controls paint with, taken from the window
/// chrome's palette so the title bar and status bar read as one surface.
pub(crate) struct Colors {
    pub(crate) foreground: Hsla,
    pub(crate) muted_foreground: Hsla,
    pub(crate) border: Hsla,
    pub(crate) accent: Hsla,
    pub(crate) accent_foreground: Hsla,
}

impl Remote {
    pub(crate) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// An operation of this module started; nothing else to record, the
    /// progress and the outcome arrive separately.
    pub(crate) fn on_started(&mut self, id: OperationId, cx: &mut Context<RepoView>, active: bool) {
        if self
            .running
            .as_ref()
            .is_some_and(|running| running.id == id)
            && active
        {
            cx.notify();
        }
    }

    pub(crate) fn on_progress(
        &mut self,
        id: OperationId,
        text: &str,
        percent: Option<u8>,
        cx: &mut Context<RepoView>,
        active: bool,
    ) {
        let Some(running) = self.running.as_mut().filter(|running| running.id == id) else {
            return;
        };
        running.text = Some(text.trim().to_owned()).filter(|text| !text.is_empty());
        // A line without a counter (`From /tmp/origin.git`) must not blank a
        // bar that is still mid-transfer.
        if percent.is_some() {
            running.percent = percent;
        }
        if active {
            cx.notify();
        }
    }

    /// Records the end of an operation this module started and tells the user
    /// about it. Operations from other modules are ignored.
    pub(crate) fn on_finished(
        &mut self,
        id: OperationId,
        error: Option<&str>,
        cx: &mut Context<RepoView>,
        active: bool,
    ) {
        let Some(running) = self.running.take() else {
            return;
        };
        if running.id != id {
            self.running = Some(running);
            return;
        }

        let failure = error.map(|error| (classify(error), first_error_line(error)));
        let note = match &failure {
            Some((failure, line)) => {
                Notification::error(failure_message(*failure, line)).title(running.kind.failed())
            }
            None => Notification::success(running.success.clone()).title(running.kind.label()),
        };
        let entity = cx.entity();
        // A branch just pushed to GitHub is the moment the pull-request form
        // is useful; the action is added only when gh can open it.
        let note = match (&failure, running.pull_request_branch.as_deref()) {
            (None, Some(branch)) => crate::github::with_create_pr(note, &entity, branch, cx),
            _ => note,
        };
        // A push the remote refused as non-fast-forward is the one failure the
        // user can resolve in place.
        let offer = match failure {
            Some((Failure::NonFastForward, _)) if running.kind == Kind::Push => {
                running.retry.clone()
            }
            _ => None,
        };
        // Clear the status-bar segment even when there is no window left to
        // announce the outcome in.
        if active {
            cx.notify();
        }
        let Some(window) = window_handle(cx) else {
            return;
        };
        let _ = window.update(cx, |_, window, cx| {
            window.push_notification(note, cx);
            if active && let Some(offer) = offer {
                open_force_dialog(entity, offer, window, cx);
            }
        });
    }

    /// The status bar's operation segment, and nothing while idle.
    fn status(&self, colors: &Colors) -> Option<AnyElement> {
        let running = self.running.as_ref()?;
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_none()
            .max_w(px(560.0))
            .child(
                div()
                    .flex_none()
                    .text_color(colors.accent)
                    .child(running.kind.label()),
            );
        if let Some(text) = &running.text {
            row = row.child(div().min_w_0().truncate().child(text.clone()));
        }
        if let Some(percent) = running.percent {
            row = row
                .child(
                    div()
                        .flex_none()
                        .w(px(PROGRESS_WIDTH))
                        .h(px(4.0))
                        .rounded_full()
                        .bg(colors.border)
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(colors.accent)
                                .w(relative(percent as f32 / 100.0)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(colors.muted_foreground)
                        .child(format!("{percent}%")),
                );
        }
        Some(row.into_any_element())
    }
}

/// The window to put notifications and dialogs into.
///
/// Session events arrive on the foreground executor, so no window update is in
/// progress when this runs and the handle can be updated here.
fn window_handle(cx: &mut Context<RepoView>) -> Option<gpui::AnyWindowHandle> {
    cx.active_window()
        .or_else(|| cx.windows().into_iter().next())
}

/// Asks before rewriting a remote branch with `--force-with-lease`.
fn open_force_dialog(
    entity: Entity<RepoView>,
    offer: ForceOffer,
    window: &mut Window,
    cx: &mut App,
) {
    let entity = entity.downgrade();
    crate::ui::confirm(
        window,
        cx,
        crate::ui::Confirm::new(
            "Push rejected",
            format!(
                "{} has commits that {} does not have. Force pushing with lease replaces only what you have already fetched.",
                offer.remote, offer.branch
            ),
        )
        .ok_label("Force push (with lease)"),
        move |_, cx| {
            let confirmed = offer.clone();
            let _ = entity.update(cx, |view, cx| view.force_push(confirmed, cx));
        },
    );
}

impl RepoView {
    /// The Fetch / Pull / Push buttons in the title bar.
    ///
    /// `compact` draws each as an icon alone for the narrowest title bar; the
    /// tooltip then carries the label, and the ahead/behind count stays on the
    /// button as a badge.
    pub(crate) fn remote_buttons(
        &self,
        source: Option<&Source>,
        colors: &Colors,
        compact: bool,
    ) -> AnyElement {
        let targets = source.map_or_else(Targets::default, |source| match source {
            Source::Repo(snapshot) => Targets::read(snapshot),
            Source::Fake(_) => Targets::default(),
        });
        let blocked = self.remote_blocked();

        let entity = self.entity();
        // The design stacks a 16 px icon over a 10 px label; the badge sits in
        // the button's top-right corner so it cannot shift the label.
        let button = |id: &'static str,
                      icon_name: IconName,
                      label: &'static str,
                      tooltip: String,
                      enabled: bool,
                      badge: Option<String>,
                      command: Command| {
            let entity = entity.clone();
            let tooltip = if compact {
                named_tooltip(label, tooltip)
            } else {
                tooltip
            };
            let mut content = div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(2.0))
                .text_size(px(10.0))
                .child(Icon::new(icon_name).with_size(px(16.0)));
            if !compact {
                content = content.child(label);
            }
            if let Some(count) = badge {
                // An icon-only button has no label under the icon, so its
                // badge moves into the corner the label would have left free.
                let (top, right) = if compact { (1.0, 2.0) } else { (2.0, 8.0) };
                content = content.child(
                    div()
                        .absolute()
                        .top(px(top))
                        .right(px(right))
                        .min_w(px(14.0))
                        .h(px(14.0))
                        .px(px(3.0))
                        .rounded_full()
                        .bg(colors.accent)
                        .text_color(colors.accent_foreground)
                        .text_size(px(9.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(count.to_string()),
                );
            }
            crate::ui::content_button(id)
                .ghost()
                .w(px(if compact {
                    ICON_BUTTON_WIDTH
                } else {
                    LABELLED_BUTTON_WIDTH
                }))
                .h(px(40.0))
                .p_0()
                .text_color(if enabled {
                    colors.foreground
                } else {
                    colors.muted_foreground
                })
                .disabled(!enabled)
                .tooltip(tooltip)
                .on_click(move |_, window, cx| {
                    entity.update(cx, |view, cx| view.remote_command(command, window, cx));
                })
                .child(content)
        };

        let tracked = targets.upstream.as_ref().filter(|upstream| !upstream.gone);
        // An unknown count gets a "?" badge: omitting it would read as level,
        // which is exactly what the shallow history cannot promise.
        let badge = |count: Option<u32>| match count {
            Some(count) if count > 0 => Some(count.to_string()),
            None => Some("?".to_owned()),
            Some(_) => None,
        };
        let behind = tracked.and_then(|upstream| badge(upstream.behind));
        let ahead = tracked.and_then(|upstream| badge(upstream.ahead));

        let fetch = button(
            "remote-fetch",
            IconName::RefreshCw,
            "Fetch",
            fetch_tooltip(blocked),
            blocked.is_none(),
            None,
            Command::Fetch,
        );

        let pull = button(
            "remote-pull",
            IconName::ArrowDown,
            "Pull",
            pull_tooltip(&targets, blocked),
            blocked.is_none() && targets.branch().is_ok(),
            behind,
            Command::Pull,
        );

        let push = button(
            "remote-push",
            IconName::ArrowUp,
            "Push",
            push_tooltip(&targets, blocked),
            blocked.is_none() && push_plan(&targets).is_ok(),
            ahead,
            Command::Push,
        );

        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .flex_none()
            .child(fetch)
            .child(pull)
            .child(push)
            .into_any_element()
    }

    /// The status bar's running-operation segment.
    pub(crate) fn remote_status(&self, colors: &Colors) -> Option<AnyElement> {
        self.remote.status(colors)
    }

    /// Why a remote command cannot run right now, if it cannot.
    fn remote_blocked(&self) -> Option<&'static str> {
        if self.session.is_none() {
            return Some(NO_SESSION);
        }
        if self.remote.is_running()
            || self.branches.is_running()
            || self
                .session
                .as_ref()
                .is_some_and(|session| session.snapshot().busy.is_some())
        {
            return Some(BUSY);
        }
        None
    }

    /// Runs what a button or its key binding asked for.
    pub(crate) fn remote_command(
        &mut self,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.remote_blocked().is_some() {
            return;
        }
        match command {
            Command::Fetch => self.fetch_remotes(cx),
            Command::Pull => self.pull(cx),
            Command::Push => self.push(window, cx),
        }
    }

    /// Fetches every remote, pruning branches that were deleted on it unless
    /// the user turned pruning off.
    ///
    /// Fetching only the current branch's remote would leave every other
    /// remote's badges stale, and prune is what stops a branch deleted on the
    /// server from keeping its remote-tracking ref forever.
    fn fetch_remotes(&mut self, cx: &mut Context<Self>) {
        self.start_remote(
            Operation::Fetch {
                remote: None,
                prune: crate::settings::Settings::get(cx).behaviour.prune_on_fetch,
            },
            Kind::Fetch,
            "Fetched all remotes".to_owned(),
            None,
            None,
            cx,
        );
    }

    /// Fetches the history below a shallow clone's boundary.
    ///
    /// The commit details offer this when the selected commit is the boundary;
    /// it runs through the same progress bar and outcome notification as Fetch.
    pub(crate) fn fetch_full_history(&mut self, cx: &mut Context<Self>) {
        if self.remote_blocked().is_some() {
            return;
        }
        self.start_remote(
            Operation::FetchUnshallow,
            Kind::Fetch,
            "Fetched the full history".to_owned(),
            None,
            None,
            cx,
        );
    }

    /// Pulls the current branch's upstream, with the merge or rebase behaviour
    /// the repository's own git config asks for.
    fn pull(&mut self, cx: &mut Context<Self>) {
        let targets = self.remote_targets();
        if targets.branch().is_err() {
            return;
        }
        let upstream = targets
            .upstream
            .as_ref()
            .filter(|upstream| !upstream.gone)
            .map(|upstream| upstream.name.as_str());
        self.start_remote(
            Operation::Pull { rebase: None },
            Kind::Pull,
            pull_message(upstream),
            None,
            None,
            cx,
        );
    }

    fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.remote_targets();
        match push_plan(&targets) {
            Err(_) => {}
            Ok(PushPlan::Upstream { branch, remote }) => {
                let pull_request_branch = Some(branch.clone());
                let (success, retry) = match remote {
                    Some(remote) => (
                        push_message(&branch, &remote),
                        Some(ForceOffer {
                            remote,
                            branch,
                            set_upstream: false,
                        }),
                    ),
                    None => (format!("Pushed {branch}"), None),
                };
                self.start_remote(
                    Operation::Push {
                        remote: None,
                        branch: None,
                        set_upstream: false,
                        force_with_lease: false,
                    },
                    Kind::Push,
                    success,
                    retry,
                    pull_request_branch,
                    cx,
                );
            }
            Ok(PushPlan::Publish { branch }) => {
                self.open_publish_dialog(branch, targets.remotes, window, cx)
            }
        }
    }

    /// Asks which remote a branch without an upstream should be published to.
    fn open_publish_dialog(
        &mut self,
        branch: String,
        remotes: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `refs/remotes` is the only remote list a snapshot carries, and a
        // remote configured but never fetched has none, so `origin` stays the
        // fallback choice.
        let remotes = if remotes.is_empty() {
            vec!["origin".to_owned()]
        } else {
            remotes
        };
        let entity = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let mut footer = DialogFooter::new().child(
                crate::ui::button("publish-cancel", "Cancel")
                    .size(crate::ui::ButtonSize::Medium)
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            );
            for remote in &remotes {
                let entity = entity.clone();
                let branch = branch.clone();
                let target = remote.clone();
                footer = footer.child(
                    crate::ui::button(format!("publish-{remote}"), format!("Push to {remote}"))
                        .size(crate::ui::ButtonSize::Medium)
                        .kind(if remote == "origin" {
                            crate::ui::ButtonKind::Primary
                        } else {
                            crate::ui::ButtonKind::Normal
                        })
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let remote = target.clone();
                            let branch = branch.clone();
                            let _ = entity.update(cx, |view, cx| {
                                view.publish_branch(remote, branch, cx);
                            });
                        }),
                );
            }
            dialog
                .title(format!("Publish {branch}"))
                .description("This branch has no upstream. Choose the remote that should track it.")
                .footer(footer)
        });
    }

    /// Pushes a branch for the first time and records the upstream.
    fn publish_branch(&mut self, remote: String, branch: String, cx: &mut Context<Self>) {
        let success = push_message(&branch, &remote);
        let pull_request_branch = Some(branch.clone());
        let retry = Some(ForceOffer {
            remote: remote.clone(),
            branch: branch.clone(),
            set_upstream: true,
        });
        self.start_remote(
            Operation::Push {
                remote: Some(remote),
                branch: Some(branch),
                set_upstream: true,
                force_with_lease: false,
            },
            Kind::Push,
            success,
            retry,
            pull_request_branch,
            cx,
        );
    }

    /// Reruns a rejected push with `--force-with-lease`, after confirmation.
    fn force_push(&mut self, offer: ForceOffer, cx: &mut Context<Self>) {
        let success = push_message(&offer.branch, &offer.remote);
        self.start_remote(
            Operation::Push {
                remote: Some(offer.remote),
                branch: Some(offer.branch),
                set_upstream: offer.set_upstream,
                force_with_lease: true,
            },
            Kind::Push,
            success,
            // A rejected lease is reported, not offered again.
            None,
            None,
            cx,
        );
    }

    fn start_remote(
        &mut self,
        operation: Operation,
        kind: Kind,
        success: String,
        retry: Option<ForceOffer>,
        pull_request_branch: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let id = session.run(operation);
        self.remote.running = Some(Running {
            id,
            kind,
            success,
            retry,
            pull_request_branch,
            text: None,
            percent: None,
        });
        cx.notify();
    }

    fn remote_targets(&self) -> Targets {
        self.session
            .as_ref()
            .map_or_else(Targets::default, |session| {
                Targets::read(&session.snapshot())
            })
    }
}

fn shortcut(key: char) -> String {
    format!("{MODIFIER}+Alt+{key}")
}

/// An icon-only button has no visible label, so its tooltip has to name it.
///
/// The tooltip builders normally open with the button's verb already; the
/// prefix covers the states that do not, such as a blocked button.
fn named_tooltip(label: &str, tooltip: String) -> String {
    if tooltip.starts_with(label) {
        tooltip
    } else {
        format!("{label} · {tooltip}")
    }
}

fn fetch_tooltip(blocked: Option<&'static str>) -> String {
    blocked.map_or_else(
        || {
            format!(
                "Fetch every remote and prune branches deleted on them · {}",
                shortcut('F')
            )
        },
        str::to_owned,
    )
}

fn pull_tooltip(targets: &Targets, blocked: Option<&'static str>) -> String {
    if let Some(blocked) = blocked {
        return blocked.to_owned();
    }
    if let Err(reason) = targets.branch() {
        return reason.to_owned();
    }
    let mut tip = match &targets.upstream {
        Some(upstream) if !upstream.gone => {
            format!("Pull {} (merge or rebase per git config)", upstream.name)
        }
        Some(_) => "Pull the current branch (its upstream is gone)".to_owned(),
        None => "Pull the current branch".to_owned(),
    };
    if let Some(upstream) = targets.upstream.as_ref().filter(|upstream| !upstream.gone) {
        match upstream.behind {
            Some(behind) if behind > 0 => {
                tip.push_str(&format!(" · {} behind", commits(behind)));
            }
            None => tip.push_str(" · counts unavailable: history is shallow"),
            Some(_) => {}
        }
    }
    tip.push_str(&format!(" · {}", shortcut('L')));
    tip
}

fn push_tooltip(targets: &Targets, blocked: Option<&'static str>) -> String {
    if let Some(blocked) = blocked {
        return blocked.to_owned();
    }
    match push_plan(targets) {
        Err(reason) => reason.to_owned(),
        Ok(PushPlan::Upstream { branch, remote }) => {
            let mut tip = match remote {
                Some(remote) => format!("Push {branch} to {remote}"),
                None => format!("Push {branch} to its upstream"),
            };
            if let Some(upstream) = targets.upstream.as_ref().filter(|upstream| !upstream.gone) {
                match upstream.ahead {
                    Some(ahead) if ahead > 0 => {
                        tip.push_str(&format!(" · {} ahead", commits(ahead)));
                    }
                    None => tip.push_str(" · counts unavailable: history is shallow"),
                    Some(_) => {}
                }
            }
            tip.push_str(&format!(" · {}", shortcut('P')));
            tip
        }
        Ok(PushPlan::Publish { branch }) => format!(
            "Publish {branch}: choose a remote and set its upstream · {}",
            shortcut('P')
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use strata_core::session::CommitList;
    use strata_core::{Oid, RepoState, StashInfo, WorktreeStatus};

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn reference(short_name: &str, kind: RefKind) -> RefInfo {
        RefInfo {
            full_name: match kind {
                RefKind::LocalBranch => format!("refs/heads/{short_name}"),
                RefKind::RemoteBranch => format!("refs/remotes/{short_name}"),
                RefKind::Tag => format!("refs/tags/{short_name}"),
            },
            short_name: short_name.to_owned(),
            kind,
            target: oid(1),
        }
    }

    fn snapshot(head: Option<HeadInfo>, upstreams: &[(&str, UpstreamInfo)]) -> RepoSnapshot {
        RepoSnapshot {
            generation: 1,
            head,
            refs: Arc::new(Vec::new()),
            refs_by_commit: Arc::new(HashMap::new()),
            commits: CommitList::default(),
            commit_index: Arc::new(HashMap::new()),
            status: Arc::new(WorktreeStatus::default()),
            stashes: Arc::new(Vec::<StashInfo>::new()),
            upstreams: Arc::new(
                upstreams
                    .iter()
                    .map(|(name, upstream)| ((*name).to_owned(), upstream.clone()))
                    .collect(),
            ),
            state: RepoState::Normal,
            conflicts: Arc::new(Vec::new()),
            busy: None,
            remotes: Vec::new(),
        }
    }

    fn upstream(name: &str, ahead: u32, behind: u32, gone: bool) -> UpstreamInfo {
        UpstreamInfo {
            name: name.to_owned(),
            ahead: Some(ahead),
            behind: Some(behind),
            gone,
        }
    }

    fn unknown_upstream(name: &str) -> UpstreamInfo {
        UpstreamInfo {
            name: name.to_owned(),
            ahead: None,
            behind: None,
            gone: false,
        }
    }

    fn branch_head(name: &str) -> HeadInfo {
        HeadInfo::Branch {
            short_name: name.to_owned(),
            target: Some(oid(2)),
        }
    }

    #[test]
    fn remotes_come_from_remote_tracking_refs_once_each() {
        let refs = vec![
            reference("main", RefKind::LocalBranch),
            reference("v1.0", RefKind::Tag),
            reference("origin/main", RefKind::RemoteBranch),
            reference("origin/HEAD", RefKind::RemoteBranch),
            reference("origin/feature/login", RefKind::RemoteBranch),
            reference("upstream/main", RefKind::RemoteBranch),
        ];
        assert_eq!(remotes_of(&refs), ["origin", "upstream"]);
        assert!(
            remotes_of(&[reference("main", RefKind::LocalBranch)]).is_empty(),
            "a repository without remote refs lists no remotes"
        );
    }

    #[test]
    fn an_upstream_name_carries_its_remote() {
        assert_eq!(remote_of("origin/main"), Some("origin"));
        assert_eq!(remote_of("upstream/feature/login"), Some("upstream"));
        assert_eq!(remote_of("main"), None);
        assert_eq!(remote_of("/main"), None);
    }

    #[test]
    fn git_failures_are_classified_for_the_ui() {
        assert_eq!(
            classify(
                "fatal: could not read Username for 'https://example.com': terminal prompts disabled"
            ),
            Failure::Auth
        );
        assert_eq!(
            classify(
                "git@example.com: Permission denied (publickey).\nfatal: Could not read from remote repository."
            ),
            Failure::Auth
        );
        assert_eq!(
            classify(
                "To /tmp/origin.git\n ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs"
            ),
            Failure::NonFastForward
        );
        assert_eq!(
            classify(" ! [rejected]        main -> main (non-fast-forward)"),
            Failure::NonFastForward
        );
        assert_eq!(
            classify("fatal: The current branch feat/x has no upstream branch"),
            Failure::NoUpstream
        );
        assert_eq!(
            classify(" ! [rejected]        main -> main (stale info)"),
            Failure::Other,
            "a rejected lease must not offer the same force again"
        );
        assert_eq!(classify("fatal: not a git repository"), Failure::Other);
    }

    #[test]
    fn the_first_meaningful_error_line_is_the_one_reported() {
        let push = "To /tmp/origin.git\n ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs to '/tmp/origin.git'\nhint: Updates were rejected because the remote contains work\nhint: See the 'Note about fast-forwards' section";
        assert_eq!(
            first_error_line(push),
            "! [rejected] main -> main (fetch first)",
            "the rejected-ref line names the branch and the reason"
        );

        let auth =
            "remote: progress\nfatal: Authentication failed for 'https://example.com/repo.git'";
        assert_eq!(
            first_error_line(auth),
            "fatal: Authentication failed for 'https://example.com/repo.git'"
        );

        assert_eq!(
            first_error_line("  \n only chatter here \n"),
            "only chatter here"
        );
        assert_eq!(first_error_line(""), "git failed");
    }

    #[test]
    fn a_failed_push_names_the_branch_it_could_not_rewrite() {
        assert_eq!(
            failure_message(
                Failure::NonFastForward,
                "! [rejected] main -> main (fetch first)"
            ),
            "! [rejected] main -> main (fetch first)"
        );
        let auth = failure_message(Failure::Auth, "fatal: Authentication failed");
        assert!(auth.starts_with("fatal: Authentication failed — "));
        assert!(
            auth.contains("credential helper") && auth.contains("ssh-agent"),
            "the hint has to say what to configure: {auth}"
        );
        assert!(
            failure_message(Failure::NoUpstream, "fatal: no upstream").contains("publish"),
            "a missing upstream points at the publish flow"
        );
    }

    #[test]
    fn notifications_name_what_moved() {
        assert_eq!(push_message("feat/x", "origin"), "Pushed feat/x to origin");
        assert_eq!(pull_message(Some("origin/main")), "Pulled origin/main");
        assert_eq!(pull_message(None), "Pulled the current branch");
        assert_eq!(commits(1), "1 commit");
        assert_eq!(commits(2), "2 commits");
    }

    #[test]
    fn a_detached_head_blocks_pull_and_push() {
        let targets = Targets::read(&snapshot(Some(HeadInfo::Detached { target: oid(3) }), &[]));
        assert_eq!(targets.branch(), Err(DETACHED_HEAD));
        assert_eq!(push_plan(&targets), Err(DETACHED_HEAD));
        assert!(pull_tooltip(&targets, None).contains("Detached HEAD"));
        assert!(push_tooltip(&targets, None).contains("Detached HEAD"));
    }

    #[test]
    fn a_fake_or_loading_view_cannot_run_remotes() {
        let targets = Targets::default();
        assert_eq!(targets.branch(), Err(NO_SESSION));
        assert!(push_plan(&targets).is_err());
        assert!(pull_tooltip(&Targets::default(), None).contains("--fake"));

        // The first snapshot has no HEAD yet.
        let loading = Targets::read(&snapshot(None, &[]));
        assert!(loading.ready);
        assert_eq!(
            pull_tooltip(&loading, None),
            "This branch has no commits yet."
        );
    }

    #[test]
    fn push_follows_the_upstream_or_publishes() {
        let tracking = Targets::read(&snapshot(
            Some(branch_head("feat/x")),
            &[("feat/x", upstream("origin/feat/x", 2, 1, false))],
        ));
        assert_eq!(
            push_plan(&tracking),
            Ok(PushPlan::Upstream {
                branch: "feat/x".to_owned(),
                remote: Some("origin".to_owned()),
            })
        );
        let tip = push_tooltip(&tracking, None);
        assert!(tip.starts_with("Push feat/x to origin"), "{tip}");
        assert!(tip.contains("2 commits ahead"), "{tip}");
        let pull_tip = pull_tooltip(&tracking, None);
        assert!(pull_tip.starts_with("Pull origin/feat/x"), "{pull_tip}");
        assert!(pull_tip.contains("1 commit behind"), "{pull_tip}");

        let untracked = Targets::read(&snapshot(Some(branch_head("feat/x")), &[]));
        assert_eq!(
            push_plan(&untracked),
            Ok(PushPlan::Publish {
                branch: "feat/x".to_owned()
            })
        );
        assert!(push_tooltip(&untracked, None).contains("Publish feat/x"));

        let gone = Targets::read(&snapshot(
            Some(branch_head("feat/x")),
            &[("feat/x", upstream("origin/feat/x", 0, 0, true))],
        ));
        assert!(
            matches!(push_plan(&gone), Ok(PushPlan::Publish { .. })),
            "a gone upstream is republished"
        );
    }

    #[test]
    fn unknown_counts_do_not_read_as_up_to_date() {
        let targets = Targets::read(&snapshot(
            Some(branch_head("feat/x")),
            &[("feat/x", unknown_upstream("origin/feat/x"))],
        ));
        let pull_tip = pull_tooltip(&targets, None);
        let push_tip = push_tooltip(&targets, None);
        for tip in [&pull_tip, &push_tip] {
            assert!(
                tip.contains("counts unavailable: history is shallow"),
                "an unknown count must say so: {tip}"
            );
        }
    }

    #[test]
    fn an_icon_only_button_names_itself_in_the_tooltip() {
        let fetch = fetch_tooltip(None);
        assert_eq!(
            named_tooltip("Fetch", fetch.clone()),
            fetch,
            "a tooltip that already opens with the verb needs no prefix"
        );
        assert_eq!(
            named_tooltip("Pull", BUSY.to_owned()),
            format!("Pull · {BUSY}"),
            "a blocked button still says which command it is"
        );
        assert_eq!(
            named_tooltip("Push", push_tooltip(&Targets::default(), None)),
            format!("Push · {NO_SESSION}")
        );
    }

    #[test]
    fn one_running_operation_blocks_another() {
        assert!(fetch_tooltip(Some(BUSY)).contains("already running"));
        assert!(push_tooltip(&Targets::default(), Some(BUSY)).contains("already running"));
        assert!(
            fetch_tooltip(None).contains(&shortcut('F')),
            "an enabled button names its key"
        );
    }
}
