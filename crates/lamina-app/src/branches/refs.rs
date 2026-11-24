//! Names, labels and choices shared by the sidebar and its forms: how a remote
//! branch maps to a local name, what an upstream says about its branch, and
//! which remote branches a branch can track.

use lamina_core::{RefInfo, RefKind, RemoteInfo, UpstreamInfo};

use crate::natural_sort::natural_cmp;

/// One remote branch the set-upstream dialog can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpstreamChoice {
    /// `origin/main`, as the sidebar labels it.
    pub(crate) label: String,
    pub(crate) remote: String,
    pub(crate) remote_branch: String,
}

/// The sidebar label for a branch's upstream: `↑2 ↓1`, `↑3`, `↓1`, `?`,
/// or nothing when the branch is level with or missing its upstream.
pub(crate) fn upstream_label(upstream: &UpstreamInfo) -> Option<String> {
    if upstream.gone {
        return None;
    }
    let (Some(ahead), Some(behind)) = (upstream.ahead, upstream.behind) else {
        return Some("?".to_owned());
    };
    let mut label = String::new();
    if ahead > 0 {
        label.push_str(&t!("branch.upstream.ahead", count = ahead));
    }
    if behind > 0 {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push_str(&t!("branch.upstream.behind", count = behind));
    }
    (!label.is_empty()).then_some(label)
}

/// Why an upstream shows a marker, when it is `?` or gone.
pub(crate) fn upstream_note(upstream: &UpstreamInfo) -> Option<String> {
    if upstream.gone {
        return Some(t!("branch.upstream.gone_tooltip").to_string());
    }
    (upstream.ahead.is_none() || upstream.behind.is_none())
        .then(|| t!("branch.upstream.shallow").to_string())
}

/// The full ref name of a ref the sidebar lists by its short name.
///
/// The walk hides refs by full name, so the prefix has to match the kind: a
/// local `topic` and a remote `topic` are different refs.
pub(crate) fn ref_full_name(kind: RefKind, short_name: &str) -> String {
    match kind {
        RefKind::LocalBranch => format!("refs/heads/{short_name}"),
        RefKind::RemoteBranch => format!("refs/remotes/{short_name}"),
        RefKind::Tag => format!("refs/tags/{short_name}"),
    }
}

/// `origin/feature/login` -> `feature/login`. The remote name is the first path
/// component, which is how `refs/remotes/` names are built.
///
/// `origin/HEAD` is the remote's symbolic default-branch ref, not something
/// that can be checked out, so it maps to nothing.
pub(crate) fn local_name_for_remote(remote: &str) -> Option<&str> {
    let (_, local) = remote.split_once('/')?;
    if local.is_empty() || local == "HEAD" {
        return None;
    }
    Some(local)
}

/// Every remote branch, as the set-upstream dialog lists it: the sidebar's
/// natural order, with the remote's symbolic `HEAD` left out.
pub(crate) fn upstream_choices(refs: &[RefInfo], remotes: &[RemoteInfo]) -> Vec<UpstreamChoice> {
    let mut choices: Vec<UpstreamChoice> = refs
        .iter()
        .filter(|info| info.kind == RefKind::RemoteBranch)
        .filter_map(|info| {
            let (remote, remote_branch) = split_remote_branch(&info.short_name, remotes)?;
            if remote_branch == "HEAD" {
                return None;
            }
            Some(UpstreamChoice {
                label: info.short_name.clone(),
                remote: remote.to_owned(),
                remote_branch: remote_branch.to_owned(),
            })
        })
        .collect();
    choices.sort_by(|a, b| natural_cmp(&a.label, &b.label));
    choices
}

/// Splits `origin/topic` into the remote and the branch on it.
///
/// git accepts remote names that contain `/`, so the configured remote names
/// are matched longest first instead of taking the first component. A tracking
/// ref whose remote is no longer configured falls back to that first component,
/// which is all that is left to go on.
fn split_remote_branch<'a>(
    short_name: &'a str,
    remotes: &'a [RemoteInfo],
) -> Option<(&'a str, &'a str)> {
    let mut best: Option<(&str, &str)> = None;
    for remote in remotes {
        let Some(rest) = short_name
            .strip_prefix(remote.name.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        if best.is_none_or(|(name, _)| remote.name.len() > name.len()) {
            best = Some((remote.name.as_str(), rest));
        }
    }
    best.or_else(|| {
        short_name
            .split_once('/')
            .filter(|(_, rest)| !rest.is_empty())
    })
}

/// `git branch -d` refuses a branch with commits that are not reachable from
/// HEAD; that message is what turns the delete dialog into a force offer.
pub(super) fn is_not_fully_merged(error: &str) -> bool {
    error.contains("not fully merged")
}

/// `git worktree remove` refuses a checkout with local changes; that message is
/// what turns the removal into a force offer. A missing directory is git's
/// `validation failed` instead, and forcing would not help: Prune is the fix.
pub(super) fn is_dirty_worktree(error: &str) -> bool {
    error.contains("contains modified or untracked files")
        || error.contains("use --force to delete it")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn upstream_labels_show_only_the_sides_that_differ() {
        assert_eq!(
            upstream_label(&upstream("origin/main", 2, 1, false)).as_deref(),
            Some("↑2 ↓1")
        );
        assert_eq!(
            upstream_label(&upstream("origin/main", 3, 0, false)).as_deref(),
            Some("↑3")
        );
        assert_eq!(
            upstream_label(&upstream("origin/main", 0, 4, false)).as_deref(),
            Some("↓4")
        );
        assert_eq!(
            upstream_label(&upstream("origin/main", 0, 0, false)),
            None,
            "a branch level with its upstream needs no label"
        );
        assert_eq!(
            upstream_label(&upstream("origin/gone", 0, 0, true)),
            None,
            "a deleted upstream renders an icon rather than a text label"
        );
    }

    #[test]
    fn unknown_counts_render_a_question_mark_with_a_reason() {
        let unknown = unknown_upstream("origin/main");
        assert_eq!(upstream_label(&unknown).as_deref(), Some("?"));
        assert_eq!(
            upstream_note(&unknown).as_deref(),
            Some("Counts unavailable: history is shallow")
        );
        assert_eq!(
            upstream_note(&upstream("origin/main", 0, 0, false)),
            None,
            "a known count needs no explanation"
        );
        assert_eq!(
            upstream_note(&upstream("origin/gone", 0, 0, true)).as_deref(),
            Some("The upstream branch was deleted on the remote"),
            "a gone upstream carries its explanation in its tooltip"
        );
    }

    fn oid(byte: u8) -> lamina_core::Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn remote(name: &str, fetch_url: Option<&str>) -> RemoteInfo {
        RemoteInfo {
            name: name.to_owned(),
            fetch_url: fetch_url.map(str::to_owned),
            push_url: None,
        }
    }

    fn ref_info(kind: RefKind, short_name: &str) -> RefInfo {
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

    #[test]
    fn set_upstream_lists_remote_branches_and_drops_the_symbolic_head() {
        let remotes = [
            remote("origin", Some("/tmp/origin.git")),
            remote("team/one", Some("/tmp/team.git")),
        ];
        let refs = [
            ref_info(RefKind::RemoteBranch, "origin/main"),
            ref_info(RefKind::RemoteBranch, "origin/HEAD"),
            ref_info(RefKind::RemoteBranch, "origin/feature"),
            ref_info(RefKind::RemoteBranch, "team/one/main"),
            ref_info(RefKind::LocalBranch, "main"),
            ref_info(RefKind::Tag, "v1"),
        ];

        let choices = upstream_choices(&refs, &remotes);
        let labels: Vec<&str> = choices.iter().map(|choice| choice.label.as_str()).collect();
        assert_eq!(
            labels,
            ["origin/feature", "origin/main", "team/one/main"],
            "remote branches in natural order, without the remote's HEAD"
        );
        assert_eq!(
            choices[2].remote, "team/one",
            "a remote name with a slash is not split at its first component"
        );
        assert_eq!(choices[2].remote_branch, "main");
    }

    #[test]
    fn a_tracking_ref_of_an_unconfigured_remote_falls_back_to_its_first_part() {
        let refs = [ref_info(RefKind::RemoteBranch, "gone/main")];
        let choices = upstream_choices(&refs, &[]);
        assert_eq!(choices[0].remote, "gone");
        assert_eq!(choices[0].remote_branch, "main");
        assert_eq!(split_remote_branch("origin", &[]), None);
    }

    #[test]
    fn only_an_unmerged_delete_offers_force() {
        assert!(is_not_fully_merged(
            "error: the branch 'topic' is not fully merged\nhint: ..."
        ));
        assert!(!is_not_fully_merged("fatal: branch 'topic' not found"));
        assert!(!is_not_fully_merged("error: cannot delete branch 'main'"));
    }

    #[test]
    fn remote_branches_map_to_the_local_name_after_the_remote() {
        assert_eq!(local_name_for_remote("origin/main"), Some("main"));
        assert_eq!(
            local_name_for_remote("origin/feature/login"),
            Some("feature/login")
        );
        assert_eq!(local_name_for_remote("upstream/fix-1"), Some("fix-1"));
        assert_eq!(local_name_for_remote("origin"), None);
        assert_eq!(local_name_for_remote("origin/"), None);
        assert_eq!(
            local_name_for_remote("origin/HEAD"),
            None,
            "the remote's symbolic HEAD is not a branch"
        );
    }
}
