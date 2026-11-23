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
use lamina_core::Oid;
use lamina_core::tools as core_tools;
use lamina_core::tools::GitHubRemote;

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

/// t!("github.open_on_github").to_string() for a commit.
pub(crate) fn commit_item(
    entity: &Entity<RepoView>,
    commit: Oid,
    cx: &App,
) -> Option<PopupMenuItem> {
    let remote = remote_for(entity.read(cx), cx)?;
    Some(open_item(remote.commit_url(&commit.to_string())))
}

/// t!("github.open_on_github").to_string() for a file at a commit, optionally highlighting a line
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
        let label = t!("github.open_pull_request", number = pull_request.number).to_string();
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

/// t!("github.open_on_github").to_string() for a remote-tracking branch, using its branch name on the
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
/// branch is on a GitHub remote.
///
/// The action opens the branch's existing pull request when the cache knows one
/// is open, and asks `gh` for the creation form when it does not; either way the
/// push marks the cache, because it is the moment the branch may have gained or
/// lost a pull request, and the refresh that follows it must not be throttled.
pub(crate) fn with_create_pr(
    note: Notification,
    view: &RepoView,
    branch: &str,
    cx: &App,
) -> Notification {
    // The push ran on the host. Pointing the local `gh` at that working tree
    // would open a path this machine does not have, so the note stays as it is.
    if view.is_ssh() {
        return note;
    }
    if remote_for(view, cx).is_none() {
        return note;
    }
    view.pull_requests.note_push();

    // The branch's pull request is already open: the form would ask GitHub for
    // a second one, so the notification offers the existing one instead.
    let existing = view
        .pull_requests
        .open_for_branch(branch)
        .map(|pull_request| (pull_request.number, pull_request.url.clone()));
    if let Some((number, url)) = existing {
        return note.action(move |_, _, _| {
            let url = url.clone();
            Button::new("notification-open-pr")
                .label(t!("github.open_pull_request", number = number).to_string())
                .on_click(move |_, _, cx| {
                    cx.open_with_system(std::path::Path::new(&url));
                })
        });
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
    let Some(workdir) = view
        .session
        .as_ref()
        .map(|session| session.git_extras().workdir().to_path_buf())
    else {
        return note;
    };
    let branch = branch.to_owned();
    note.action(move |_, _, _| {
        let gh = gh.clone();
        let workdir = workdir.clone();
        let branch = branch.clone();
        Button::new("notification-create-pr")
            .label(t!("github.create_pr").to_string())
            .on_click(move |_, window, cx| {
                create_pull_request(gh.clone(), workdir.clone(), branch.clone(), window, cx);
            })
    })
}

fn open_item(url: String) -> PopupMenuItem {
    PopupMenuItem::new(t!("github.open_on_github").to_string()).on_click(
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            cx.open_with_system(std::path::Path::new(&url));
        },
    )
}

fn create_pr_item(entity: &Entity<RepoView>, branch: &str, cx: &App) -> PopupMenuItem {
    let blocked = pr_form_blocked(entity.read(cx).is_ssh(), gh_blocked(cx));
    let label = match &blocked {
        Some(reason) => t!("github.create_pr_blocked", reason = reason).to_string(),
        None => t!("github.create_pr").to_string().to_owned(),
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
            .then(|| t!("github.checkout_running").to_string())
    });
    let label = match &blocked {
        Some(reason) => t!("github.checkout_pr_blocked", reason = reason).to_string(),
        None => t!("github.checkout_pr").to_string().to_owned(),
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
    gh_for(lamina_core::tools::GITHUB_HOST, cx).err()
}

/// The `gh` binary when it is installed and logged in to `host`.
///
/// The host check matters: a repository on an enterprise host whose account is
/// not logged in would otherwise get a list call that always fails.
pub(crate) fn gh_for(host: &str, cx: &App) -> Result<PathBuf, String> {
    if !Settings::get(cx).tools.gh.enabled {
        return Err(t!("github.integration_off").to_string().to_owned());
    }
    let Some(tooling) = Tooling::get(cx) else {
        return Err(t!("github.gh_missing").to_string().to_owned());
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
        Some(gh) if !gh.logged_in => Err(t!("github.gh_not_logged_in").to_string().to_owned()),
        Some(_) => Err(t!("github.not_logged_in_host", host = host).to_string()),
        None => Err(tooling.gh_reason()),
    }
}

fn workdir(entity: &Entity<RepoView>, cx: &App) -> Option<PathBuf> {
    entity
        .read(cx)
        .session
        .as_ref()
        .map(|session| session.git_extras().workdir().to_path_buf())
}

/// Why the pull-request form cannot open from here.
///
/// A remote tab has no working tree on this machine for `gh` to run in, so
/// the form stays off however healthy the local `gh` is.
fn pr_form_blocked(ssh: bool, gh_error: Option<String>) -> Option<String> {
    if ssh {
        Some(crate::remote_open::unavailable_reason())
    } else {
        gh_error
    }
}

fn create_pull_request(
    gh: PathBuf,
    workdir: PathBuf,
    branch: String,
    window: &mut Window,
    cx: &mut App,
) {
    let proxy = Settings::get(cx).proxy.clone();
    let handle = window.window_handle();
    let task = cx.background_executor().spawn(async move {
        let policy = crate::proxy::resolve_policy("https://github.com", &proxy);
        core_tools::create_pull_request_with_proxy(&gh, &workdir, &branch, &policy)
            .map_err(|error| error.to_string())
    });
    cx.spawn(async move |cx| {
        let Err(error) = task.await else {
            return;
        };
        let _ = handle.update(cx, |_, window, cx| {
            window.push_notification(
                Notification::error(t!("github.create_pr_error", error = error).to_string())
                    .title(t!("github.title").to_string()),
                cx,
            );
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_tab_cannot_open_the_pull_request_form() {
        rust_i18n::set_locale("en");
        let remote = crate::remote_open::unavailable_reason();
        assert_eq!(pr_form_blocked(true, None), Some(remote.clone()));
        assert_eq!(
            pr_form_blocked(true, Some("gh is not installed".to_owned())),
            Some(remote),
            "the remote reason wins over a missing gh"
        );
        assert_eq!(pr_form_blocked(false, None), None);
        assert_eq!(
            pr_form_blocked(false, Some("gh is not installed".to_owned())),
            Some("gh is not installed".to_owned())
        );
    }
}
