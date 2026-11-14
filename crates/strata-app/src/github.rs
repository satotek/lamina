//! GitHub links and pull-request creation for GitHub remotes.
//!
//! The URL is built from the repository's own remote, so a link never needs a
//! token or a network call. `gh` is used only for the one operation that must
//! open a browser form: `gh pr create --web`, which keeps credentials inside
//! the CLI.
//!
//! Items are hidden when the selected remote is not a GitHub remote; a GitHub
//! remote whose `gh` is missing or logged out keeps its links and shows the
//! pull-request entry disabled with the reason.

use std::path::PathBuf;

use gpui::{App, Entity, Window};
use gpui_component::WindowExt as _;
use gpui_component::button::Button;
use gpui_component::menu::PopupMenuItem;
use gpui_component::notification::Notification;
use strata_core::Oid;
use strata_core::tools as core_tools;
use strata_core::tools::GitHubRemote;

use crate::repo_view::RepoView;
use crate::settings::Settings;
use crate::tools::Tooling;

/// The two branch-menu entries GitHub integration adds, plus the pull-request
/// entries that need the branch's own pull request.
pub(crate) struct BranchItems {
    pub(crate) open: PopupMenuItem,
    pub(crate) create_pr: PopupMenuItem,
    /// Set when the branch has a pull request in the cache.
    pub(crate) open_pr: Option<PopupMenuItem>,
    pub(crate) checkout_pr: PopupMenuItem,
}

/// The GitHub remote this repository pushes to, if any.
///
/// `origin` wins over the other remotes, then the first GitHub URL in the
/// configured order. A non-GitHub remote returns `None`, which hides every
/// GitHub item.
pub(crate) fn remote_for(view: &RepoView, cx: &App) -> Option<GitHubRemote> {
    if !Settings::get(cx).tools.gh.enabled {
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
            if let Some(parsed) = GitHubRemote::parse(url, &known) {
                return Some(parsed);
            }
        }
    }
    None
}

/// "Open on GitHub" for a commit.
pub(crate) fn commit_item(
    entity: &Entity<RepoView>,
    commit: Oid,
    cx: &App,
) -> Option<PopupMenuItem> {
    let remote = remote_for(entity.read(cx), cx)?;
    Some(open_item(remote.commit_url(&commit.to_string())))
}

/// "Open on GitHub" for a file at a commit, optionally highlighting a line
/// range.
pub(crate) fn file_item(
    entity: &Entity<RepoView>,
    commit: Oid,
    path: &str,
    lines: Option<(u32, u32)>,
    cx: &App,
) -> Option<PopupMenuItem> {
    let remote = remote_for(entity.read(cx), cx)?;
    Some(open_item(remote.blob_url(&commit.to_string(), path, lines)))
}

/// Branch-menu entries: the tree link, the pull-request form, and the branch's
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
        let label = format!("Open Pull Request #{} on GitHub", pull_request.number);
        PopupMenuItem::new(label).on_click(
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                cx.open_with_system(std::path::Path::new(&url));
            },
        )
    });
    Some(BranchItems {
        open: open_item(remote.tree_url(branch)),
        create_pr: create_pr_item(entity, branch, cx),
        open_pr,
        checkout_pr: checkout_pr_item(entity, &remote, cx),
    })
}

/// "Open on GitHub" for a remote-tracking branch, using its branch name on the
/// remote rather than the `origin/` prefix.
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
    Some(open_item(remote.tree_url(&branch)))
}

/// Adds the pull-request action to a successful push notification, when the
/// branch is on a GitHub remote and `gh` can create one.
pub(crate) fn with_create_pr(
    note: Notification,
    entity: &Entity<RepoView>,
    branch: &str,
    cx: &App,
) -> Notification {
    if remote_for(entity.read(cx), cx).is_none() {
        return note;
    }
    let Some(tooling) = Tooling::get(cx) else {
        return note;
    };
    if !tooling.gh_reason().is_empty() {
        return note;
    }
    let Some(gh) = tooling.gh().map(|info| info.binary.clone()) else {
        return note;
    };
    let Some(workdir) = workdir(entity, cx) else {
        return note;
    };
    let branch = branch.to_owned();
    note.action(move |_, _, _| {
        let gh = gh.clone();
        let workdir = workdir.clone();
        let branch = branch.clone();
        Button::new("notification-create-pr")
            .label("Create Pull Request…")
            .on_click(move |_, window, cx| {
                create_pull_request(gh.clone(), workdir.clone(), branch.clone(), window, cx);
            })
    })
}

fn open_item(url: String) -> PopupMenuItem {
    PopupMenuItem::new("Open on GitHub").on_click(
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            cx.open_with_system(std::path::Path::new(&url));
        },
    )
}

fn create_pr_item(entity: &Entity<RepoView>, branch: &str, cx: &App) -> PopupMenuItem {
    let blocked = gh_blocked(cx);
    let label = match &blocked {
        Some(reason) => format!("Create Pull Request… ({reason})"),
        None => "Create Pull Request…".to_owned(),
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

fn checkout_pr_item(entity: &Entity<RepoView>, remote: &GitHubRemote, cx: &App) -> PopupMenuItem {
    let blocked = gh_for(&remote.host, cx).err().or_else(|| {
        entity
            .read(cx)
            .pull_requests
            .is_checking_out()
            .then(|| "a checkout is running".to_owned())
    });
    let label = match &blocked {
        Some(reason) => format!("Check out Pull Request… ({reason})"),
        None => "Check out Pull Request…".to_owned(),
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

fn gh_blocked(cx: &App) -> Option<String> {
    gh_for(strata_core::tools::GITHUB_HOST, cx).err()
}

/// The `gh` binary when it is installed and logged in to `host`.
///
/// The host check matters: a repository on an enterprise host whose account is
/// not logged in would otherwise get a list call that always fails.
pub(crate) fn gh_for(host: &str, cx: &App) -> Result<PathBuf, String> {
    if !Settings::get(cx).tools.gh.enabled {
        return Err("GitHub CLI integration is off".to_owned());
    }
    let Some(tooling) = Tooling::get(cx) else {
        return Err("gh is not installed.".to_owned());
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
        Some(gh) if !gh.logged_in => Err("gh is not logged in: run gh auth login".to_owned()),
        Some(_) => Err(format!("gh is not logged in to {host}")),
        None => Err(tooling.gh_reason()),
    }
}

fn workdir(entity: &Entity<RepoView>, cx: &App) -> Option<PathBuf> {
    entity
        .read(cx)
        .session
        .as_ref()
        .map(|session| session.git().workdir().to_path_buf())
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
                Notification::error(format!("Could not create the pull request: {error}"))
                    .title("GitHub"),
                cx,
            );
        });
    })
    .detach();
}
