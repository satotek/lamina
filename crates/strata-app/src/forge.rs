//! Forge links and pull-request actions for the remotes Strata understands.
//!
//! Every URL is built from the repository's own remote, so a link never needs a
//! token or a network call. The CLIs are used only for what a link cannot do:
//! listing pull requests, checking one out, and — on GitHub — opening the
//! creation form through `gh pr create --web`, which keeps credentials inside
//! the CLI.
//!
//! Items are hidden when the selected remote is not one of the supported
//! hosting services. A GitHub remote whose `gh` is missing or logged out keeps
//! its links and disables the pull-request entries with the reason; Azure
//! DevOps does the same for `az` and its extension.

use std::path::PathBuf;

use gpui::{App, Entity, Window};
use gpui_component::WindowExt as _;
use gpui_component::button::Button;
use gpui_component::menu::PopupMenuItem;
use gpui_component::notification::Notification;
use strata_core::Oid;
use strata_core::forge::{Az, ForgeCli, ForgeKind, ForgeRemote, Gh};
use strata_core::tools as core_tools;

use crate::repo_view::RepoView;
use crate::settings::Settings;
use crate::tools::Tooling;

/// The branch-menu entries forge integration adds: the web page for the
/// branch, the pull-request form, and the branch's own pull request when the
/// cache knows one.
pub(crate) struct BranchItems {
    pub(crate) open: PopupMenuItem,
    pub(crate) create_pr: PopupMenuItem,
    /// Set when the branch has a pull request in the cache.
    pub(crate) open_pr: Option<PopupMenuItem>,
    pub(crate) checkout_pr: PopupMenuItem,
}

/// The supported forge remote this repository pushes to, if any.
///
/// `origin` wins over the other remotes, then the first supported URL in the
/// configured order. An unsupported remote returns `None`, which hides every
/// forge item.
pub(crate) fn remote_for(view: &RepoView, cx: &App) -> Option<ForgeRemote> {
    if !Settings::get(cx).tools.pull_requests.enabled {
        return None;
    }
    let session = view.session.as_ref()?;
    let snapshot = session.snapshot();
    let known = Tooling::get(cx)
        .and_then(Tooling::gh)
        .map(|gh| gh.hosts.clone())
        .unwrap_or_default();

    let ordered = snapshot
        .remotes
        .iter()
        .filter(|remote| remote.name == "origin")
        .chain(
            snapshot
                .remotes
                .iter()
                .filter(|remote| remote.name != "origin"),
        );
    for remote in ordered {
        for url in [&remote.fetch_url, &remote.push_url].into_iter().flatten() {
            if let Some(parsed) = ForgeRemote::parse(url, &known) {
                return Some(parsed);
            }
        }
    }
    None
}

/// The host-aware "open on …" item for a commit.
pub(crate) fn commit_item(
    entity: &Entity<RepoView>,
    commit: Oid,
    cx: &App,
) -> Option<PopupMenuItem> {
    let remote = remote_for(entity.read(cx), cx)?;
    Some(open_item(
        remote.kind(),
        remote.commit_url(&commit.to_string()),
    ))
}

/// The same, for a file at a commit, optionally highlighting a line range.
pub(crate) fn file_item(
    entity: &Entity<RepoView>,
    commit: Oid,
    path: &str,
    lines: Option<(u32, u32)>,
    cx: &App,
) -> Option<PopupMenuItem> {
    let remote = remote_for(entity.read(cx), cx)?;
    Some(open_item(
        remote.kind(),
        remote.file_url(&commit.to_string(), path, lines),
    ))
}

/// Branch-menu entries: the web page, the pull-request form, and the branch's
/// pull request when the cache has one.
pub(crate) fn branch_items(
    entity: &Entity<RepoView>,
    branch: &str,
    cx: &App,
) -> Option<BranchItems> {
    let remote = remote_for(entity.read(cx), cx)?;
    let pull_request = entity.read(cx).pull_request_for_branch(branch);
    let open_pr = pull_request.as_ref().map(|pull_request| {
        let url = pull_request.url.clone();
        let label = t!("forge.open_pull_request", number = pull_request.number).to_string();
        PopupMenuItem::new(label).on_click(
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                cx.open_with_system(std::path::Path::new(&url));
            },
        )
    });
    // A cached pull request knows the branch it merges into; without one the
    // host's form preselects the repository's default branch.
    let target = pull_request.map(|pull_request| pull_request.base.clone());
    Some(BranchItems {
        open: open_item(remote.kind(), remote.branch_url(branch)),
        create_pr: create_pr_item(entity, &remote, branch, target.as_deref(), cx),
        open_pr,
        checkout_pr: checkout_pr_item(entity, &remote, cx),
    })
}

/// The host-aware "open on …" item for a remote-tracking branch, using its
/// branch name on the remote rather than the `origin/` prefix.
pub(crate) fn remote_branch_item(
    entity: &Entity<RepoView>,
    reference: &str,
    cx: &App,
) -> Option<PopupMenuItem> {
    let remote = remote_for(entity.read(cx), cx)?;
    let branch = entity
        .read(cx)
        .session
        .as_ref()
        .and_then(|session| {
            let snapshot = session.snapshot();
            reference
                .split_once('/')
                .filter(|(name, _)| snapshot.remotes.iter().any(|remote| remote.name == *name))
                .map(|(_, branch)| branch.to_owned())
        })
        .unwrap_or_else(|| reference.to_owned());
    Some(open_item(remote.kind(), remote.branch_url(&branch)))
}

/// Adds the pull-request action to a successful push notification, when the
/// branch is on a supported remote.
///
/// The action opens the branch's existing pull request when the cache knows one
/// is open, and asks for the creation form when it does not; either way the
/// push marks the cache, because it is the moment the branch may have gained or
/// lost a pull request, and the refresh that follows it must not be throttled.
pub(crate) fn with_create_pr(
    note: Notification,
    view: &RepoView,
    branch: &str,
    cx: &App,
) -> Notification {
    let Some(remote) = remote_for(view, cx) else {
        return note;
    };
    view.pull_requests.note_push();

    // The branch's pull request is already open: the form would ask the host
    // for a second one, so the notification offers the existing one instead.
    let existing = view
        .pull_requests
        .open_for_branch(branch)
        .map(|pull_request| (pull_request.number, pull_request.url.clone()));
    if let Some((number, url)) = existing {
        return note.action(move |_, _, _| {
            let url = url.clone();
            Button::new("notification-open-pr")
                .label(t!("forge.open_pull_request", number = number).to_string())
                .on_click(move |_, _, cx| {
                    cx.open_with_system(std::path::Path::new(&url));
                })
        });
    }

    match &remote {
        // GitHub keeps `gh pr create --web`, which carries the same form as the
        // link and fails loudly when the CLI is unusable.
        ForgeRemote::GitHub(_) => {
            let Some(tooling) = Tooling::get(cx) else {
                return note;
            };
            if !tooling.gh_reason().is_empty() {
                return note;
            }
            let Some(gh) = tooling.gh().map(|info| info.binary.clone()) else {
                return note;
            };
            let Some(workdir) = view_workdir(view) else {
                return note;
            };
            let branch = branch.to_owned();
            note.action(move |_, _, _| {
                let gh = gh.clone();
                let workdir = workdir.clone();
                let branch = branch.clone();
                Button::new("notification-create-pr")
                    .label(t!("forge.create_pr").to_string())
                    .on_click(move |_, window, cx| {
                        create_pull_request(
                            gh.clone(),
                            workdir.clone(),
                            branch.clone(),
                            window,
                            cx,
                        );
                    })
            })
        }
        _ => {
            let target = view
                .pull_requests
                .for_branch(branch)
                .map(|pull_request| pull_request.base.clone());
            let url = remote.create_pull_request_url(branch, target.as_deref());
            note.action(move |_, _, _| {
                let url = url.clone();
                Button::new("notification-create-pr")
                    .label(t!("forge.create_pr").to_string())
                    .on_click(move |_, _, cx| {
                        cx.open_with_system(std::path::Path::new(&url));
                    })
            })
        }
    }
}

fn open_label(kind: ForgeKind) -> String {
    match kind {
        ForgeKind::GitHub => t!("forge.open_on_github").to_string(),
        ForgeKind::AzureDevOps => t!("forge.open_on_azure").to_string(),
        ForgeKind::Bitbucket => t!("forge.open_on_bitbucket").to_string(),
    }
}

fn open_item(kind: ForgeKind, url: String) -> PopupMenuItem {
    PopupMenuItem::new(open_label(kind)).on_click(
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            cx.open_with_system(std::path::Path::new(&url));
        },
    )
}

fn create_pr_item(
    entity: &Entity<RepoView>,
    remote: &ForgeRemote,
    branch: &str,
    target: Option<&str>,
    cx: &App,
) -> PopupMenuItem {
    // Only GitHub has a CLI form; the other hosts open their own web form.
    if remote.kind() != ForgeKind::GitHub {
        let url = remote.create_pull_request_url(branch, target);
        return PopupMenuItem::new(t!("forge.create_pr").to_string()).on_click(
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                cx.open_with_system(std::path::Path::new(&url));
            },
        );
    }

    let blocked = gh_for(remote.host(), cx).err();
    let label = match &blocked {
        Some(reason) => t!("forge.create_pr_blocked", reason = reason).to_string(),
        None => t!("forge.create_pr").to_string(),
    };
    let mut item = PopupMenuItem::new(label).disabled(blocked.is_some());
    if blocked.is_some() {
        return item;
    }
    let gh = Tooling::get(cx)
        .and_then(Tooling::gh)
        .map(|info| info.binary.clone());
    let workdir = workdir(entity, cx);
    let (Some(gh), Some(workdir)) = (gh, workdir) else {
        return item;
    };
    let branch = branch.to_owned();
    item = item.on_click(
        move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
            create_pull_request(gh.clone(), workdir.clone(), branch.clone(), window, cx);
        },
    );
    item
}

fn checkout_pr_item(entity: &Entity<RepoView>, remote: &ForgeRemote, cx: &App) -> PopupMenuItem {
    let blocked = if entity.read(cx).pull_requests.is_checking_out() {
        Some(t!("forge.checkout_running").to_string())
    } else {
        cli_for(remote, cx).err()
    };
    let label = match &blocked {
        Some(reason) => t!("forge.checkout_pr_blocked", reason = reason).to_string(),
        None => t!("forge.checkout_pr").to_string(),
    };
    let mut item = PopupMenuItem::new(label).disabled(blocked.is_some());
    if blocked.is_some() {
        return item;
    }
    let entity = entity.clone();
    item = item.on_click(
        move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| view.open_checkout_dialog(window, cx));
        },
    );
    item
}

/// The CLI that can answer pull-request questions for `remote`.
///
/// Bitbucket Cloud has no CLI in this build: its pull requests need an API
/// token, which is a later stage, so the items say that instead of failing.
pub(crate) fn cli_for(remote: &ForgeRemote, cx: &App) -> Result<Box<dyn ForgeCli>, String> {
    if !Settings::get(cx).tools.pull_requests.enabled {
        return Err(t!("forge.integration_off").to_string());
    }
    match remote {
        ForgeRemote::GitHub(remote) => {
            gh_for(&remote.host, cx).map(|binary| Box::new(Gh::new(&binary)) as Box<dyn ForgeCli>)
        }
        ForgeRemote::AzureDevOps(remote) => {
            az_for(cx).map(|binary| Box::new(Az::new(&binary, remote.clone())) as Box<dyn ForgeCli>)
        }
        ForgeRemote::Bitbucket(_) => Err(t!("forge.pull_requests_unavailable").to_string()),
    }
}

/// The `gh` binary when it is installed and logged in to `host`.
///
/// The host check matters: a repository on an enterprise host whose account is
/// not logged in would otherwise get a list call that always fails.
pub(crate) fn gh_for(host: &str, cx: &App) -> Result<PathBuf, String> {
    if !Settings::get(cx).tools.pull_requests.enabled {
        return Err(t!("forge.integration_off").to_string());
    }
    let Some(tooling) = Tooling::get(cx) else {
        return Err(t!("settings.tools.gh_missing").to_string());
    };
    match tooling.gh() {
        Some(gh)
            if gh.logged_in
                && gh
                    .hosts
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(host)) =>
        {
            Ok(gh.binary.clone())
        }
        Some(gh) if !gh.logged_in => Err(t!("forge.gh_not_logged_in").to_string()),
        Some(_) => Err(t!("forge.not_logged_in_host", host = host).to_string()),
        None => Err(tooling.gh_reason()),
    }
}

/// The `az` binary when its Azure DevOps extension is installed and an account
/// is signed in.
pub(crate) fn az_for(cx: &App) -> Result<PathBuf, String> {
    if !Settings::get(cx).tools.pull_requests.enabled {
        return Err(t!("forge.integration_off").to_string());
    }
    let Some(tooling) = Tooling::get(cx) else {
        return Err(t!("settings.tools.az_missing").to_string());
    };
    match tooling.az() {
        Some(az) if tooling.az_reason().is_empty() => Ok(az.binary.clone()),
        Some(_) => Err(tooling.az_reason()),
        None => Err(tooling.az_reason()),
    }
}

fn view_workdir(view: &RepoView) -> Option<PathBuf> {
    view.session
        .as_ref()
        .map(|session| session.git_extras().workdir().to_path_buf())
}

fn workdir(entity: &Entity<RepoView>, cx: &App) -> Option<PathBuf> {
    entity
        .read(cx)
        .session
        .as_ref()
        .map(|session| session.git_extras().workdir().to_path_buf())
}

fn create_pull_request(
    gh: PathBuf,
    workdir: PathBuf,
    branch: String,
    window: &mut Window,
    cx: &mut App,
) {
    let handle = window.window_handle();
    let task = cx.background_executor().spawn(async move {
        core_tools::create_pull_request(&gh, &workdir, &branch).map_err(|error| error.to_string())
    });
    cx.spawn(async move |cx| {
        let Err(error) = task.await else {
            return;
        };
        let _ = handle.update(cx, |_, window, cx| {
            window.push_notification(
                Notification::error(t!("forge.create_pr_error", error = error).to_string())
                    .title(t!("forge.title").to_string()),
                cx,
            );
        });
    })
    .detach();
}
