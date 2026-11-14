//! The commit list's data source.
//!
//! The renderer never knows whether it is showing a real repository or the
//! synthetic benchmark: both modes answer the same "what is row `i`?" question
//! through [`Source`], and return borrowed [`RowData`] that owns nothing but the
//! short id and the badge list.

use std::borrow::Cow;
use std::rc::Rc;
use std::sync::Arc;

use strata_core::graph::GraphRow;
use strata_core::session::{CommitList, RepoSnapshot};
use strata_core::{CommitInfo, HeadInfo, Oid, RefInfo, RefKind, StashInfo};

use crate::fake::{self, FakeRepo};

/// Branches the synthetic sidebar lists, so `--fake` exercises the same chrome.
const FAKE_BRANCHES: [&str; 10] = [
    "main",
    "t3/gpui-graph-validation",
    "graph/lane-layout",
    "diff/word-emphasis",
    "repo/status-watch",
    "ui/theme-pass",
    "fix/windows-paths",
    "git_cli/spawn-env",
    "perf/scrolling",
    "docs/phase-0",
];

/// One commit-list row, borrowed from whichever source produced it.
pub struct RowData<'a> {
    pub graph: GraphRow<'a>,
    /// Abbreviated object id, e.g. `4f2a1c9`.
    pub id: Cow<'a, str>,
    pub summary: Cow<'a, str>,
    pub author: Cow<'a, str>,
    /// Author address; only the avatar colour uses it.
    pub email: Cow<'a, str>,
    /// Seconds since the Unix epoch.
    pub time: i64,
    pub badges: Vec<Badge<'a>>,
    /// The commit is a shallow clone's boundary, so its missing parents mean
    /// "history was not fetched" rather than "history starts here".
    pub shallow_boundary: bool,
}

/// A ref badge drawn before the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge<'a> {
    pub label: Cow<'a, str>,
    pub kind: RefKind,
    /// The local branch HEAD is on; rendered more strongly than the others.
    pub head: bool,
}

/// The commit list, either live or synthetic. Cloning shares the underlying
/// snapshot/chunks, so the render closure can own a copy.
#[derive(Clone)]
pub enum Source {
    Fake(Rc<FakeRepo>),
    Repo(Arc<RepoSnapshot>),
}

impl Source {
    pub fn len(&self) -> usize {
        match self {
            Source::Fake(repo) => repo.commits.len(),
            Source::Repo(snapshot) => snapshot.commits.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True once the walk reached the end of history (always for synthetic data).
    pub fn complete(&self) -> bool {
        match self {
            Source::Fake(_) => true,
            Source::Repo(snapshot) => snapshot.commits.complete,
        }
    }

    pub fn row(&self, index: usize) -> Option<RowData<'_>> {
        match self {
            Source::Fake(repo) => {
                let commit = repo.commits.get(index)?;
                let author = fake::author(commit.author);
                Some(RowData {
                    graph: repo.graph.get(index)?,
                    id: Cow::Owned(fake::short_id(commit.id)),
                    summary: Cow::Owned(fake::summary(commit.summary_seed)),
                    author: Cow::Borrowed(author),
                    email: Cow::Owned(fake_email(author)),
                    time: commit.time,
                    badges: fake_badges(index),
                    shallow_boundary: false,
                })
            }
            Source::Repo(snapshot) => {
                let (commit, graph) = snapshot.commits.get(index)?;
                let indices = snapshot
                    .refs_by_commit
                    .get(&commit.id)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                Some(RowData {
                    graph,
                    id: Cow::Owned(short_oid(commit.id)),
                    summary: Cow::Borrowed(commit.summary.as_str()),
                    author: Cow::Borrowed(commit.author_name.as_str()),
                    email: Cow::Borrowed(commit.author_email.as_str()),
                    time: commit.author_time,
                    badges: badges_for_commit(
                        &snapshot.refs,
                        indices,
                        snapshot.head.as_ref(),
                        commit.id,
                    ),
                    shallow_boundary: commit.shallow_boundary,
                })
            }
        }
    }

    /// The commit at `index` in a real repository; `None` for synthetic rows.
    pub fn commit(&self, index: usize) -> Option<&CommitInfo> {
        match self {
            Source::Fake(_) => None,
            Source::Repo(snapshot) => snapshot.commits.get(index).map(|(commit, _)| commit),
        }
    }

    /// The commit at `index` in a real repository; `None` for synthetic rows.
    pub fn commit_id(&self, index: usize) -> Option<Oid> {
        match self {
            Source::Fake(_) => None,
            Source::Repo(snapshot) => snapshot.commits.get(index).map(|(commit, _)| commit.id),
        }
    }

    pub fn head(&self) -> Option<&HeadInfo> {
        match self {
            Source::Fake(_) => None,
            Source::Repo(snapshot) => snapshot.head.as_ref(),
        }
    }

    pub fn sidebar(&self) -> SidebarData {
        match self {
            Source::Fake(_) => SidebarData {
                head_branch: Some("main".into()),
                working_tree: (0, 0),
                local: FAKE_BRANCHES.iter().map(|name| (*name).into()).collect(),
                remote: Vec::new(),
                tags: Vec::new(),
                stashes: Vec::new(),
            },
            Source::Repo(snapshot) => {
                let mut data = SidebarData {
                    head_branch: snapshot.head.as_ref().map(|head| match head {
                        HeadInfo::Branch { short_name, .. } => short_name.clone(),
                        HeadInfo::Detached { .. } => "detached HEAD".into(),
                    }),
                    working_tree: (snapshot.status.staged.len(), snapshot.status.unstaged.len()),
                    local: Vec::new(),
                    remote: Vec::new(),
                    tags: Vec::new(),
                    stashes: snapshot
                        .stashes
                        .iter()
                        .map(|stash: &StashInfo| {
                            format!("stash@{{{}}}: {}", stash.index, stash.message)
                        })
                        .collect(),
                };
                for reference in snapshot.refs.iter() {
                    let list = match reference.kind {
                        RefKind::LocalBranch => &mut data.local,
                        RefKind::RemoteBranch => &mut data.remote,
                        RefKind::Tag => &mut data.tags,
                    };
                    list.push(reference.short_name.clone());
                }
                data
            }
        }
    }
}

/// Everything the sidebar lists, gathered from one source.
pub struct SidebarData {
    /// Branch HEAD is on, emphasised in the local list.
    pub head_branch: Option<String>,
    /// `(staged, unstaged)` counts from the worktree status.
    pub working_tree: (usize, usize),
    pub local: Vec<String>,
    pub remote: Vec<String>,
    pub tags: Vec<String>,
    /// Already formatted as `stash@{N}: message`.
    pub stashes: Vec<String>,
}

/// Badges for one commit, in the order they are drawn: local, remote, tag.
///
/// `indices` point into `refs` (the snapshot's `refs_by_commit` map); the map's
/// order carries no meaning, so sorting here keeps the row stable between
/// renders. HEAD is flagged on the local branch that matches, or shown as a
/// synthetic badge when HEAD is detached at this commit.
pub fn badges_for_commit<'a>(
    refs: &'a [RefInfo],
    indices: &[usize],
    head: Option<&HeadInfo>,
    commit: Oid,
) -> Vec<Badge<'a>> {
    let head_branch = match head {
        Some(HeadInfo::Branch { short_name, .. }) => Some(short_name.as_str()),
        _ => None,
    };

    let mut badges: Vec<Badge<'a>> = indices
        .iter()
        .filter_map(|&index| refs.get(index))
        .map(|reference| Badge {
            label: Cow::Borrowed(reference.short_name.as_str()),
            kind: reference.kind,
            head: reference.kind == RefKind::LocalBranch
                && head_branch == Some(reference.short_name.as_str()),
        })
        .collect();

    badges.sort_by(|a, b| {
        ref_rank(a.kind)
            .cmp(&ref_rank(b.kind))
            .then_with(|| a.label.cmp(&b.label))
    });

    if let Some(HeadInfo::Detached { target }) = head
        && *target == commit
    {
        badges.insert(
            0,
            Badge {
                label: Cow::Borrowed("HEAD"),
                kind: RefKind::LocalBranch,
                head: true,
            },
        );
    }

    badges
}

fn ref_rank(kind: RefKind) -> u8 {
    match kind {
        RefKind::LocalBranch => 0,
        RefKind::RemoteBranch => 1,
        RefKind::Tag => 2,
    }
}

/// Synthetic badges so the shared badge renderer is exercised by `--fake`.
fn fake_badges(index: usize) -> Vec<Badge<'static>> {
    let badges: &[(&'static str, RefKind, bool)] = match index {
        0 => &[("main", RefKind::LocalBranch, true)],
        3 => &[("origin/main", RefKind::RemoteBranch, false)],
        7 => &[("v0.1", RefKind::Tag, false)],
        _ => &[],
    };
    badges
        .iter()
        .map(|(label, kind, head)| Badge {
            label: Cow::Borrowed(*label),
            kind: *kind,
            head: *head,
        })
        .collect()
}

/// A stable synthetic address so `--fake` avatars get a colour of their own.
fn fake_email(author: &str) -> String {
    let local: String = author
        .chars()
        .map(|character| {
            if character == ' ' {
                '.'
            } else {
                character.to_ascii_lowercase()
            }
        })
        .collect();
    format!("{local}@strata.invalid")
}

/// Seven hex digits is what the status columns need; full ids make the list
/// impossible to scan.
pub fn short_oid(oid: Oid) -> String {
    let hex = oid.to_hex().to_string();
    hex[..7.min(hex.len())].to_owned()
}

/// Title for the window, from the repository name and HEAD.
pub fn window_title(repo: &str, head: Option<&HeadInfo>) -> String {
    let branch = match head {
        Some(HeadInfo::Branch { short_name, .. }) => short_name.as_str(),
        Some(HeadInfo::Detached { .. }) => "detached",
        // The first snapshot has not been published yet.
        None => "loading",
    };
    format!("Strata — {repo} ({branch})")
}

/// The widest graph row seen so far.
///
/// History arrives in chunks, so the width only ever grows: each snapshot is
/// asked for the rows after `scanned` and nothing is revisited. A reload that
/// replaces the commit list with a different one restarts the scan.
///
/// Rows indent themselves from their own width, so no column is sized from this
/// maximum; the tests keep asserting that the chunked scan works.
#[derive(Debug, Default)]
pub struct LaneWidth {
    max: u16,
    scanned: usize,
    first: Option<Oid>,
}

impl LaneWidth {
    /// Widens the scan for a producer that already knows its maximum, such as
    /// the synthetic graph.
    pub fn observe_max(&mut self, width: u16) {
        self.max = self.max.max(width);
    }

    pub fn observe(&mut self, commits: &CommitList) {
        let first = commits.get(0).map(|(commit, _)| commit.id);
        if first != self.first || commits.len() < self.scanned {
            self.max = 0;
            self.scanned = 0;
        }
        self.first = first;

        for index in self.scanned..commits.len() {
            if let Some((_, row)) = commits.get(index) {
                self.max = self.max.max(row.width);
            }
        }
        self.scanned = commits.len();
    }

    /// At least one lane so an empty (or still loading) list still scans a
    /// lane.
    ///
    /// Rows size their own graph, but the commit list lays out its columns
    /// from the widest graph it will draw, so this is read once per frame.
    pub fn widest(&self) -> u16 {
        self.max.max(1)
    }

    /// Rows accounted for so far; tests use it to prove chunks are not rescanned.
    #[cfg(test)]
    pub fn scanned(&self) -> usize {
        self.scanned
    }
}

/// `YYYY-MM-DD HH:MM` in UTC. Hand-rolled so the app does not pull in a date
/// crate just for a column.
pub fn format_time(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3600;
    let minute = (seconds % 3600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Howard Hinnant's `civil_from_days`, with the epoch shifted to 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::CommitInfo;
    use strata_core::graph::GraphRows;

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn reference(short_name: &str, kind: RefKind, target: Oid) -> RefInfo {
        RefInfo {
            full_name: match kind {
                RefKind::LocalBranch => format!("refs/heads/{short_name}"),
                RefKind::RemoteBranch => format!("refs/remotes/{short_name}"),
                RefKind::Tag => format!("refs/tags/{short_name}"),
            },
            short_name: short_name.into(),
            kind,
            target,
        }
    }

    fn commit_info(index: usize) -> CommitInfo {
        CommitInfo {
            id: oid(index as u8),
            parents: Vec::new(),
            summary: format!("commit {index}"),
            author_name: "Strata Test".into(),
            author_email: "test@example.invalid".into(),
            author_time: index as i64,
            committer_time: index as i64,
            shallow_boundary: false,
        }
    }

    fn commit_list(range: std::ops::Range<usize>, widths: &[u16]) -> CommitList {
        let mut rows = GraphRows::new();
        for width in widths {
            rows.push_row(0, 0, *width, &[], &[]);
        }
        let mut list = CommitList::default();
        list.extend(range.map(commit_info).collect(), rows);
        list
    }

    #[test]
    fn badges_are_sorted_local_remote_tag_and_flag_head() {
        let (local, remote, tag) = (oid(1), oid(2), oid(3));
        let refs = vec![
            reference("origin/main", RefKind::RemoteBranch, remote),
            reference("v1.0", RefKind::Tag, tag),
            reference("main", RefKind::LocalBranch, local),
        ];
        let head = HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(local),
        };

        let badges = badges_for_commit(&refs, &[1, 2, 0], Some(&head), local);

        let labels: Vec<&str> = badges.iter().map(|b| b.label.as_ref()).collect();
        assert_eq!(labels, ["main", "origin/main", "v1.0"]);
        assert_eq!(
            badges.iter().map(|b| b.kind).collect::<Vec<_>>(),
            [RefKind::LocalBranch, RefKind::RemoteBranch, RefKind::Tag]
        );
        assert_eq!(
            badges.iter().filter(|b| b.head).count(),
            1,
            "only the branch HEAD is on is emphasised"
        );
        assert!(badges[0].head);
        assert!(
            !badges[1].head && !badges[2].head,
            "remote and tag are not HEAD"
        );
    }

    #[test]
    fn detached_head_adds_a_badge_at_its_commit_only() {
        let target = oid(9);
        let refs = vec![reference("main", RefKind::LocalBranch, target)];
        let head = HeadInfo::Detached { target };

        let here = badges_for_commit(&refs, &[0], Some(&head), target);
        assert_eq!(here[0].label, "HEAD");
        assert_eq!(here[0].kind, RefKind::LocalBranch);
        assert!(here[0].head);
        assert_eq!(here[1].label, "main");
        assert!(!here[1].head, "a detached HEAD is not on the branch");

        let elsewhere = badges_for_commit(&refs, &[0], Some(&head), oid(10));
        assert_eq!(elsewhere.len(), 1);
        assert_eq!(elsewhere[0].label, "main");
    }

    #[test]
    fn lane_width_only_scans_new_rows_and_never_shrinks_in_one_history() {
        let mut lanes = LaneWidth::default();
        lanes.observe(&commit_list(0..3, &[1, 4, 2]));
        assert_eq!(lanes.widest(), 4);
        assert_eq!(lanes.scanned(), 3);

        let mut grown = commit_list(0..3, &[1, 4, 2]);
        grown.extend(commit_info_range(3..6), rows_with_widths(&[2, 7, 3]));
        lanes.observe(&grown);
        assert_eq!(lanes.widest(), 7);
        assert_eq!(lanes.scanned(), 6, "the first chunk is not revisited");
    }

    #[test]
    fn lane_width_restarts_for_a_replaced_history() {
        let mut lanes = LaneWidth::default();
        lanes.observe(&commit_list(0..4, &[6, 6, 6, 6]));
        assert_eq!(lanes.widest(), 6);

        // A reload publishes a shorter list whose first commit is different.
        let mut replaced = CommitList::default();
        replaced.extend(
            vec![CommitInfo {
                id: oid(200),
                ..commit_info(0)
            }],
            rows_with_widths(&[2]),
        );
        lanes.observe(&replaced);
        assert_eq!(lanes.widest(), 2, "a fresh history starts a fresh maximum");
        assert_eq!(lanes.scanned(), 1);
    }

    #[test]
    fn window_title_names_the_repository_and_head() {
        let head = HeadInfo::Branch {
            short_name: "feat/ui".into(),
            target: None,
        };
        assert_eq!(
            window_title("strata", Some(&head)),
            "Strata — strata (feat/ui)"
        );
        assert_eq!(
            window_title("strata", Some(&HeadInfo::Detached { target: oid(1) })),
            "Strata — strata (detached)"
        );
        assert_eq!(window_title("strata", None), "Strata — strata (loading)");
    }

    #[test]
    fn short_oid_is_seven_hex_digits() {
        assert_eq!(short_oid(oid(0xab)), "abababa");
    }

    #[test]
    fn fake_source_reuses_the_real_row_shape() {
        let source = Source::Fake(Rc::new(FakeRepo::generate(20)));
        assert_eq!(source.len(), 20);
        assert!(source.complete());

        let first = source.row(0).expect("first row");
        assert!(first.badges[0].head);
        assert_eq!(first.badges[0].kind, RefKind::LocalBranch);
        assert!(first.summary.contains(" in "));
        assert_eq!(first.id.len(), 7);
        assert!(source.row(20).is_none());
    }

    #[test]
    fn dates_render_as_iso() {
        assert_eq!(format_time(0), "1970-01-01 00:00");
        assert_eq!(format_time(1_794_528_000), "2026-11-13 00:00");
    }

    fn commit_info_range(range: std::ops::Range<usize>) -> Vec<CommitInfo> {
        range.map(commit_info).collect()
    }

    fn rows_with_widths(widths: &[u16]) -> GraphRows {
        let mut rows = GraphRows::new();
        for width in widths {
            rows.push_row(0, 0, *width, &[], &[]);
        }
        rows
    }
}
