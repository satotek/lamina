//! The sidebar's flattened list: which rows a section shows, how shared path
//! segments fold into folders, and how a refresh keeps the viewport's row.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{Pixels, ScrollStrategy, point};
use lamina_core::repo::SubmoduleStatus;
use lamina_core::{RefKind, RemoteInfo, UpstreamInfo};

use crate::data::SidebarData;
use crate::natural_sort::{natural_cmp, natural_sorted, natural_sorted_owned};
use crate::style::Design;

use super::refs::{local_name_for_remote, upstream_label, upstream_note};
use super::rows::{submodule_detail, worktree_detail, worktree_label};
use super::{Branches, ITEM_HEIGHT, RevealedRef, Section, SidebarExtras, same_path};

/// One line of the sidebar's virtualised list.
///
/// The list owns its strings, so drawing a frame holds nothing borrowed from
/// the snapshot; the parts that change without the refs (the revealed row,
/// HEAD, busy) stay out and are read while the visible rows are built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SidebarItem {
    SectionHeader {
        section: Section,
        count: usize,
        open: bool,
    },
    /// A remote's own collapsible heading, above its branch list.
    RemoteGroup {
        name: String,
        /// Shown as the header's tooltip, for a remote that was never fetched.
        fetch_url: Option<String>,
        count: usize,
        collapsed: bool,
    },
    /// A folder built from a path segment shared by several refs.
    Folder {
        /// Section and path the fold is stored under; see [`folder_key`].
        key: String,
        /// The last path segment, which is the row's label.
        segment: String,
        /// How many refs sit below the folder.
        count: usize,
        open: bool,
        depth: usize,
        /// The section the folder belongs to, for its indent under a remote.
        section: Section,
    },
    LocalBranch {
        name: String,
        /// The row's label: the last path segment, or the whole name while a
        /// filter is active.
        label: String,
        /// Position in the section's natural order, for the element id.
        index: usize,
        depth: usize,
        /// `↑2 ↓1`, `?`, or nothing when level with or missing the upstream.
        upstream: Option<String>,
        /// Whether the tracked upstream branch was deleted on the remote.
        upstream_gone: bool,
        /// Why the upstream shows a marker, for its tooltip.
        upstream_note: Option<String>,
    },
    RemoteBranch {
        name: String,
        /// The row's label: the path below the remote, or its last segment
        /// when a folder provides the rest.
        label: String,
        index: usize,
        depth: usize,
        /// The local branch `origin/foo` stands for; `None` for `origin/HEAD`.
        local_name: Option<String>,
        /// Whether that local branch exists already.
        existing: bool,
    },
    Tag {
        name: String,
        label: String,
        index: usize,
        depth: usize,
    },
    Stash {
        /// `stash@{N}: message`, as the snapshot lists it.
        name: String,
        index: usize,
    },
    /// One entry of `.gitmodules` and how its checkout compares with HEAD.
    Submodule {
        index: usize,
        name: String,
        /// Repository-relative path, which is what an operation addresses.
        path: String,
        /// Path, URL and both commits, for the row's tooltip.
        detail: String,
        status: SubmoduleStatus,
    },
    /// One checkout registered under `worktrees/`, the main one included.
    Worktree {
        index: usize,
        /// Branch name, or `detached <short id>`.
        label: String,
        path: String,
        /// Path and lock reason, for the row's tooltip.
        detail: String,
        /// The lock reason alone, for the t!("branch.worktree.locked").to_string() badge's tooltip.
        lock_reason: Option<String>,
        is_main: bool,
        locked: bool,
        prunable: bool,
        /// This window's own worktree, drawn like the HEAD branch.
        current: bool,
    },
    /// The line a remote with no fetched branches shows under its heading.
    Placeholder { text: String, remote: String },
}

/// Identity of a list position that survives a snapshot refresh, so the row
/// under the viewport can be found again by what it is rather than by index.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ItemKey {
    Section(Section),
    RemoteGroup(String),
    Folder(String),
    LocalBranch(String),
    RemoteBranch(String),
    Tag(String),
    Stash(usize),
    Submodule(String),
    Worktree(String),
    RemoteEmpty(String),
}

impl SidebarItem {
    fn key(&self) -> ItemKey {
        match self {
            SidebarItem::SectionHeader { section, .. } => ItemKey::Section(*section),
            SidebarItem::RemoteGroup { name, .. } => ItemKey::RemoteGroup(name.clone()),
            SidebarItem::Folder { key, .. } => ItemKey::Folder(key.clone()),
            SidebarItem::LocalBranch { name, .. } => ItemKey::LocalBranch(name.clone()),
            SidebarItem::RemoteBranch { name, .. } => ItemKey::RemoteBranch(name.clone()),
            SidebarItem::Tag { name, .. } => ItemKey::Tag(name.clone()),
            SidebarItem::Stash { index, .. } => ItemKey::Stash(*index),
            SidebarItem::Submodule { path, .. } => ItemKey::Submodule(path.clone()),
            SidebarItem::Worktree { path, .. } => ItemKey::Worktree(path.clone()),
            SidebarItem::Placeholder { remote, .. } => ItemKey::RemoteEmpty(remote.clone()),
        }
    }

    /// Whether this item is the row `key` names. Compared without building a
    /// key, so finding the scroll anchor does not allocate per entry.
    fn is(&self, key: &ItemKey) -> bool {
        match (self, key) {
            (SidebarItem::SectionHeader { section, .. }, ItemKey::Section(key)) => section == key,
            (SidebarItem::RemoteGroup { name, .. }, ItemKey::RemoteGroup(key)) => name == key,
            (SidebarItem::Folder { key: folder, .. }, ItemKey::Folder(key)) => folder == key,
            (SidebarItem::LocalBranch { name, .. }, ItemKey::LocalBranch(key)) => name == key,
            (SidebarItem::RemoteBranch { name, .. }, ItemKey::RemoteBranch(key)) => name == key,
            (SidebarItem::Tag { name, .. }, ItemKey::Tag(key)) => name == key,
            (SidebarItem::Stash { index, .. }, ItemKey::Stash(key)) => index == key,
            (SidebarItem::Submodule { path, .. }, ItemKey::Submodule(key)) => path == key,
            (SidebarItem::Worktree { path, .. }, ItemKey::Worktree(key)) => path == key,
            (SidebarItem::Placeholder { remote, .. }, ItemKey::RemoteEmpty(key)) => remote == key,
            _ => false,
        }
    }
}

/// The flattened list, rebuilt only when its inputs change, so a frame costs
/// the rows on screen rather than the size of the repository.
#[derive(Default)]
pub(super) struct ItemCache {
    key: Option<ItemsKey>,
    items: Rc<Vec<SidebarItem>>,
}

#[derive(PartialEq, Eq)]
struct ItemsKey {
    /// Snapshot generation; every publish bumps it.
    revision: u64,
    /// Generation of the last submodule/worktree answer, which arrives outside
    /// the snapshot and changes those rows on its own.
    lists: u64,
    /// Bumped when a section, a remote group or a folder folds.
    local: u64,
    /// The ref a "show only" scope limits the list to; a different filter
    /// lists different rows without a new snapshot.
    filter: Option<String>,
    show_remote_branches: bool,
    show_tags: bool,
    show_stashes: bool,
}

/// The list drawn last frame, so a refresh can put the same row back under
/// the viewport instead of the same index.
#[derive(Default)]
pub(super) struct ScrollAnchor {
    items: Option<Rc<Vec<SidebarItem>>>,
}

/// The fold key of a folder: the section, the remote whose branches it groups,
/// and the path inside that section. Two remotes can both have a `t3` folder,
/// and a tag and a branch share a segment without sharing a fold.
fn folder_key(section: Section, remote: Option<&str>, path: &str) -> String {
    match remote {
        Some(remote) => format!("{}:{remote}:{path}", section.id()),
        None => format!("{}:{path}", section.id()),
    }
}

/// The folder keys above `name`, e.g. `a`, `a/b` for `a/b/c`.
fn folder_ancestors(section: Section, remote: Option<&str>, name: &str) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut end = 0;
    while let Some(offset) = name[end..].find('/') {
        end += offset;
        ancestors.push(folder_key(section, remote, &name[..end]));
        end += 1;
    }
    ancestors
}

/// One node of the tree a section's ref names build.
#[derive(Debug, PartialEq, Eq)]
enum RefNode<T> {
    Leaf {
        /// The row's label: the last path segment, or the whole remaining path
        /// when this leaf stands in for a folder that would have split it.
        label: String,
        /// The path inside the section.
        path: String,
        value: T,
    },
    Folder {
        /// The path inside the section, e.g. `t3`.
        path: String,
        /// The last path segment, the folder row's label.
        segment: String,
        children: Vec<RefNode<T>>,
    },
}

impl<T> RefNode<T> {
    /// How many refs sit below the node.
    fn leaf_count(&self) -> usize {
        match self {
            RefNode::Leaf { .. } => 1,
            RefNode::Folder { children, .. } => children.iter().map(RefNode::leaf_count).sum(),
        }
    }
}

/// Whether a ref passes the sidebar's filter, which is the "show only" scope:
/// the whole name is searched, so a scope names the ref it limits history to.
fn matches_ref(filter: &str, name: &str) -> bool {
    name.to_lowercase().contains(&filter.trim().to_lowercase())
}

/// One local branch before it is placed in the folder tree.
struct LocalLeaf {
    name: String,
    index: usize,
}

/// One remote branch before it is placed in the folder tree.
struct RemoteLeaf {
    name: String,
    index: usize,
    local_name: Option<String>,
    existing: bool,
}

/// One tag before it is placed in the folder tree.
struct TagLeaf {
    name: String,
    index: usize,
}

/// Groups `(path, value)` entries into folders by path segment.
///
/// A folder around a single branch is not worth a row: it is replaced by a leaf
/// that keeps the whole path, so `t3/blame-view` reads as one line unless the
/// repository has another `t3/...` ref beside it.
fn ref_tree<T>(entries: Vec<(String, T)>) -> Vec<RefNode<T>> {
    grouped(entries, "")
}

fn grouped<T>(mut entries: Vec<(String, T)>, prefix: &str) -> Vec<RefNode<T>> {
    entries.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    let mut nodes = Vec::new();
    let mut entries = entries.into_iter().peekable();
    while let Some((path, value)) = entries.next() {
        let Some((segment, rest)) = path.split_once('/') else {
            nodes.push(RefNode::Leaf {
                label: path.clone(),
                path: format!("{prefix}{path}"),
                value,
            });
            continue;
        };
        let segment_prefix = format!("{segment}/");
        let mut children = vec![(rest.to_owned(), value)];
        while let Some((next, value)) =
            entries.next_if(|(next, _)| next.starts_with(&segment_prefix))
        {
            let rest = next
                .strip_prefix(&segment_prefix)
                .unwrap_or(&next)
                .to_owned();
            children.push((rest, value));
        }
        let mut children = grouped(children, &format!("{prefix}{segment}/"));
        // A folder that holds one branch is indistinguishable from that branch
        // at this level, so the leaf takes the path the folder would have split.
        if children.len() == 1 && matches!(children.first(), Some(RefNode::Leaf { .. })) {
            let Some(RefNode::Leaf { path, value, .. }) = children.pop() else {
                unreachable!("a one-child folder's child is a leaf");
            };
            let label = path.strip_prefix(prefix).unwrap_or(&path).to_owned();
            nodes.push(RefNode::Leaf { label, path, value });
            continue;
        }
        nodes.push(RefNode::Folder {
            path: format!("{prefix}{segment}"),
            segment: segment.to_owned(),
            children,
        });
    }
    nodes.sort_by(|a, b| match (a, b) {
        (RefNode::Leaf { label: a, .. }, RefNode::Leaf { label: b, .. }) => natural_cmp(a, b),
        (RefNode::Folder { path: a, .. }, RefNode::Folder { path: b, .. }) => natural_cmp(a, b),
        (RefNode::Leaf { label, .. }, RefNode::Folder { path, .. }) => natural_cmp(label, path),
        (RefNode::Folder { path, .. }, RefNode::Leaf { label, .. }) => natural_cmp(path, label),
    });
    nodes
}

/// Pushes the folder tree's rows: a folder row per branch-point, its leaves
/// below, indented a step further.
#[allow(clippy::too_many_arguments)]
fn push_ref_nodes<T>(
    items: &mut Vec<SidebarItem>,
    nodes: Vec<RefNode<T>>,
    depth: usize,
    section: Section,
    remote: Option<&str>,
    folded: &HashSet<String>,
    forced: &HashSet<String>,
    leaf: &mut impl FnMut(T, &str, usize) -> SidebarItem,
) {
    for node in nodes {
        match node {
            RefNode::Leaf { label, value, .. } => items.push(leaf(value, &label, depth)),
            RefNode::Folder {
                path,
                segment,
                children,
            } => {
                let key = folder_key(section, remote, &path);
                let open = !folded.contains(&key) || forced.contains(&key);
                let count = children.iter().map(RefNode::leaf_count).sum();
                items.push(SidebarItem::Folder {
                    key,
                    segment,
                    count,
                    open,
                    depth,
                    section,
                });
                if open {
                    push_ref_nodes(
                        items,
                        children,
                        depth + 1,
                        section,
                        remote,
                        folded,
                        forced,
                        leaf,
                    );
                }
            }
        }
    }
}

/// One remote's branch list in the sidebar.
#[derive(Debug, PartialEq, Eq)]
struct RemoteGroup {
    name: String,
    /// Shown as the header's tooltip, so a remote that has never been fetched
    /// still tells the user where it points.
    fetch_url: Option<String>,
    branches: Vec<String>,
}

/// `origin/main`, `origin/feat/x`, `upstream/main` grouped by their first path
/// component. Every configured remote gets a group even without branches, and
/// a remote-tracking ref for a remote that is no longer configured still gets
/// one so it cannot silently disappear.
fn remote_groups(entries: &[String], remotes: &[RemoteInfo]) -> Vec<RemoteGroup> {
    let mut groups: Vec<RemoteGroup> = remotes
        .iter()
        .map(|remote| RemoteGroup {
            name: remote.name.clone(),
            fetch_url: remote.fetch_url.clone(),
            branches: Vec::new(),
        })
        .collect();
    for name in entries {
        let group = name.split('/').next().unwrap_or(name).to_owned();
        match groups.iter_mut().find(|existing| existing.name == group) {
            Some(existing) => existing.branches.push(name.clone()),
            None => groups.push(RemoteGroup {
                name: group,
                fetch_url: None,
                branches: vec![name.clone()],
            }),
        }
    }
    for group in &mut groups {
        group.branches.sort_by(|a, b| natural_cmp(a, b));
    }
    groups
}

impl Branches {
    /// The flattened list, rebuilt only when the snapshot or a fold changes.
    ///
    /// `revision` is the snapshot's generation, which every publish bumps; that
    /// is what lets the cached strings outlive the frame they were copied in.
    pub(super) fn items(
        &self,
        data: SidebarData,
        revision: u64,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
        extras: &SidebarExtras<'_>,
        design: &Design,
    ) -> Rc<Vec<SidebarItem>> {
        let key = ItemsKey {
            revision,
            lists: extras.revision,
            local: self.local_revision,
            filter: self.filter.clone(),
            show_remote_branches: design.show_remote_branches,
            show_tags: design.show_tags,
            show_stashes: design.show_stashes,
        };
        let mut cache = self.cache.borrow_mut();
        if cache.key.as_ref() == Some(&key) {
            return cache.items.clone();
        }
        let items = Rc::new(self.flatten(data, upstreams, remotes, extras, design));
        cache.key = Some(key);
        cache.items = items.clone();
        items
    }

    /// Every visible line of the section list, in drawing order: a heading for
    /// each shown section, then its rows, grouped into folders where their
    /// paths share a segment. A collapsed section or folder contributes its
    /// row alone, and a section with nothing in it is just its heading.
    fn flatten(
        &self,
        data: SidebarData,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
        extras: &SidebarExtras<'_>,
        design: &Design,
    ) -> Vec<SidebarItem> {
        // Counted before the lists are destructured: the heading's number and
        // the rail's are the same number.
        let counts = Section::ALL.map(|section| section.count(&data, extras));
        let SidebarData {
            local,
            remote,
            mut tags,
            mut stashes,
            head_branch,
            ..
        } = data;
        let mut items = Vec::with_capacity(
            local.len()
                + remote.len()
                + tags.len()
                + stashes.len()
                + extras.submodules.len()
                + extras.worktrees.len()
                + 10,
        );
        self.open_head_folder(head_branch.as_deref());

        // The folder the history was last revealed from stays open for as long
        // as its ref row is highlighted, so the tint is never hidden in a fold.
        let mut forced = HashSet::new();
        if let Some(head) = head_branch.as_deref() {
            forced.extend(folder_ancestors(Section::Local, None, head));
        }
        if let Some(RevealedRef::Ref(kind, name)) = self.reveal.revealed.as_ref() {
            match kind {
                RefKind::LocalBranch => {
                    forced.extend(folder_ancestors(Section::Local, None, name));
                }
                RefKind::Tag => {
                    forced.extend(folder_ancestors(Section::Tags, None, name));
                }
                RefKind::RemoteBranch => {
                    if let Some((remote_name, path)) = name.split_once('/') {
                        forced.extend(folder_ancestors(Section::Remote, Some(remote_name), path));
                    }
                }
            }
        }
        let folded = self.folders.borrow();
        // The "show only" scope is the sidebar's filter: while it is on, the
        // list shows the refs it matches by their whole path and cannot group
        // them into folders, which would hide the match behind a row.
        let filter = self
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|filter| !filter.is_empty());

        for section in Section::ALL {
            if section.hidden(design) || !section.present(extras) {
                continue;
            }
            let count = counts[section.index()];
            let open = !self.collapsed[section.index()];
            items.push(SidebarItem::SectionHeader {
                section,
                count,
                open,
            });
            if !open {
                continue;
            }

            match section {
                Section::Local => {
                    let entries: Vec<(String, LocalLeaf)> = natural_sorted(&local)
                        .into_iter()
                        .enumerate()
                        .map(|(index, name)| {
                            (
                                name.to_owned(),
                                LocalLeaf {
                                    name: name.to_owned(),
                                    index,
                                },
                            )
                        })
                        .collect();
                    let mut leaf = |value: LocalLeaf, label: &str, depth: usize| {
                        let upstream = upstreams.get(&value.name);
                        SidebarItem::LocalBranch {
                            name: value.name,
                            label: label.to_owned(),
                            index: value.index,
                            depth,
                            upstream: upstream.and_then(upstream_label),
                            upstream_gone: upstream.is_some_and(|info| info.gone),
                            upstream_note: upstream.and_then(upstream_note),
                        }
                    };
                    match filter {
                        Some(filter) => {
                            for (path, entry) in entries {
                                if matches_ref(filter, &entry.name) {
                                    items.push(leaf(entry, &path, 0));
                                }
                            }
                        }
                        None => push_ref_nodes(
                            &mut items,
                            ref_tree(entries),
                            0,
                            Section::Local,
                            None,
                            &folded,
                            &forced,
                            &mut leaf,
                        ),
                    }
                }
                Section::Tags => {
                    let entries: Vec<(String, TagLeaf)> =
                        natural_sorted_owned(std::mem::take(&mut tags))
                            .into_iter()
                            .enumerate()
                            .map(|(index, name)| (name.clone(), TagLeaf { name, index }))
                            .collect();
                    let mut leaf = |value: TagLeaf, label: &str, depth: usize| SidebarItem::Tag {
                        name: value.name,
                        label: label.to_owned(),
                        index: value.index,
                        depth,
                    };
                    match filter {
                        Some(filter) => {
                            for (path, entry) in entries {
                                if matches_ref(filter, &entry.name) {
                                    items.push(leaf(entry, &path, 0));
                                }
                            }
                        }
                        None => push_ref_nodes(
                            &mut items,
                            ref_tree(entries),
                            0,
                            Section::Tags,
                            None,
                            &folded,
                            &forced,
                            &mut leaf,
                        ),
                    }
                }
                // Stash rows keep `stash@{N}` order: the index is the identity.
                Section::Stashes => {
                    for (index, name) in std::mem::take(&mut stashes).into_iter().enumerate() {
                        items.push(SidebarItem::Stash { name, index });
                    }
                }
                // The submodule list is already sorted by path, which is the
                // order a person reads it in; the name is the row's label.
                Section::Submodules => {
                    for (index, submodule) in extras.submodules.iter().enumerate() {
                        items.push(SidebarItem::Submodule {
                            index,
                            name: submodule.name.clone(),
                            path: submodule.path.clone(),
                            detail: submodule_detail(submodule),
                            status: submodule.status,
                        });
                    }
                }
                // `git worktree list` order is the main worktree, then the
                // registered ones; that is also the order that keeps the one
                // the user is looking at on top.
                Section::Worktrees => {
                    for (index, worktree) in extras.worktrees.iter().enumerate() {
                        items.push(SidebarItem::Worktree {
                            index,
                            label: worktree_label(worktree),
                            path: worktree.path.display().to_string(),
                            detail: worktree_detail(worktree),
                            lock_reason: worktree.lock_reason.clone(),
                            is_main: worktree.is_main,
                            locked: worktree.locked,
                            prunable: worktree.prunable,
                            current: extras
                                .current_worktree
                                .is_some_and(|current| same_path(current, &worktree.path)),
                        });
                    }
                }
                // Remote branches are grouped under their remote, which is the
                // first path component of `origin/main`. A configured remote is
                // listed even before anything was fetched from it.
                Section::Remote => {
                    if remote.is_empty() && remotes.is_empty() {
                        continue;
                    }
                    let groups = remote_groups(&remote, remotes);
                    // Row ids keep the whole section's natural order even when
                    // the filter hides the groups before this one.
                    let mut indices = HashMap::new();
                    let mut next_index = 0;
                    for group in &groups {
                        for name in &group.branches {
                            indices.insert(name.clone(), next_index);
                            next_index += 1;
                        }
                    }
                    for group in groups {
                        let count = match filter {
                            Some(filter) => group
                                .branches
                                .iter()
                                .filter(|name| matches_ref(filter, name))
                                .count(),
                            None => group.branches.len(),
                        };
                        if filter.is_some() && count == 0 {
                            continue;
                        }
                        let collapsed = self
                            .remote_groups
                            .get(&group.name)
                            .copied()
                            .unwrap_or(false);
                        items.push(SidebarItem::RemoteGroup {
                            name: group.name.clone(),
                            fetch_url: group.fetch_url.clone(),
                            count,
                            collapsed,
                        });
                        if collapsed {
                            continue;
                        }
                        if group.branches.is_empty() {
                            items.push(SidebarItem::Placeholder {
                                text: t!("branch.empty.no_branches_fetched").to_string(),
                                remote: group.name,
                            });
                            continue;
                        }
                        let prefix = format!("{}/", group.name);
                        let entries: Vec<(String, RemoteLeaf)> = group
                            .branches
                            .into_iter()
                            .map(|name| {
                                let local_name = local_name_for_remote(&name).map(str::to_owned);
                                let existing = local_name.as_deref().is_some_and(|local_name| {
                                    local.iter().any(|branch| branch == local_name)
                                });
                                let path = name.strip_prefix(&prefix).unwrap_or(&name).to_owned();
                                let index = indices.get(&name).copied().unwrap_or(0);
                                (
                                    path,
                                    RemoteLeaf {
                                        name,
                                        index,
                                        local_name,
                                        existing,
                                    },
                                )
                            })
                            .collect();
                        let mut leaf = |value: RemoteLeaf, label: &str, depth: usize| {
                            SidebarItem::RemoteBranch {
                                name: value.name,
                                label: label.to_owned(),
                                index: value.index,
                                depth,
                                local_name: value.local_name,
                                existing: value.existing,
                            }
                        };
                        match filter {
                            Some(filter) => {
                                for (path, entry) in entries {
                                    if matches_ref(filter, &entry.name) {
                                        items.push(leaf(entry, &path, 0));
                                    }
                                }
                            }
                            None => push_ref_nodes(
                                &mut items,
                                ref_tree(entries),
                                0,
                                Section::Remote,
                                Some(group.name.as_str()),
                                &folded,
                                &forced,
                                &mut leaf,
                            ),
                        }
                    }
                }
            }
        }
        items
    }

    /// Opens the folder the checked-out branch lives in whenever HEAD moves,
    /// so switching branches never leaves the current one hidden in a fold.
    fn open_head_folder(&self, head: Option<&str>) {
        let mut seen = self.head_seen.borrow_mut();
        if seen.as_deref() == head {
            return;
        }
        *seen = head.map(str::to_owned);
        let Some(head) = head else {
            return;
        };
        let mut folded = self.folders.borrow_mut();
        for key in folder_ancestors(Section::Local, None, head) {
            folded.remove(&key);
        }
    }

    /// Brings the heading the rail asked for into view, then forgets it.
    pub(super) fn scroll_to_requested_section(&self, items: &[SidebarItem]) {
        let Some(section) = self.pending_scroll.borrow_mut().take() else {
            return;
        };
        if let Some(index) = items.iter().position(|item| {
            matches!(item, SidebarItem::SectionHeader { section: found, .. } if *found == section)
        }) {
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        }
    }

    /// Puts the row that was at the top of the viewport back where it was when
    /// a refresh inserts or removes entries above it. The row is remembered by
    /// its key, not its index, so adding a tag above the viewport does not
    /// scroll the list by a row.
    pub(super) fn restore_scroll(&self, items: &Rc<Vec<SidebarItem>>, design: &Design) {
        let mut anchor = self.anchor.borrow_mut();
        let Some(previous) = anchor.items.replace(items.clone()) else {
            return;
        };
        // Nothing was rebuilt, so the index and the identity agree already.
        if Rc::ptr_eq(&previous, items) || previous.is_empty() {
            return;
        }

        let base = self.scroll.0.borrow().base_handle.clone();
        let height = design.px(ITEM_HEIGHT);
        let scroll_top = -base.offset().y;
        if scroll_top <= Pixels::ZERO {
            return;
        }

        let mut index = (scroll_top / height).floor() as usize;
        let mut within = scroll_top - height * index as f32;
        while let Some(key) = previous.get(index).map(SidebarItem::key) {
            if let Some(new_index) = items.iter().position(|item| item.is(&key)) {
                if new_index != index {
                    let offset = base.offset();
                    base.set_offset(point(offset.x, -(height * new_index as f32 + within)));
                }
                return;
            }
            // The anchored row left with a section that collapsed; the next
            // surviving row down is the closest place to stay.
            index += 1;
            within = Pixels::ZERO;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use gpui::px;
    use lamina_core::HeadInfo;
    use lamina_core::repo::{SubmoduleInfo, WorktreeInfo};

    use super::*;
    use crate::branches::rows::submodule_badge;

    fn upstream(name: &str, ahead: u32, behind: u32, gone: bool) -> UpstreamInfo {
        UpstreamInfo {
            name: name.to_owned(),
            ahead: Some(ahead),
            behind: Some(behind),
            gone,
        }
    }

    fn remote(name: &str, fetch_url: Option<&str>) -> RemoteInfo {
        RemoteInfo {
            name: name.to_owned(),
            fetch_url: fetch_url.map(str::to_owned),
            push_url: None,
        }
    }

    #[test]
    fn every_configured_remote_gets_a_group_even_without_branches() {
        let remotes = [
            remote("backup", Some("/tmp/backup.git")),
            remote("origin", Some("https://example.invalid/repo.git")),
        ];
        let entries = vec!["origin/main".to_owned(), "origin/feature".to_owned()];

        let groups = remote_groups(&entries, &remotes);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "backup");
        assert!(
            groups[0].branches.is_empty(),
            "a never-fetched remote must still be listed"
        );
        assert_eq!(
            groups[0].fetch_url.as_deref(),
            Some("/tmp/backup.git"),
            "the header tooltip needs the fetch URL"
        );
        assert_eq!(groups[1].name, "origin");
        assert_eq!(
            groups[1].branches,
            ["origin/feature", "origin/main"],
            "each remote's branches read in natural order"
        );
    }

    #[test]
    fn a_remote_tracking_ref_without_a_configured_remote_keeps_its_group() {
        let entries = vec!["gone/main".to_owned()];
        let groups = remote_groups(&entries, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "gone");
        assert_eq!(groups[0].branches, ["gone/main"]);
        assert_eq!(groups[0].fetch_url, None);
    }

    #[test]
    fn no_configured_remotes_and_no_refs_means_no_groups() {
        assert!(remote_groups(&[], &[]).is_empty());
    }

    fn sidebar_data(
        local: &[&str],
        remote: &[&str],
        tags: &[&str],
        stashes: &[&str],
    ) -> SidebarData {
        let owned = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
        SidebarData {
            head_branch: Some("main".to_owned()),
            working_tree: (0, 0),
            local: owned(local),
            remote: owned(remote),
            tags: owned(tags),
            stashes: owned(stashes),
        }
    }

    fn flatten_with(
        branches: &Branches,
        data: SidebarData,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
    ) -> Vec<SidebarItem> {
        branches.flatten(
            data,
            upstreams,
            remotes,
            &SidebarExtras::default(),
            &Design::default(),
        )
    }

    fn flatten(branches: &Branches, data: SidebarData) -> Vec<SidebarItem> {
        flatten_with(branches, data, &HashMap::new(), &[])
    }

    fn flatten_extras(
        branches: &Branches,
        data: SidebarData,
        extras: &SidebarExtras<'_>,
    ) -> Vec<SidebarItem> {
        branches.flatten(data, &HashMap::new(), &[], extras, &Design::default())
    }

    #[test]
    fn sections_flatten_into_headings_and_rows() {
        let branches = Branches::default();
        let mut upstreams = HashMap::new();
        upstreams.insert("main".to_owned(), upstream("origin/main", 2, 1, false));
        upstreams.insert("topic".to_owned(), upstream("origin/topic", 0, 0, true));

        let items = flatten_with(
            &branches,
            sidebar_data(&["topic", "main"], &[], &[], &[]),
            &upstreams,
            &[],
        );

        assert_eq!(
            items,
            vec![
                SidebarItem::SectionHeader {
                    section: Section::Local,
                    count: 2,
                    open: true,
                },
                SidebarItem::LocalBranch {
                    name: "main".into(),
                    label: "main".into(),
                    index: 0,
                    depth: 0,
                    upstream: Some("↑2 ↓1".into()),
                    upstream_gone: false,
                    upstream_note: None,
                },
                SidebarItem::LocalBranch {
                    name: "topic".into(),
                    label: "topic".into(),
                    index: 1,
                    depth: 0,
                    upstream: None,
                    upstream_gone: true,
                    upstream_note: Some("The upstream branch was deleted on the remote".into()),
                },
                SidebarItem::SectionHeader {
                    section: Section::Remote,
                    count: 0,
                    open: true,
                },
                SidebarItem::SectionHeader {
                    section: Section::Tags,
                    count: 0,
                    open: true,
                },
                SidebarItem::SectionHeader {
                    section: Section::Stashes,
                    count: 0,
                    open: true,
                },
            ],
            "an empty section is its heading and a zero, with no placeholder row"
        );
    }

    #[test]
    fn a_collapsed_section_contributes_only_its_heading() {
        let mut branches = Branches::default();
        branches.toggle(Section::Tags.index());

        let items = flatten(
            &branches,
            sidebar_data(&["main"], &[], &["v1", "v2"], &["stash@{0}: wip"]),
        );

        assert!(items.contains(&SidebarItem::SectionHeader {
            section: Section::Tags,
            count: 2,
            open: false,
        }));
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, SidebarItem::Tag { .. })),
            "a collapsed section draws no rows"
        );
        assert!(
            items
                .iter()
                .any(|item| matches!(item, SidebarItem::Stash { .. })),
            "the sections after it are unaffected"
        );
    }

    #[test]
    fn hidden_sections_contribute_nothing() {
        let branches = Branches::default();
        let mut design = Design::default();
        design.show_tags = false;
        design.show_stashes = false;

        let items = branches.flatten(
            sidebar_data(&["main"], &[], &["v1"], &["stash@{0}: wip"]),
            &HashMap::new(),
            &[],
            &SidebarExtras::default(),
            &design,
        );

        assert!(
            !items.iter().any(|item| matches!(
                item,
                SidebarItem::SectionHeader {
                    section: Section::Tags | Section::Stashes,
                    ..
                } | SidebarItem::Tag { .. }
                    | SidebarItem::Stash { .. }
            )),
            "a section the settings hide has neither a heading nor rows"
        );
    }

    #[test]
    fn the_remote_section_groups_branches_under_their_remote() {
        let branches = Branches::default();
        let remotes = [
            remote("backup", Some("/tmp/backup.git")),
            remote("origin", Some("https://example.invalid/repo.git")),
        ];

        let items = flatten_with(
            &branches,
            sidebar_data(
                &["main"],
                &["origin/main", "origin/feature", "origin/HEAD"],
                &[],
                &[],
            ),
            &HashMap::new(),
            &remotes,
        );

        let remote_items: Vec<&SidebarItem> = items
            .iter()
            .skip_while(|item| {
                !matches!(
                    item,
                    SidebarItem::SectionHeader {
                        section: Section::Remote,
                        ..
                    }
                )
            })
            .take_while(|item| {
                !matches!(
                    item,
                    SidebarItem::SectionHeader {
                        section: Section::Tags,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(
            remote_items,
            vec![
                &SidebarItem::SectionHeader {
                    section: Section::Remote,
                    count: 3,
                    open: true,
                },
                &SidebarItem::RemoteGroup {
                    name: "backup".into(),
                    fetch_url: Some("/tmp/backup.git".into()),
                    count: 0,
                    collapsed: false,
                },
                &SidebarItem::Placeholder {
                    text: "no branches fetched yet".to_string(),
                    remote: "backup".into(),
                },
                &SidebarItem::RemoteGroup {
                    name: "origin".into(),
                    fetch_url: Some("https://example.invalid/repo.git".into()),
                    count: 3,
                    collapsed: false,
                },
                &SidebarItem::RemoteBranch {
                    name: "origin/feature".into(),
                    label: "feature".into(),
                    index: 0,
                    depth: 0,
                    local_name: Some("feature".into()),
                    existing: false,
                },
                &SidebarItem::RemoteBranch {
                    name: "origin/HEAD".into(),
                    label: "HEAD".into(),
                    index: 1,
                    depth: 0,
                    local_name: None,
                    existing: false,
                },
                &SidebarItem::RemoteBranch {
                    name: "origin/main".into(),
                    label: "main".into(),
                    index: 2,
                    depth: 0,
                    local_name: Some("main".into()),
                    existing: true,
                },
            ]
        );
    }

    #[test]
    fn a_collapsed_remote_group_contributes_only_its_heading() {
        let mut branches = Branches::default();
        branches.toggle_remote_group("origin".to_owned());

        let items = flatten_with(
            &branches,
            sidebar_data(&[], &["origin/main"], &[], &[]),
            &HashMap::new(),
            &[remote("origin", None)],
        );

        assert!(items.contains(&SidebarItem::RemoteGroup {
            name: "origin".into(),
            fetch_url: None,
            count: 1,
            collapsed: true,
        }));
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, SidebarItem::RemoteBranch { .. })),
            "a folded remote draws no branches"
        );
        assert!(
            !items
                .iter()
                .any(|item| item.is(&ItemKey::RemoteEmpty("origin".to_owned()))),
            "a folded remote draws no placeholder either"
        );
    }

    fn tree(names: &[&str]) -> Vec<RefNode<()>> {
        ref_tree(names.iter().map(|name| ((*name).to_owned(), ())).collect())
    }

    /// The tree as each row reads: a folder prints its segment and path, a
    /// leaf its full label and path.
    fn shape<T>(nodes: &[RefNode<T>]) -> Vec<String> {
        nodes
            .iter()
            .map(|node| match node {
                RefNode::Leaf { label, path, .. } => format!("leaf:{label}@{path}"),
                RefNode::Folder {
                    path,
                    segment,
                    children,
                } => format!("folder:{segment}@{path}[{}]", shape(children).join(",")),
            })
            .collect()
    }

    #[test]
    fn refs_fold_along_their_shared_path_segments() {
        // Each row names the scenario, the refs it folds, and the shape the
        // tree must have: folders in brackets, leaves by their full label.
        let cases: &[(&str, &[&str], &[&str])] = &[
            (
                "refs sharing a path segment nest under a folder",
                &["t3/fast-blame", "t3/blame-view", "main"],
                &[
                    "leaf:main@main",
                    "folder:t3@t3[leaf:blame-view@t3/blame-view,leaf:fast-blame@t3/fast-blame]",
                ],
            ),
            (
                "deeper segments nest inside each other",
                &[
                    "feature/auth/login",
                    "feature/auth/logout",
                    "feature/ui",
                    "main",
                ],
                &[
                    "folder:feature@feature[folder:auth@feature/auth[leaf:login@feature/auth/login,leaf:logout@feature/auth/logout],leaf:ui@feature/ui]",
                    "leaf:main@main",
                ],
            ),
            (
                "a folder holding one branch shows the whole name in its row",
                &["t3/blame-view"],
                &["leaf:t3/blame-view@t3/blame-view"],
            ),
            (
                "a chain of one-branch folders collapses to one row",
                &["a/b/c"],
                &["leaf:a/b/c@a/b/c"],
            ),
            (
                "a folder appears once a sibling gives it something to group",
                &["a/b/c", "a/b/d"],
                &["folder:a@a[folder:b@a/b[leaf:c@a/b/c,leaf:d@a/b/d]]"],
            ),
            (
                "folders and leaves keep natural order",
                &[
                    "topic-10",
                    "topic-2",
                    "feature-10/x",
                    "feature-2/x",
                    "zebra/x",
                ],
                &[
                    "leaf:feature-2/x@feature-2/x",
                    "leaf:feature-10/x@feature-10/x",
                    "leaf:topic-2@topic-2",
                    "leaf:topic-10@topic-10",
                    "leaf:zebra/x@zebra/x",
                ],
            ),
        ];

        for (case, names, expected) in cases {
            assert_eq!(
                shape(&tree(names)),
                *expected,
                "the ref tree changed for case: {case}"
            );
        }
    }

    #[test]
    fn flatten_draws_folder_rows_above_their_leaves() {
        let items = flatten(
            &Branches::default(),
            sidebar_data(&["t3/fast-blame", "main", "t3/blame-view"], &[], &[], &[]),
        );

        assert_eq!(
            items,
            vec![
                SidebarItem::SectionHeader {
                    section: Section::Local,
                    count: 3,
                    open: true,
                },
                SidebarItem::LocalBranch {
                    name: "main".into(),
                    label: "main".into(),
                    index: 0,
                    depth: 0,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                SidebarItem::Folder {
                    key: "local:t3".into(),
                    segment: "t3".into(),
                    count: 2,
                    open: true,
                    depth: 0,
                    section: Section::Local,
                },
                SidebarItem::LocalBranch {
                    name: "t3/blame-view".into(),
                    label: "blame-view".into(),
                    index: 1,
                    depth: 1,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                SidebarItem::LocalBranch {
                    name: "t3/fast-blame".into(),
                    label: "fast-blame".into(),
                    index: 2,
                    depth: 1,
                    upstream: None,
                    upstream_gone: false,
                    upstream_note: None,
                },
                SidebarItem::SectionHeader {
                    section: Section::Remote,
                    count: 0,
                    open: true,
                },
                SidebarItem::SectionHeader {
                    section: Section::Tags,
                    count: 0,
                    open: true,
                },
                SidebarItem::SectionHeader {
                    section: Section::Stashes,
                    count: 0,
                    open: true,
                },
            ]
        );
    }

    #[test]
    fn remote_branches_fold_under_their_remote() {
        let items = flatten_with(
            &Branches::default(),
            sidebar_data(
                &[],
                &["origin/t3/a", "origin/t3/b", "origin/main"],
                &[],
                &[],
            ),
            &HashMap::new(),
            &[remote("origin", None)],
        );

        assert!(items.contains(&SidebarItem::Folder {
            key: "remote:origin:t3".into(),
            segment: "t3".into(),
            count: 2,
            open: true,
            depth: 0,
            section: Section::Remote,
        }));
        let labels: Vec<String> = items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::RemoteBranch { label, depth, .. } => Some(format!("{label}/{depth}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            ["main/0", "a/1", "b/1"],
            "the folder groups origin's t3 refs, and only those"
        );
    }

    #[test]
    fn a_folder_folds_like_a_section_and_stays_folded() {
        let mut branches = Branches::default();
        assert!(!branches.folder_folded("local:t3"));
        branches.toggle_folder("local:t3".into());

        let folded = |items: &[SidebarItem]| {
            items.iter().any(|item| {
                matches!(item, SidebarItem::Folder { key, open: false, .. } if key == "local:t3")
            })
        };
        let items = flatten(&branches, sidebar_data(&["t3/a", "t3/b"], &[], &[], &[]));
        assert!(folded(&items), "a folded folder starts closed");
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, SidebarItem::LocalBranch { .. })),
            "a folded folder draws no leaves"
        );

        let refreshed = flatten(
            &branches,
            sidebar_data(&["t3/a", "t3/b", "t3/c"], &[], &[], &[]),
        );
        assert!(
            folded(&refreshed),
            "the fold survives the snapshot that adds a ref"
        );

        branches.toggle_folder("local:t3".into());
        let open = flatten(
            &branches,
            sidebar_data(&["t3/a", "t3/b", "t3/c"], &[], &[], &[]),
        );
        assert!(open.contains(&SidebarItem::Folder {
            key: "local:t3".into(),
            segment: "t3".into(),
            count: 3,
            open: true,
            depth: 0,
            section: Section::Local,
        }));
        assert_eq!(
            open.iter()
                .filter(|item| matches!(item, SidebarItem::LocalBranch { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn the_current_branch_folder_opens_when_head_moves_into_it() {
        let mut branches = Branches::default();
        branches.toggle_folder("local:t3".into());
        let main = sidebar_data(&["main", "t3/a", "t3/b"], &[], &[], &[]);
        let items = flatten(&branches, main);
        assert!(
            items.iter().any(|item| matches!(
                item,
                SidebarItem::Folder { key, open: false, .. } if key == "local:t3"
            )),
            "the folder folds while HEAD is elsewhere"
        );

        let mut on_t3 = sidebar_data(&["main", "t3/a", "t3/b"], &[], &[], &[]);
        on_t3.head_branch = Some("t3/a".into());
        let items = flatten(&branches, on_t3);
        assert!(
            items.iter().any(|item| matches!(
                item,
                SidebarItem::Folder { key, open: true, .. } if key == "local:t3"
            )),
            "the checked-out branch's folder opens itself"
        );
        assert!(
            items.iter().any(
                |item| matches!(item, SidebarItem::LocalBranch { name, .. } if name == "t3/a")
            )
        );
    }

    #[test]
    fn a_filter_lists_matching_leaves_by_their_full_path_without_folders() {
        let mut branches = Branches::default();
        branches.set_filter(Some("t3/blame".into()));

        let items = flatten(
            &branches,
            sidebar_data(&["t3/blame-view", "t3/fast-blame", "main"], &[], &[], &[]),
        );

        assert!(
            !items
                .iter()
                .any(|item| matches!(item, SidebarItem::Folder { .. })),
            "a filter shows the leaves directly"
        );
        let rows: Vec<(String, String, usize)> = items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::LocalBranch {
                    name, label, depth, ..
                } => Some((name.clone(), label.clone(), *depth)),
                _ => None,
            })
            .collect();
        assert_eq!(
            rows,
            [("t3/blame-view".to_owned(), "t3/blame-view".to_owned(), 0)],
            "the match keeps its whole path and no indent"
        );
    }

    #[test]
    fn a_filter_hides_remotes_without_a_match() {
        let mut branches = Branches::default();
        branches.set_filter(Some("origin/t3/a".into()));

        let items = flatten_with(
            &branches,
            sidebar_data(
                &[],
                &["origin/t3/a", "origin/t3/b", "backup/main"],
                &[],
                &[],
            ),
            &HashMap::new(),
            &[],
        );

        assert!(!items.iter().any(|item| matches!(
            item,
            SidebarItem::RemoteGroup { name, .. } if name == "backup"
        )));
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::RemoteBranch { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["t3/a"]);
    }

    #[test]
    fn item_keys_are_unique_and_survive_a_refresh() {
        let branches = Branches::default();
        let before = flatten(&branches, sidebar_data(&["main"], &[], &["v1", "v2"], &[]));
        let after = flatten(
            &branches,
            sidebar_data(&["main"], &[], &["v0", "v1", "v2"], &[]),
        );

        let keys: std::collections::HashSet<ItemKey> =
            before.iter().map(SidebarItem::key).collect();
        assert_eq!(keys.len(), before.len(), "every row has a key of its own");

        let key = ItemKey::Tag("v2".to_owned());
        let old_index = before.iter().position(|item| item.is(&key)).unwrap();
        let new_index = after.iter().position(|item| item.is(&key)).unwrap();
        assert_eq!(
            new_index,
            old_index + 1,
            "a tag added above moves the row, not the key"
        );
    }

    #[test]
    fn a_refresh_keeps_the_row_at_the_top_of_the_viewport() {
        let branches = Branches::default();
        let design = Design::default();
        let tags: Vec<String> = (1..=10).map(|number| format!("v{number}")).collect();
        let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let before = Rc::new(flatten(&branches, sidebar_data(&[], &[], &tag_refs, &[])));
        branches.restore_scroll(&before, &design);

        // Five rows down: past the two empty sections' headings and
        // placeholders, so `v1` is the first tag at the top.
        branches
            .scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.0), px(-140.0)));

        let mut inserted = vec!["v0".to_owned()];
        inserted.extend(tags);
        let inserted_refs: Vec<&str> = inserted.iter().map(String::as_str).collect();
        let after = Rc::new(flatten(
            &branches,
            sidebar_data(&[], &[], &inserted_refs, &[]),
        ));
        branches.restore_scroll(&after, &design);

        assert_eq!(
            branches.scroll.0.borrow().base_handle.offset().y,
            px(-168.0),
            "the same tag must stay at the top after one is added above it"
        );
    }

    #[test]
    fn a_refresh_that_keeps_the_list_keeps_the_scroll_offset() {
        let branches = Branches::default();
        let design = Design::default();
        let items = Rc::new(flatten(&branches, sidebar_data(&["main"], &[], &[], &[])));
        branches.restore_scroll(&items, &design);
        branches
            .scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.0), px(-56.0)));

        branches.restore_scroll(&items, &design);

        assert_eq!(
            branches.scroll.0.borrow().base_handle.offset().y,
            px(-56.0),
            "an unchanged list must not move under the pointer"
        );
    }

    fn oid(byte: u8) -> lamina_core::Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn submodule(name: &str, status: SubmoduleStatus) -> SubmoduleInfo {
        SubmoduleInfo {
            name: name.to_owned(),
            path: name.to_owned(),
            url: Some(format!("../{name}.git")),
            recorded_commit: Some(oid(1)),
            checked_out_commit: match status {
                SubmoduleStatus::Uninitialised => None,
                SubmoduleStatus::CommitDiffers => Some(oid(2)),
                SubmoduleStatus::UpToDate | SubmoduleStatus::Modified => Some(oid(1)),
            },
            status,
        }
    }

    fn worktree(
        path: &str,
        head: Option<HeadInfo>,
        is_main: bool,
        locked: bool,
        prunable: bool,
    ) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from(path),
            head,
            is_main,
            locked,
            lock_reason: locked.then(|| "testing".to_owned()),
            prunable,
        }
    }

    fn branch_head(name: &str) -> HeadInfo {
        HeadInfo::Branch {
            short_name: name.to_owned(),
            target: Some(oid(3)),
        }
    }

    #[test]
    fn a_repository_without_submodules_or_linked_worktrees_hides_both_sections() {
        let branches = Branches::default();
        let extras = SidebarExtras {
            revision: 4,
            ..SidebarExtras::default()
        };

        let items = flatten_extras(&branches, sidebar_data(&["main"], &[], &[], &[]), &extras);

        assert!(
            !items.iter().any(|item| matches!(
                item,
                SidebarItem::SectionHeader {
                    section: Section::Submodules | Section::Worktrees,
                    ..
                } | SidebarItem::Submodule { .. }
                    | SidebarItem::Worktree { .. }
            )),
            "an empty repository has no submodule or worktree heading to fold"
        );
    }

    #[test]
    fn the_main_worktree_alone_does_not_earn_a_section() {
        let branches = Branches::default();
        let main = [worktree(
            "/src/lamina",
            Some(branch_head("main")),
            true,
            false,
            false,
        )];
        let main_only = SidebarExtras {
            worktrees: &main,
            current_worktree: Some(Path::new("/src/lamina")),
            revision: 1,
            ..SidebarExtras::default()
        };
        let items = flatten_extras(
            &branches,
            sidebar_data(&["main"], &[], &[], &[]),
            &main_only,
        );
        assert!(!items.iter().any(|item| matches!(
            item,
            SidebarItem::SectionHeader {
                section: Section::Worktrees,
                ..
            } | SidebarItem::Worktree { .. }
        )));

        let both = [
            worktree("/src/lamina", Some(branch_head("main")), true, false, false),
            worktree(
                "/src/lamina-docs",
                Some(branch_head("docs")),
                false,
                false,
                false,
            ),
        ];
        let linked = SidebarExtras {
            worktrees: &both,
            current_worktree: Some(Path::new("/src/lamina")),
            ..main_only
        };
        let items = flatten_extras(&branches, sidebar_data(&["main"], &[], &[], &[]), &linked);
        assert!(items.contains(&SidebarItem::SectionHeader {
            section: Section::Worktrees,
            count: 2,
            open: true,
        }));
        let current = items
            .iter()
            .find_map(|item| match item {
                SidebarItem::Worktree { current, path, .. } if path == "/src/lamina" => {
                    Some(*current)
                }
                _ => None,
            })
            .expect("the main worktree row");
        assert!(current, "the window's own checkout is marked like HEAD");
    }

    #[test]
    fn submodule_rows_carry_the_state_their_badge_names() {
        let branches = Branches::default();
        let submodules = [
            submodule("libs/parser", SubmoduleStatus::UpToDate),
            submodule("libs/legacy", SubmoduleStatus::Uninitialised),
            submodule("libs/ui", SubmoduleStatus::CommitDiffers),
            submodule("libs/dirty", SubmoduleStatus::Modified),
        ];
        let extras = SidebarExtras {
            submodules: &submodules,
            revision: 1,
            ..SidebarExtras::default()
        };

        let items = flatten_extras(&branches, sidebar_data(&["main"], &[], &[], &[]), &extras);

        assert!(items.contains(&SidebarItem::SectionHeader {
            section: Section::Submodules,
            count: 4,
            open: true,
        }));
        let statuses: Vec<SubmoduleStatus> = items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::Submodule { status, .. } => Some(*status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            [
                SubmoduleStatus::UpToDate,
                SubmoduleStatus::Uninitialised,
                SubmoduleStatus::CommitDiffers,
                SubmoduleStatus::Modified,
            ]
        );
        assert_eq!(
            submodule_badge(SubmoduleStatus::UpToDate),
            None,
            "an up-to-date checkout needs no label"
        );
        assert_eq!(
            submodule_badge(SubmoduleStatus::Uninitialised)
                .map(|(label, _)| label)
                .as_deref(),
            Some("not initialised")
        );
        assert_eq!(
            submodule_badge(SubmoduleStatus::CommitDiffers)
                .map(|(label, _)| label)
                .as_deref(),
            Some("changed")
        );
        assert_eq!(
            submodule_badge(SubmoduleStatus::Modified)
                .map(|(label, _)| label)
                .as_deref(),
            Some("modified")
        );
    }

    #[test]
    fn a_submodule_tooltip_names_its_path_url_and_both_commits() {
        let detail = submodule_detail(&submodule("libs/ui", SubmoduleStatus::CommitDiffers));
        assert!(detail.starts_with("libs/ui\n../libs/ui.git"), "{detail}");
        assert!(detail.contains("recorded 0101010"), "{detail}");
        assert!(detail.contains("checked out 0202020"), "{detail}");

        let uninitialised =
            submodule_detail(&submodule("libs/legacy", SubmoduleStatus::Uninitialised));
        assert!(
            uninitialised.contains("checked out none"),
            "{uninitialised}"
        );
    }

    #[test]
    fn worktree_rows_label_their_branch_or_detached_commit_and_badge_their_state() {
        let branches = Branches::default();
        let worktrees = [
            worktree("/src/lamina", Some(branch_head("main")), true, false, false),
            worktree(
                "/src/lamina-docs",
                Some(branch_head("docs")),
                false,
                true,
                false,
            ),
            worktree(
                "/src/lamina-gone",
                Some(HeadInfo::Detached { target: oid(0xab) }),
                false,
                false,
                true,
            ),
        ];
        let extras = SidebarExtras {
            worktrees: &worktrees,
            revision: 1,
            ..SidebarExtras::default()
        };

        let items = flatten_extras(&branches, sidebar_data(&["main"], &[], &[], &[]), &extras);

        let rows: Vec<&SidebarItem> = items
            .iter()
            .filter(|item| matches!(item, SidebarItem::Worktree { .. }))
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            worktree_label(&worktrees[0]),
            "main",
            "a branch checkout is named by its branch"
        );
        assert_eq!(
            worktree_label(&worktrees[2]),
            "detached abababa · lamina-gone",
            "a detached checkout still has to be distinguishable by its directory"
        );
        let labels: Vec<String> = rows
            .iter()
            .map(|item| match item {
                SidebarItem::Worktree {
                    is_main,
                    locked,
                    prunable,
                    ..
                } => format!("{is_main}/{locked}/{prunable}"),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            labels,
            ["true/false/false", "false/true/false", "false/false/true"]
        );
        assert_eq!(
            worktree_detail(&worktrees[1]),
            "/src/lamina-docs\nlocked: testing"
        );
        assert!(worktree_detail(&worktrees[2]).contains("missing"));
    }

    #[test]
    fn submodule_and_worktree_keys_survive_a_refresh() {
        let branches = Branches::default();
        let first = [
            submodule("a", SubmoduleStatus::UpToDate),
            submodule("b", SubmoduleStatus::UpToDate),
        ];
        let second = [
            submodule("0", SubmoduleStatus::UpToDate),
            submodule("a", SubmoduleStatus::UpToDate),
            submodule("b", SubmoduleStatus::UpToDate),
        ];
        let worktrees = [
            worktree("/src/lamina", Some(branch_head("main")), true, false, false),
            worktree(
                "/src/lamina-docs",
                Some(branch_head("docs")),
                false,
                false,
                false,
            ),
        ];
        let before_extras = SidebarExtras {
            submodules: &first,
            worktrees: &worktrees,
            revision: 1,
            ..SidebarExtras::default()
        };
        let after_extras = SidebarExtras {
            submodules: &second,
            worktrees: &worktrees,
            revision: 2,
            ..SidebarExtras::default()
        };
        let data = || sidebar_data(&["main"], &[], &[], &[]);
        let before = flatten_extras(&branches, data(), &before_extras);
        let after = flatten_extras(&branches, data(), &after_extras);

        let key = ItemKey::Submodule("b".to_owned());
        let old_index = before.iter().position(|item| item.is(&key)).unwrap();
        let new_index = after.iter().position(|item| item.is(&key)).unwrap();
        assert_eq!(
            new_index,
            old_index + 1,
            "a submodule added above moves the row, not the key"
        );

        let keys: std::collections::HashSet<ItemKey> = after.iter().map(SidebarItem::key).collect();
        assert_eq!(keys.len(), after.len(), "every row has a key of its own");
    }

    #[test]
    fn the_item_cache_follows_the_submodule_and_worktree_answer() {
        let branches = Branches::default();
        let submodules = [submodule("libs/ui", SubmoduleStatus::UpToDate)];
        let worktrees = [
            worktree("/src/lamina", Some(branch_head("main")), true, false, false),
            worktree(
                "/src/lamina-docs",
                Some(branch_head("docs")),
                false,
                false,
                false,
            ),
        ];
        let upstreams = HashMap::new();
        let design = Design::default();

        let first = SidebarExtras {
            submodules: &submodules,
            worktrees: &worktrees,
            revision: 1,
            ..SidebarExtras::default()
        };
        let cached = branches.items(
            sidebar_data(&["main"], &[], &[], &[]),
            3,
            &upstreams,
            &[],
            &first,
            &design,
        );
        let again = branches.items(
            sidebar_data(&["main"], &[], &[], &[]),
            3,
            &upstreams,
            &[],
            &first,
            &design,
        );
        assert!(
            Rc::ptr_eq(&cached, &again),
            "an unchanged snapshot and answer reuse the flattened list"
        );

        let mut updated = SidebarExtras { ..first };
        updated.revision = 2;
        let rebuilt = branches.items(
            sidebar_data(&["main"], &[], &[], &[]),
            3,
            &upstreams,
            &[],
            &updated,
            &design,
        );
        assert!(
            !Rc::ptr_eq(&cached, &rebuilt),
            "a new submodule/worktree answer rebuilds the sections"
        );
    }
}
