//! Branch management: the sidebar's ref sections, their context menus, and the
//! create/delete dialogs.
//!
//! Nothing here runs git. Every write is an [`Operation`] handed to the
//! window's [`RepoView`], which gives it to the session worker; the sections
//! only read the snapshot the view has already published. The helpers that do
//! not need a window (name validation, upstream labels, remote-to-local name
//! mapping) are unit tested without a repository.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Pixels, Size, Stateful,
    StatefulInteractiveElement as _, Styled, UniformListScrollHandle, Window, actions, anchored,
    deferred, div, hsla, point, px, uniform_list,
};
use gpui_component::{
    Icon,
    checkbox::Checkbox,
    input::{Input, InputState},
    menu::{ContextMenuExt as _, PopupMenuItem},
};
use strata_core::session::{HistoryScope, Operation, OperationId};
use strata_core::{RefKind, RemoteInfo, UpstreamInfo};

use crate::actions::REPO_VIEW;
use crate::data::SidebarData;
use crate::repo_view::RepoView;
use crate::scrollbar::WithScrollbar as _;
use crate::style::{self, Design};

/// Key context the dialog sets while it is open, so `Enter` and `Escape` mean
/// confirm and cancel only there.
const BRANCH_DIALOG: &str = "BranchDialog";

/// Height of one line of the sidebar list. `uniform_list` lays every item out
/// at the height it measured once, so headers, rows and placeholders must all
/// be this tall; it matches [`crate::ui::list_row`]'s row height.
const ITEM_HEIGHT: f32 = 28.0;

actions!(
    strata,
    [
        CreateBranchFromHead,
        ConfirmBranchDialog,
        CancelBranchDialog,
    ]
);

// `KeyBinding::new` parses without an OS, so `secondary-` is rejected there.
#[cfg(target_os = "macos")]
const CREATE_BRANCH_KEY: &str = "cmd-shift-b";
#[cfg(not(target_os = "macos"))]
const CREATE_BRANCH_KEY: &str = "ctrl-shift-b";

/// Binds the branch commands. Their handlers live on `RepoView`'s root
/// element, which is on the focus path whenever the history view has focus.
pub fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(CREATE_BRANCH_KEY, CreateBranchFromHead, Some(REPO_VIEW)),
        KeyBinding::new("enter", ConfirmBranchDialog, Some(BRANCH_DIALOG)),
        KeyBinding::new("escape", CancelBranchDialog, Some(BRANCH_DIALOG)),
    ]);
}

/// The colours the sections and dialogs paint with, taken from the window
/// chrome's palette so the sidebar reads as one surface.
#[derive(Clone)]
pub(crate) struct Colors {
    pub(crate) design: Design,
    pub(crate) sidebar_foreground: Hsla,
    pub(crate) muted_foreground: Hsla,
    pub(crate) border: Hsla,
    pub(crate) foreground: Hsla,
    pub(crate) danger: Hsla,
    pub(crate) overlay: Hsla,
}

/// One collapsible section of the sidebar.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Section {
    Local,
    Remote,
    Tags,
    Stashes,
}

impl Section {
    const ALL: [Section; 4] = [
        Section::Local,
        Section::Remote,
        Section::Tags,
        Section::Stashes,
    ];

    fn index(self) -> usize {
        match self {
            Section::Local => 0,
            Section::Remote => 1,
            Section::Tags => 2,
            Section::Stashes => 3,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Section::Local => "BRANCHES",
            Section::Remote => "REMOTES",
            Section::Tags => "TAGS",
            Section::Stashes => "STASHES",
        }
    }

    fn icon(self) -> &'static [u8] {
        match self {
            Section::Local => style::glyph::BRANCH,
            Section::Remote => style::glyph::CLOUD,
            Section::Tags => style::glyph::TAG,
            Section::Stashes => style::glyph::ARCHIVE,
        }
    }

    fn empty_label(self) -> &'static str {
        match self {
            Section::Local => "no local branches",
            Section::Remote => "no remotes",
            Section::Tags => "no tags",
            Section::Stashes => "no stashes",
        }
    }

    /// Whether `history` settings hide this section.
    fn hidden(self, design: &Design) -> bool {
        match self {
            Section::Local => false,
            Section::Remote => !design.show_remote_branches,
            Section::Tags => !design.show_tags,
            Section::Stashes => !design.show_stashes,
        }
    }
}

/// Where a new branch should start. The label is user-facing, e.g. `from main`
/// or `from commit 4f2a1c9`.
pub(crate) struct BranchStart {
    pub(crate) point: Option<String>,
    pub(crate) label: String,
}

/// The sidebar entry whose tip the history last jumped to.
///
/// It stays highlighted until another ref is revealed or a commit row is
/// selected; the highlight says where the view came from, not what is checked
/// out, which is why it is kept apart from HEAD.
#[derive(Clone, Default)]
pub(crate) struct RevealState {
    revealed: Option<RevealedRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RevealedRef {
    Ref(RefKind, String),
    Stash(usize),
}

impl RevealState {
    fn reveal_ref(&mut self, kind: RefKind, short_name: &str) {
        self.revealed = Some(RevealedRef::Ref(kind, short_name.to_owned()));
    }

    fn reveal_stash(&mut self, index: usize) {
        self.revealed = Some(RevealedRef::Stash(index));
    }

    fn clear(&mut self) {
        self.revealed = None;
    }

    fn is_ref(&self, kind: RefKind, short_name: &str) -> bool {
        matches!(
            &self.revealed,
            Some(RevealedRef::Ref(revealed_kind, name))
                if *revealed_kind == kind && name == short_name
        )
    }

    fn is_stash(&self, index: usize) -> bool {
        matches!(self.revealed, Some(RevealedRef::Stash(revealed)) if revealed == index)
    }
}

/// One line of the sidebar's virtualised list.
///
/// The list owns its strings, so drawing a frame holds nothing borrowed from
/// the snapshot; the parts that change without the refs (the revealed row,
/// HEAD, busy) stay out and are read while the visible rows are built.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarItem {
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
    LocalBranch {
        name: String,
        /// Position in the section's natural order, for the element id.
        index: usize,
        /// `↑2 ↓1`, `gone`, or nothing when level with the upstream.
        upstream: Option<String>,
    },
    RemoteBranch {
        name: String,
        index: usize,
        /// The local branch `origin/foo` stands for; `None` for `origin/HEAD`.
        local_name: Option<String>,
        /// Whether that local branch exists already.
        existing: bool,
    },
    Tag {
        name: String,
        index: usize,
    },
    Stash {
        /// `stash@{N}: message`, as the snapshot lists it.
        name: String,
        index: usize,
    },
    /// "no tags", "no branches fetched yet", ...
    Placeholder {
        text: &'static str,
        place: PlaceholderPlace,
    },
}

/// Where a placeholder belongs, which decides its indent and its key.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PlaceholderPlace {
    Section(Section),
    Remote(String),
}

impl PlaceholderPlace {
    fn indent(&self) -> Pixels {
        match self {
            PlaceholderPlace::Section(_) => px(38.0),
            PlaceholderPlace::Remote(_) => px(42.0),
        }
    }
}

/// Identity of a list position that survives a snapshot refresh, so the row
/// under the viewport can be found again by what it is rather than by index.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ItemKey {
    Section(Section),
    RemoteGroup(String),
    LocalBranch(String),
    RemoteBranch(String),
    Tag(String),
    Stash(usize),
    SectionEmpty(Section),
    RemoteEmpty(String),
}

impl SidebarItem {
    fn key(&self) -> ItemKey {
        match self {
            SidebarItem::SectionHeader { section, .. } => ItemKey::Section(*section),
            SidebarItem::RemoteGroup { name, .. } => ItemKey::RemoteGroup(name.clone()),
            SidebarItem::LocalBranch { name, .. } => ItemKey::LocalBranch(name.clone()),
            SidebarItem::RemoteBranch { name, .. } => ItemKey::RemoteBranch(name.clone()),
            SidebarItem::Tag { name, .. } => ItemKey::Tag(name.clone()),
            SidebarItem::Stash { index, .. } => ItemKey::Stash(*index),
            SidebarItem::Placeholder { place, .. } => match place {
                PlaceholderPlace::Section(section) => ItemKey::SectionEmpty(*section),
                PlaceholderPlace::Remote(name) => ItemKey::RemoteEmpty(name.clone()),
            },
        }
    }

    /// Whether this item is the row `key` names. Compared without building a
    /// key, so finding the scroll anchor does not allocate per entry.
    fn is(&self, key: &ItemKey) -> bool {
        match (self, key) {
            (SidebarItem::SectionHeader { section, .. }, ItemKey::Section(key)) => section == key,
            (SidebarItem::RemoteGroup { name, .. }, ItemKey::RemoteGroup(key)) => name == key,
            (SidebarItem::LocalBranch { name, .. }, ItemKey::LocalBranch(key)) => name == key,
            (SidebarItem::RemoteBranch { name, .. }, ItemKey::RemoteBranch(key)) => name == key,
            (SidebarItem::Tag { name, .. }, ItemKey::Tag(key)) => name == key,
            (SidebarItem::Stash { index, .. }, ItemKey::Stash(key)) => index == key,
            (SidebarItem::Placeholder { place, .. }, ItemKey::SectionEmpty(key)) => {
                matches!(place, PlaceholderPlace::Section(section) if section == key)
            }
            (SidebarItem::Placeholder { place, .. }, ItemKey::RemoteEmpty(key)) => {
                matches!(place, PlaceholderPlace::Remote(name) if name == key)
            }
            _ => false,
        }
    }
}

/// The flattened list, rebuilt only when its inputs change, so a frame costs
/// the rows on screen rather than the size of the repository.
#[derive(Default)]
struct ItemCache {
    key: Option<ItemsKey>,
    items: Rc<Vec<SidebarItem>>,
}

#[derive(PartialEq, Eq)]
struct ItemsKey {
    /// Snapshot generation; every publish bumps it.
    revision: u64,
    /// Bumped when a section or a remote group folds.
    local: u64,
    show_remote_branches: bool,
    show_tags: bool,
    show_stashes: bool,
}

/// The list drawn last frame, so a refresh can put the same row back under
/// the viewport instead of the same index.
#[derive(Default)]
struct ScrollAnchor {
    items: Option<Rc<Vec<SidebarItem>>>,
}

/// Everything the branch UI owns between frames.
#[derive(Default)]
pub(crate) struct Branches {
    collapsed: [bool; 4],
    /// Remote name -> collapsed, so each remote's branch list folds on its own.
    remote_groups: HashMap<String, bool>,
    /// The ref revealed last, drawn with the neutral tint until a commit is
    /// selected.
    reveal: RevealState,
    /// The ref history is limited to, as its short name, while a "show only"
    /// scope is active. The scope itself lives in the session.
    scope: Option<String>,
    dialog: Option<Dialog>,
    /// Operations this module started, keyed by the id the session returned.
    /// The label is what the status bar prefixes a failure with.
    running: HashMap<OperationId, Running>,
    /// Last viewport size seen, so a dialog opened from an operation result can
    /// still paint a window-sized backdrop.
    viewport: Size<Pixels>,
    /// Focus target for dialog cards. Created once by the view because a
    /// `FocusHandle` needs an `App`, and reused: only one dialog is open and it
    /// is the only element tracking the handle while it lives.
    dialog_focus: Option<FocusHandle>,
    /// A branch git refused to delete because it is not fully merged. The view
    /// offers the forced delete through [`crate::ui::confirm`], which needs a
    /// window the session event does not have.
    force_delete: Option<String>,
    /// The section list's scroll position, shared by the list and its scrollbar.
    scroll: UniformListScrollHandle,
    cache: RefCell<ItemCache>,
    /// Bumped by every fold, so the cached list cannot outlive a toggle.
    local_revision: u64,
    anchor: RefCell<ScrollAnchor>,
}

struct Running {
    label: String,
    /// Set for an unforced delete, which can be offered again with `force`.
    delete: Option<String>,
}

struct Dialog {
    kind: DialogKind,
    viewport: Size<Pixels>,
    /// Focused while the dialog is open, so its `Enter` and `Escape` bindings
    /// apply and the keys no longer reach the commit list.
    focus: Option<FocusHandle>,
}

enum DialogKind {
    Create(CreateForm),
}

struct CreateForm {
    input: Entity<InputState>,
    start: BranchStart,
    checkout: bool,
    error: Option<String>,
}

/// The validated contents of the create dialog.
pub(crate) struct CreateRequest {
    pub(crate) name: String,
    pub(crate) start_point: Option<String>,
    pub(crate) checkout: bool,
}

impl Branches {
    /// Renders the virtualised ref sections and, when one is open, the dialog
    /// on top of the window.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &self,
        data: SidebarData,
        revision: u64,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
        enabled: bool,
        busy: bool,
        colors: &Colors,
        entity: &Entity<RepoView>,
    ) -> AnyElement {
        let head = data.head_branch.clone();
        let items = self.items(data, revision, upstreams, remotes, &colors.design);
        self.restore_scroll(&items);

        let reveal = self.reveal.clone();
        let list_colors = colors.clone();
        let list_entity = entity.clone();
        let list = uniform_list("sidebar-refs", items.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    item_element(
                        &items[index],
                        head.as_deref(),
                        &reveal,
                        enabled,
                        busy,
                        &list_colors,
                        list_entity.clone(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        .with_scrollbar(&self.scroll)
        .flex_1()
        .min_h_0();

        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(list);
        if let Some(dialog) = self.dialog_overlay(colors, entity) {
            column = column.child(dialog);
        }
        column.into_any_element()
    }

    /// The flattened list, rebuilt only when the snapshot or a fold changes.
    ///
    /// `revision` is the snapshot's generation, which every publish bumps; that
    /// is what lets the cached strings outlive the frame they were copied in.
    fn items(
        &self,
        data: SidebarData,
        revision: u64,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
        design: &Design,
    ) -> Rc<Vec<SidebarItem>> {
        let key = ItemsKey {
            revision,
            local: self.local_revision,
            show_remote_branches: design.show_remote_branches,
            show_tags: design.show_tags,
            show_stashes: design.show_stashes,
        };
        let mut cache = self.cache.borrow_mut();
        if cache.key.as_ref() == Some(&key) {
            return cache.items.clone();
        }
        let items = Rc::new(self.flatten(data, upstreams, remotes, design));
        cache.key = Some(key);
        cache.items = items.clone();
        items
    }

    /// Every visible line of the section list, in drawing order: a heading for
    /// each shown section, then its rows or one placeholder. A collapsed
    /// section contributes its heading alone.
    fn flatten(
        &self,
        data: SidebarData,
        upstreams: &HashMap<String, UpstreamInfo>,
        remotes: &[RemoteInfo],
        design: &Design,
    ) -> Vec<SidebarItem> {
        let SidebarData {
            local,
            remote,
            mut tags,
            mut stashes,
            ..
        } = data;
        let mut items =
            Vec::with_capacity(local.len() + remote.len() + tags.len() + stashes.len() + 8);
        let mut remote_row_index = 0;

        for section in Section::ALL {
            if section.hidden(design) {
                continue;
            }
            let count = match section {
                Section::Local => local.len(),
                Section::Remote => remote.len(),
                Section::Tags => tags.len(),
                Section::Stashes => stashes.len(),
            };
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
                    if local.is_empty() {
                        items.push(placeholder(section));
                        continue;
                    }
                    for (index, name) in natural_sorted(&local).into_iter().enumerate() {
                        items.push(SidebarItem::LocalBranch {
                            name: name.to_owned(),
                            index,
                            upstream: upstreams.get(name).and_then(upstream_label),
                        });
                    }
                }
                Section::Tags => {
                    if tags.is_empty() {
                        items.push(placeholder(section));
                        continue;
                    }
                    // Tags are the largest section of a real repository, so
                    // they move their strings instead of copying them.
                    for (index, name) in natural_sorted_owned(std::mem::take(&mut tags))
                        .into_iter()
                        .enumerate()
                    {
                        items.push(SidebarItem::Tag { name, index });
                    }
                }
                // Stash rows keep `stash@{N}` order: the index is the identity.
                Section::Stashes => {
                    if stashes.is_empty() {
                        items.push(placeholder(section));
                        continue;
                    }
                    for (index, name) in std::mem::take(&mut stashes).into_iter().enumerate() {
                        items.push(SidebarItem::Stash { name, index });
                    }
                }
                // Remote branches are grouped under their remote, which is the
                // first path component of `origin/main`. A configured remote is
                // listed even before anything was fetched from it.
                Section::Remote => {
                    if remote.is_empty() && remotes.is_empty() {
                        items.push(placeholder(section));
                        continue;
                    }
                    for group in remote_groups(&remote, remotes) {
                        let collapsed = self
                            .remote_groups
                            .get(&group.name)
                            .copied()
                            .unwrap_or(false);
                        items.push(SidebarItem::RemoteGroup {
                            name: group.name.clone(),
                            fetch_url: group.fetch_url.clone(),
                            count: group.branches.len(),
                            collapsed,
                        });
                        if collapsed {
                            continue;
                        }
                        if group.branches.is_empty() {
                            items.push(SidebarItem::Placeholder {
                                text: "no branches fetched yet",
                                place: PlaceholderPlace::Remote(group.name.clone()),
                            });
                            continue;
                        }
                        for name in group.branches {
                            let local_name = local_name_for_remote(&name).map(str::to_owned);
                            let existing = local_name.as_deref().is_some_and(|local_name| {
                                local.iter().any(|branch| branch == local_name)
                            });
                            items.push(SidebarItem::RemoteBranch {
                                name,
                                index: remote_row_index,
                                local_name,
                                existing,
                            });
                            remote_row_index += 1;
                        }
                    }
                }
            }
        }
        items
    }

    /// Puts the row that was at the top of the viewport back where it was when
    /// a refresh inserts or removes entries above it. The row is remembered by
    /// its key, not its index, so adding a tag above the viewport does not
    /// scroll the list by a row.
    fn restore_scroll(&self, items: &Rc<Vec<SidebarItem>>) {
        let mut anchor = self.anchor.borrow_mut();
        let Some(previous) = anchor.items.replace(items.clone()) else {
            return;
        };
        // Nothing was rebuilt, so the index and the identity agree already.
        if Rc::ptr_eq(&previous, items) || previous.is_empty() {
            return;
        }

        let base = self.scroll.0.borrow().base_handle.clone();
        let height = px(ITEM_HEIGHT);
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

    fn dialog_overlay(&self, colors: &Colors, entity: &Entity<RepoView>) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let DialogKind::Create(form) = &dialog.kind;
        let card = create_card(form, colors, entity, dialog.focus.as_ref());
        let viewport = dialog.viewport;

        // A deferred layer so the dialog is not clipped by the sidebar. The app
        // has no `gpui_component::Root` view, so a modal window layer is not
        // available; painting in window coordinates is enough for a dialog.
        Some(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .w(viewport.width)
                        .h(viewport.height)
                        .flex()
                        .items_center()
                        .justify_center()
                        .occlude()
                        .bg(hsla(0.0, 0.0, 0.0, 0.35))
                        .child(card),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    pub(crate) fn start(&mut self, id: OperationId, label: String, delete: Option<(String, bool)>) {
        // A forced delete that fails again is reported, not offered a third time.
        let delete = delete.and_then(|(name, forced)| (!forced).then_some(name));
        self.running.insert(id, Running { label, delete });
    }

    /// Takes the branch to offer a forced delete for, if git refused one.
    pub(crate) fn take_force_delete(&mut self) -> Option<String> {
        self.force_delete.take()
    }

    /// Records that an operation finished. Returns the label it was started
    /// with so the caller can name the operation in the status bar, and opens
    /// the force-delete offer when git refused an unforced delete.
    pub(crate) fn finish(&mut self, id: OperationId, error: Option<&str>) -> Option<String> {
        let running = self.running.remove(&id)?;
        if let (Some(error), Some(name)) = (error, running.delete)
            && is_not_fully_merged(error)
        {
            self.force_delete = Some(name);
        }
        Some(running.label)
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.running.is_empty()
    }

    pub(crate) fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// Marks the ref the history jumped to, replacing the previous highlight.
    pub(crate) fn reveal_ref(&mut self, kind: RefKind, short_name: &str) {
        self.reveal.reveal_ref(kind, short_name);
    }

    /// Like [`Self::reveal_ref`] for `stash@{index}`.
    pub(crate) fn reveal_stash(&mut self, index: usize) {
        self.reveal.reveal_stash(index);
    }

    /// Drops the reveal highlight, e.g. once a commit row is selected.
    pub(crate) fn clear_revealed(&mut self) {
        self.reveal.clear();
    }

    /// The ref history is limited to, as its short name, while "show only" is
    /// active. `None` means the whole history.
    pub(crate) fn scope_name(&self) -> Option<String> {
        self.scope.clone()
    }

    pub(crate) fn set_scope(&mut self, name: Option<String>) {
        self.scope = name;
    }

    pub(crate) fn toggle(&mut self, index: usize) {
        if let Some(collapsed) = self.collapsed.get_mut(index) {
            *collapsed = !*collapsed;
            self.local_revision = self.local_revision.wrapping_add(1);
        }
    }

    pub(crate) fn toggle_remote_group(&mut self, group: String) {
        let collapsed = self.remote_groups.entry(group).or_insert(false);
        *collapsed = !*collapsed;
        self.local_revision = self.local_revision.wrapping_add(1);
    }

    pub(crate) fn set_viewport(&mut self, viewport: Size<Pixels>) {
        self.viewport = viewport;
    }

    pub(crate) fn set_dialog_focus(&mut self, focus: FocusHandle) {
        self.dialog_focus = Some(focus);
    }

    pub(crate) fn open_create(&mut self, input: Entity<InputState>, start: BranchStart) {
        self.dialog = Some(Dialog {
            kind: DialogKind::Create(CreateForm {
                input,
                start,
                checkout: true,
                error: None,
            }),
            viewport: self.viewport,
            focus: self.dialog_focus.clone(),
        });
    }

    pub(crate) fn close_dialog(&mut self) {
        self.dialog = None;
    }

    pub(crate) fn set_checkout(&mut self, checkout: bool) {
        if let Some(Dialog {
            kind: DialogKind::Create(form),
            ..
        }) = &mut self.dialog
        {
            form.checkout = checkout;
        }
    }

    pub(crate) fn set_create_error(&mut self, message: String) {
        if let Some(Dialog {
            kind: DialogKind::Create(form),
            ..
        }) = &mut self.dialog
        {
            form.error = Some(message);
        }
    }

    /// The create dialog's input, validated. `None` when it is not open.
    pub(crate) fn create_request(&self, cx: &App) -> Option<Result<CreateRequest, String>> {
        let Dialog {
            kind: DialogKind::Create(form),
            ..
        } = self.dialog.as_ref()?;
        let name = form.input.read(cx).value().to_string();
        Some(match validate_branch_name(&name) {
            Ok(()) => Ok(CreateRequest {
                name,
                start_point: form.start.point.clone(),
                checkout: form.checkout,
            }),
            Err(message) => Err(message.to_owned()),
        })
    }
}

/// The line a section shows when it holds no entries.
fn placeholder(section: Section) -> SidebarItem {
    SidebarItem::Placeholder {
        text: section.empty_label(),
        place: PlaceholderPlace::Section(section),
    }
}

/// The names in natural order, taking the list so the caller can move each
/// string straight into a list item.
fn natural_sorted_owned(mut names: Vec<String>) -> Vec<String> {
    names.sort_by(|a, b| natural_cmp(a, b));
    names
}

/// Builds the element for one visible list item.
#[allow(clippy::too_many_arguments)]
fn item_element(
    item: &SidebarItem,
    head: Option<&str>,
    reveal: &RevealState,
    enabled: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    match item {
        SidebarItem::SectionHeader {
            section,
            count,
            open,
        } => section_header(*section, *count, *open, busy, colors, entity),
        SidebarItem::RemoteGroup {
            name,
            fetch_url,
            count,
            collapsed,
        } => remote_group_header(
            name,
            fetch_url.as_deref(),
            *count,
            *collapsed,
            colors,
            entity,
        ),
        SidebarItem::LocalBranch {
            name,
            index,
            upstream,
        } => local_row(
            name,
            *index,
            head == Some(name.as_str()),
            upstream.as_deref(),
            reveal.is_ref(RefKind::LocalBranch, name),
            enabled,
            busy,
            colors,
            entity,
        ),
        SidebarItem::RemoteBranch {
            name,
            index,
            local_name,
            existing,
        } => remote_row(
            name,
            *index,
            local_name.as_deref(),
            *existing,
            reveal.is_ref(RefKind::RemoteBranch, name),
            enabled,
            busy,
            colors,
            entity,
        ),
        SidebarItem::Tag { name, index } => tag_row(
            name,
            *index,
            reveal.is_ref(RefKind::Tag, name),
            enabled,
            busy,
            colors,
            entity,
        ),
        // The stash row and its menu live in `stash`, which can read the
        // entry's age and source branch from the snapshot.
        SidebarItem::Stash { name, index } => crate::stash::sidebar_row(
            name,
            *index,
            reveal.is_stash(*index),
            enabled,
            busy,
            colors,
            entity,
        ),
        SidebarItem::Placeholder { text, place } => {
            empty_row_at(text, place.indent(), colors).into_any_element()
        }
    }
}

/// A section's heading: the fold control and the number of entries it holds.
fn section_header(
    section: Section,
    count: usize,
    open: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let index = section.index();
    let mut header =
        crate::ui::section_header(("branch-section", index), section.title(), &colors.design)
            .count(count)
            .disclosure(open)
            .icon(style::glyph_icon(
                section.icon(),
                14.0,
                colors.muted_foreground,
            ))
            // A list item is only as tall as the heading measured, so every
            // section heading must match a row's height.
            .h(px(ITEM_HEIGHT))
            .on_click(move |_, cx| {
                entity.update(cx, |view, cx| {
                    view.branches.toggle(index);
                    cx.notify();
                });
            });
    if busy && section == Section::Local {
        header = header.trailing(
            div()
                .flex_none()
                .text_size(px(9.0))
                .font_weight(FontWeight::NORMAL)
                .child("working…"),
        );
    }
    header.into_any_element()
}

/// A remote's own collapsing header, above its branch list.
fn remote_group_header(
    name: &str,
    fetch_url: Option<&str>,
    count: usize,
    collapsed: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let click_group = name.to_owned();
    let mut header =
        crate::ui::section_header(format!("remote-group-{name}"), name, &colors.design)
            .disclosure(!collapsed)
            .icon(style::glyph_icon(
                style::glyph::CLOUD,
                14.0,
                colors.sidebar_foreground,
            ))
            .text_size(px(colors.design.small()))
            .text_color(colors.sidebar_foreground)
            .h(px(ITEM_HEIGHT))
            .on_click(move |_, cx| {
                let group = click_group.clone();
                entity.update(cx, |view, cx| {
                    view.branches.toggle_remote_group(group);
                    cx.notify();
                });
            });
    if count > 0 {
        header = header.count(count);
    }
    match fetch_url {
        Some(url) => {
            let url = url.to_owned();
            // The header's own elements consume the click, so the tooltip
            // goes on a wrapper that is as wide as the row.
            div()
                .id(format!("remote-group-url-{name}"))
                .w_full()
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(url.clone()).build(window, cx)
                })
                .child(header)
                .into_any_element()
        }
        None => header.into_any_element(),
    }
}

#[allow(clippy::too_many_arguments)]
fn local_row(
    name: &str,
    index: usize,
    active: bool,
    upstream: Option<&str>,
    revealed: bool,
    enabled: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let mut row = row_with_icon(
        ("local-branch", index),
        Some(style::glyph::BRANCH),
        name,
        crate::ui::RowState::NORMAL.head(active).revealed(revealed),
        colors,
    )
    .cursor_pointer();
    if let Some(label) = upstream {
        row = row.child(
            div()
                .flex_none()
                .text_size(px(colors.design.tiny()))
                .text_color(colors.muted_foreground)
                .child(label.to_owned()),
        );
    }

    if !enabled {
        return row.into_any_element();
    }

    // One click shows the branch in the graph; a second one checks it out.
    let click_entity = entity.clone();
    let branch = name.to_owned();
    row = row.on_click(move |event, window, cx| {
        let branch = branch.clone();
        click_entity.update(cx, |view, cx| {
            if event.click_count() < 2 {
                view.reveal_ref(RefKind::LocalBranch, &branch, window, cx);
            } else if !busy && !active {
                let label = format!("check out {branch}");
                view.run_operation(Operation::Checkout { branch }, label, window, cx);
            }
        });
    });

    let entity_menu = entity.clone();
    let branch = name.to_owned();
    row.context_menu(move |menu, _window, _cx| {
        menu.item(
            PopupMenuItem::new("Checkout")
                .disabled(busy || active)
                .on_click({
                    let entity = entity_menu.clone();
                    let branch = branch.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let branch = branch.clone();
                        entity.update(cx, |view, cx| {
                            let label = format!("check out {branch}");
                            view.run_operation(Operation::Checkout { branch }, label, window, cx);
                        });
                    }
                }),
        )
        .item(PopupMenuItem::new("Show only this branch").on_click({
            let entity = entity_menu.clone();
            let branch = branch.clone();
            move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                let branch = branch.clone();
                entity.update(cx, |view, cx| {
                    view.show_only_ref(RefKind::LocalBranch, &branch, cx)
                });
            }
        }))
        .item(
            PopupMenuItem::new("Merge into current branch")
                .disabled(busy || active)
                .on_click({
                    let entity = entity_menu.clone();
                    let branch = branch.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let branch = branch.clone();
                        entity.update(cx, |view, cx| {
                            let label = format!("merge {branch} into the current branch");
                            view.run_operation(
                                Operation::Merge {
                                    branch,
                                    no_ff: false,
                                },
                                label,
                                window,
                                cx,
                            );
                        });
                    }
                }),
        )
        .item(
            PopupMenuItem::new("Create branch from here…")
                .disabled(busy)
                .on_click({
                    let entity = entity_menu.clone();
                    let point = branch.clone();
                    let from = format!("from {branch}");
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let point = point.clone();
                        let from = from.clone();
                        entity.update(cx, |view, cx| {
                            view.open_create_dialog(Some(point), from, window, cx);
                        });
                    }
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new("Delete…")
                .disabled(busy || active)
                .on_click({
                    let entity = entity_menu.clone();
                    let branch = branch.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let branch = branch.clone();
                        entity.update(cx, |view, cx| {
                            view.confirm_delete_branch(branch, window, cx);
                        });
                    }
                }),
        )
    })
    .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn remote_row(
    name: &str,
    index: usize,
    local_name: Option<&str>,
    existing: bool,
    revealed: bool,
    enabled: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let label = local_name.unwrap_or(name);
    let row_element = row(
        ("remote-branch", index),
        label,
        crate::ui::RowState::NORMAL.revealed(revealed),
        colors,
    )
    .pl(px(42.0));
    let Some(local_name) = local_name.map(str::to_owned) else {
        return row_element.into_any_element();
    };
    if !enabled {
        return row_element.into_any_element();
    }

    // Double-clicking `origin/foo` switches to `foo` when it exists and
    // creates it from the remote ref otherwise.
    let row_element = {
        let click_entity = entity.clone();
        let remote = name.to_owned();
        let target = local_name.clone();
        row_element
            .cursor_pointer()
            .on_click(move |event, window, cx| {
                let remote = remote.clone();
                let target = target.clone();
                click_entity.update(cx, |view, cx| {
                    if event.click_count() < 2 {
                        view.reveal_ref(RefKind::RemoteBranch, &remote, window, cx);
                    } else if !busy {
                        checkout_remote(view, &remote, &target, existing, window, cx)
                    }
                });
            })
    };

    let entity_menu = entity.clone();
    let remote = name.to_owned();
    row_element
        .context_menu(move |menu, _window, _cx| {
            menu.item(PopupMenuItem::new("Checkout").disabled(busy).on_click({
                let entity = entity_menu.clone();
                let remote = remote.clone();
                let target = local_name.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let remote = remote.clone();
                    let target = target.clone();
                    entity.update(cx, |view, cx| {
                        checkout_remote(view, &remote, &target, existing, window, cx)
                    });
                }
            }))
            .item(PopupMenuItem::new("Show only this branch").on_click({
                let entity = entity_menu.clone();
                let remote = remote.clone();
                move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                    let remote = remote.clone();
                    entity.update(cx, |view, cx| {
                        view.show_only_ref(RefKind::RemoteBranch, &remote, cx)
                    });
                }
            }))
            .item(
                PopupMenuItem::new("Merge into current branch")
                    .disabled(busy)
                    .on_click({
                        let entity = entity_menu.clone();
                        let remote = remote.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let remote = remote.clone();
                            entity.update(cx, |view, cx| {
                                let label = format!("merge {remote} into the current branch");
                                view.run_operation(
                                    Operation::Merge {
                                        branch: remote,
                                        no_ff: false,
                                    },
                                    label,
                                    window,
                                    cx,
                                );
                            });
                        }
                    }),
            )
            .item(
                PopupMenuItem::new("Create branch from here…")
                    .disabled(busy)
                    .on_click({
                        let entity = entity_menu.clone();
                        let remote = remote.clone();
                        let from = format!("from {remote}");
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let remote = remote.clone();
                            let from = from.clone();
                            entity.update(cx, |view, cx| {
                                view.open_create_dialog(Some(remote), from, window, cx);
                            });
                        }
                    }),
            )
        })
        .into_any_element()
}

fn tag_row(
    name: &str,
    index: usize,
    revealed: bool,
    enabled: bool,
    busy: bool,
    colors: &Colors,
    entity: Entity<RepoView>,
) -> AnyElement {
    let tag = name.to_owned();
    let menu_entity = entity.clone();
    let menu_tag = tag.clone();
    let row_element = row_with_icon(
        ("tag", index),
        Some(style::glyph::TAG),
        name,
        crate::ui::RowState::NORMAL.revealed(revealed),
        colors,
    )
    .cursor_pointer()
    .on_click(move |_, window, cx| {
        entity.update(cx, |view, cx| {
            view.reveal_ref(RefKind::Tag, &tag, window, cx)
        });
    });
    if !enabled {
        return row_element.into_any_element();
    }
    row_element
        .context_menu(move |menu, _window, _cx| {
            menu.item(
                PopupMenuItem::new("Show only this tag")
                    .disabled(busy)
                    .on_click({
                        let entity = menu_entity.clone();
                        let tag = menu_tag.clone();
                        move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                            let tag = tag.clone();
                            entity
                                .update(cx, |view, cx| view.show_only_ref(RefKind::Tag, &tag, cx));
                        }
                    }),
            )
        })
        .into_any_element()
}

impl RepoView {
    /// Reloads history limited to `short_name`'s history and remembers the name
    /// for the sidebar banner. The full ref name is what the walk resolves, so
    /// a branch that is not `refs/heads/...` still reads exactly.
    pub(crate) fn show_only_ref(
        &mut self,
        kind: RefKind,
        short_name: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let spec = session
            .snapshot()
            .refs
            .iter()
            .find(|info| info.kind == kind && info.short_name == short_name)
            .map(|info| info.full_name.clone())
            .unwrap_or_else(|| short_name.to_owned());
        session.set_history_scope(HistoryScope::Refs(vec![spec]));
        self.branches.set_scope(Some(short_name.to_owned()));
        self.branches.clear_revealed();
        cx.notify();
    }

    /// Restores every ref as a history starting point, undoing "show only".
    pub(crate) fn show_all_refs(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        session.set_history_scope(HistoryScope::All);
        self.branches.set_scope(None);
        self.branches.clear_revealed();
        cx.notify();
    }
}

/// Checks out `remote` as `local`, creating `local` from the remote ref when it
/// does not exist yet.
fn checkout_remote(
    view: &mut RepoView,
    remote: &str,
    local: &str,
    existing: bool,
    window: &mut Window,
    cx: &mut Context<RepoView>,
) {
    if existing {
        let label = format!("check out {local}");
        view.run_operation(
            Operation::Checkout {
                branch: local.to_owned(),
            },
            label,
            window,
            cx,
        );
    } else {
        let label = format!("create branch {local} from {remote}");
        view.run_operation(
            Operation::CreateBranch {
                name: local.to_owned(),
                start_point: Some(remote.to_owned()),
                checkout: true,
            },
            label,
            window,
            cx,
        );
    }
}

fn row(
    id: impl Into<ElementId>,
    name: &str,
    state: crate::ui::RowState,
    colors: &Colors,
) -> Stateful<Div> {
    row_with_icon(id, None, name, state, colors)
}

fn row_with_icon(
    id: impl Into<ElementId>,
    item_icon: Option<&'static [u8]>,
    name: &str,
    state: crate::ui::RowState,
    colors: &Colors,
) -> Stateful<Div> {
    let name = strip_stash_prefix(name);
    let row = crate::ui::list_row(id, state, &colors.design)
        .h(px(ITEM_HEIGHT))
        .rounded(px(7.0))
        .text_size(px(colors.design.small()))
        .text_color(colors.sidebar_foreground);
    let row = match item_icon {
        Some(item_icon) => row.child(glyph(item_icon, 14.0, colors.muted_foreground)),
        None => row,
    };
    row.child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
}

fn glyph(data: &'static [u8], size: f32, color: Hsla) -> Icon {
    style::glyph_icon(data, size, color)
}

/// The sidebar lists `stash@{0}: message`; the row shows the message alone, as
/// the design does, while the click handler keeps using the stash's index.
fn strip_stash_prefix(name: &str) -> &str {
    match name
        .strip_prefix("stash@{")
        .and_then(|rest| rest.split_once("}: "))
    {
        Some((_, message)) => message,
        None => name,
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

/// The names in natural order, so a numbered ref reads `feature-2` before
/// `feature-10` instead of after it.
fn natural_sorted(names: &[String]) -> Vec<&str> {
    let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
    sorted.sort_by(|a, b| natural_cmp(a, b));
    sorted
}

/// Orders names the way a person reads them: a run of ASCII digits compares as
/// a number, everything else compares case-insensitively, and names that only
/// differ in case fall back to their original text so the order is stable.
///
/// This is what keeps `feature-1, feature-2, …, feature-10` and
/// `v1.9.0, v1.10.0` in sequence; the snapshot's own order is lexical and puts
/// `feature-10` between `feature-1` and `feature-2`.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut left = a.as_bytes();
    let mut right = b.as_bytes();
    loop {
        match (left.first(), right.first()) {
            // The common prefix matches, so only case can still separate them.
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) if l.is_ascii_digit() && r.is_ascii_digit() => {
                let (left_run, left_rest) = split_digit_run(left);
                let (right_run, right_rest) = split_digit_run(right);
                match cmp_digit_runs(left_run, right_run) {
                    // Equally large runs may still differ in leading zeros.
                    Ordering::Equal => {
                        left = left_rest;
                        right = right_rest;
                    }
                    order => return order,
                }
            }
            (Some(l), Some(r)) => match l.to_ascii_lowercase().cmp(&r.to_ascii_lowercase()) {
                Ordering::Equal => {
                    left = &left[1..];
                    right = &right[1..];
                }
                order => return order,
            },
        }
    }
}

/// Splits a byte string at the end of its leading digit run.
fn split_digit_run(bytes: &[u8]) -> (&[u8], &[u8]) {
    let end = bytes
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(bytes.len());
    bytes.split_at(end)
}

/// Compares two digit runs by value without parsing them, so a run longer than
/// `u64` is ordered as correctly as a short one: strip the leading zeros, then
/// the shorter run is the smaller number, and equal lengths compare lexically.
fn cmp_digit_runs(a: &[u8], b: &[u8]) -> Ordering {
    let a = without_leading_zeros(a);
    let b = without_leading_zeros(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn without_leading_zeros(run: &[u8]) -> &[u8] {
    let start = run
        .iter()
        .position(|&byte| byte != b'0')
        .unwrap_or(run.len());
    &run[start..]
}

/// A section's "nothing here" line. Its indent follows the place it belongs
/// to: branch rows sit under the section labels, remote branches under a
/// remote's own header.
fn empty_row_at(text: &str, indent: Pixels, colors: &Colors) -> Div {
    div()
        .pl(indent)
        .pr_2()
        .h(px(ITEM_HEIGHT))
        .flex()
        .items_center()
        .text_size(px(11.0))
        .text_color(colors.muted_foreground)
        .child(text.to_owned())
}

fn create_card(
    form: &CreateForm,
    colors: &Colors,
    entity: &Entity<RepoView>,
    focus: Option<&FocusHandle>,
) -> AnyElement {
    let confirm = {
        let entity = entity.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| view.submit_branch_dialog(window, cx));
        }
    };
    let cancel = {
        let entity = entity.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| {
                view.branches.close_dialog();
                view.focus_root(window, cx);
                cx.notify();
            });
        }
    };
    let toggle = {
        let entity = entity.clone();
        move |checked: &bool, _: &mut Window, cx: &mut App| {
            let checked = *checked;
            entity.update(cx, |view, cx| {
                view.branches.set_checkout(checked);
                cx.notify();
            });
        }
    };

    let mut card = dialog_card(colors, entity, focus)
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Create branch"),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child(format!("New branch {}", form.start.label)),
        )
        .child(Input::new(&form.input).w_full())
        .child(
            Checkbox::new("branch-create-checkout")
                .checked(form.checkout)
                .label("Check out after creating")
                .on_change(toggle),
        );
    if let Some(error) = &form.error {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.danger)
                .child(error.clone()),
        );
    }
    card.child(
        div()
            .flex()
            .justify_end()
            .gap_2()
            .pt_1()
            .child(
                crate::ui::button("branch-create-cancel", "Cancel")
                    .size(crate::ui::ButtonSize::Medium)
                    .on_click(cancel),
            )
            .child(
                crate::ui::button("branch-create-ok", "Create branch")
                    .size(crate::ui::ButtonSize::Medium)
                    .on_click(confirm),
            ),
    )
    .into_any_element()
}

fn dialog_card(colors: &Colors, entity: &Entity<RepoView>, focus: Option<&FocusHandle>) -> Div {
    let confirm = {
        let entity = entity.clone();
        move |_: &ConfirmBranchDialog, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| view.submit_branch_dialog(window, cx));
        }
    };
    let cancel = {
        let entity = entity.clone();
        move |_: &CancelBranchDialog, window: &mut Window, cx: &mut App| {
            entity.update(cx, |view, cx| {
                view.branches.close_dialog();
                view.focus_root(window, cx);
                cx.notify();
            });
        }
    };
    div()
        .w(px(380.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .bg(colors.overlay)
        .border_1()
        .border_color(colors.border)
        .text_size(px(12.0))
        .text_color(colors.foreground)
        .occlude()
        .when_some(focus, |card, focus| card.track_focus(focus))
        .key_context(BRANCH_DIALOG)
        .on_action(confirm)
        .on_action(cancel)
}

/// Offers the forced delete after git refused to delete an unmerged branch.
///
/// The session event that learns about the refusal has no window, so the dialog
/// is opened on the active one. It always asks, even when confirmations are off:
/// the forced delete is a different, stronger action than the one the user
/// chose, and skipping the ask would silently escalate it.
pub(crate) fn offer_force_delete(entity: Entity<RepoView>, name: String, cx: &mut App) {
    let Some(handle) = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())
    else {
        log::warn!("cannot offer the forced delete without a window");
        return;
    };
    let entity = entity.downgrade();
    let _ = handle.update(cx, |_, window, cx| {
        crate::ui::confirm(
            window,
            cx,
            crate::ui::Confirm::new(
                format!("Delete branch {name}?"),
                "The branch is not fully merged. Forcing the delete loses the commits it points at.",
            )
            .ok_label("Force delete")
            .always_ask(),
            move |window, cx| {
                let name = name.clone();
                let _ = entity.update(cx, |view, cx| view.delete_branch(name, true, window, cx));
            },
        );
    });
}

/// `git check-ref-format` rules that apply to `refs/heads/<name>`, plus the one
/// rule the CLI enforces by accident: a leading `-` would parse as an option.
pub(crate) fn validate_branch_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Enter a branch name.");
    }
    if name == "@" {
        return Err("'@' is shorthand for HEAD, not a branch name.");
    }
    if name.starts_with('-') {
        return Err("A branch name must not start with '-'.");
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("A branch name must not start or end with '/'.");
    }
    if name.contains("//") {
        return Err("A branch name must not contain '//'.");
    }
    if name.contains("..") {
        return Err("A branch name must not contain '..'.");
    }
    if name.contains("@{") {
        return Err("A branch name must not contain '@{'.");
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("A branch name must not contain spaces or control characters.");
    }
    if name.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err("A branch name must not contain any of ~ ^ : ? * [ \\.");
    }
    if name.ends_with('.') {
        return Err("A branch name must not end with '.'.");
    }
    if name.split('/').any(|part| {
        part.is_empty() || part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock")
    }) {
        return Err("Each part must not start with '.' or end with '.' or '.lock'.");
    }
    Ok(())
}

/// The sidebar label for a branch's upstream: `↑2 ↓1`, `↑3`, `↓1`, `gone`, or
/// nothing when the branch is level with its upstream.
pub(crate) fn upstream_label(upstream: &UpstreamInfo) -> Option<String> {
    if upstream.gone {
        return Some("gone".to_owned());
    }
    let mut label = String::new();
    if upstream.ahead > 0 {
        label.push_str(&format!("↑{}", upstream.ahead));
    }
    if upstream.behind > 0 {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push_str(&format!("↓{}", upstream.behind));
    }
    (!label.is_empty()).then_some(label)
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

/// `git branch -d` refuses a branch with commits that are not reachable from
/// HEAD; that message is what turns the delete dialog into a force offer.
fn is_not_fully_merged(error: &str) -> bool {
    error.contains("not fully merged")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(name: &str, ahead: u32, behind: u32, gone: bool) -> UpstreamInfo {
        UpstreamInfo {
            name: name.to_owned(),
            ahead,
            behind,
            gone,
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
            upstream_label(&upstream("origin/gone", 0, 0, true)).as_deref(),
            Some("gone"),
            "a deleted upstream is more important than the counts"
        );
    }

    #[test]
    fn branch_names_follow_git_check_ref_format() {
        for name in [
            "main",
            "feat/branch-sidebar",
            "fix-123",
            "v1.0.0",
            "a/b/c",
            "release.locked",
        ] {
            assert_eq!(validate_branch_name(name), Ok(()), "{name} should be valid");
        }

        for name in [
            "",
            "@",
            "-foo",
            "/foo",
            "foo/",
            "foo bar",
            "foo\tbar",
            "foo..bar",
            "foo/../bar",
            "foo//bar",
            "foo@{1}",
            "foo~1",
            "foo^",
            "foo:bar",
            "foo?",
            "foo*",
            "foo[bar]",
            "foo\\bar",
            "foo.",
            "foo./bar",
            "foo/.bar",
            ".foo",
            "foo.lock",
            "foo.lock/bar",
        ] {
            assert!(
                validate_branch_name(name).is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn stash_rows_show_the_message_without_the_index() {
        assert_eq!(
            strip_stash_prefix("stash@{0}: WIP on diff options"),
            "WIP on diff options"
        );
        assert_eq!(strip_stash_prefix("main"), "main");
        assert_eq!(strip_stash_prefix("stash@{0}"), "stash@{0}");
    }

    #[test]
    fn revealing_a_ref_moves_the_highlight_until_a_commit_is_selected() {
        let mut reveal = RevealState::default();
        assert!(!reveal.is_ref(RefKind::LocalBranch, "main"));

        reveal.reveal_ref(RefKind::LocalBranch, "main");
        assert!(reveal.is_ref(RefKind::LocalBranch, "main"));

        // Another ref replaces the highlight instead of adding to it.
        reveal.reveal_ref(RefKind::RemoteBranch, "origin/main");
        assert!(!reveal.is_ref(RefKind::LocalBranch, "main"));
        assert!(reveal.is_ref(RefKind::RemoteBranch, "origin/main"));

        reveal.reveal_stash(2);
        assert!(!reveal.is_ref(RefKind::RemoteBranch, "origin/main"));
        assert!(reveal.is_stash(2));
        assert!(!reveal.is_stash(1));

        // Selecting a commit clears whichever row was revealed.
        reveal.clear();
        assert!(!reveal.is_stash(2));
        assert!(!reveal.is_ref(RefKind::LocalBranch, "main"));
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

    #[test]
    fn only_an_unmerged_delete_offers_force() {
        assert!(is_not_fully_merged(
            "error: the branch 'topic' is not fully merged\nhint: ..."
        ));
        assert!(!is_not_fully_merged("fatal: branch 'topic' not found"));
        assert!(!is_not_fully_merged("error: cannot delete branch 'main'"));
    }

    #[test]
    fn a_failed_unforced_delete_leaves_a_force_offer() {
        let mut branches = Branches::default();
        branches.start(
            OperationId(7),
            "delete branch topic".to_owned(),
            Some(("topic".to_owned(), false)),
        );
        let label = branches.finish(
            OperationId(7),
            Some("error: the branch 'topic' is not fully merged"),
        );
        assert_eq!(label.as_deref(), Some("delete branch topic"));
        assert_eq!(
            branches.take_force_delete().as_deref(),
            Some("topic"),
            "the offer must rerun the delete with force"
        );
        assert_eq!(
            branches.take_force_delete(),
            None,
            "the offer is taken once, so it cannot reopen by itself"
        );
    }

    #[test]
    fn a_forced_delete_failure_does_not_loop() {
        let mut branches = Branches::default();
        branches.start(
            OperationId(8),
            "delete branch topic".to_owned(),
            Some(("topic".to_owned(), true)),
        );
        branches.finish(
            OperationId(8),
            Some("error: the branch 'topic' is not fully merged"),
        );
        assert_eq!(branches.take_force_delete(), None);
    }

    fn sorted(names: &[String]) -> Vec<String> {
        natural_sorted(names)
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }

    #[test]
    fn numbered_branches_sort_by_their_number_not_their_text() {
        let mut shuffled: Vec<String> =
            (1..=12).map(|number| format!("feature-{number}")).collect();
        shuffled.reverse();
        shuffled.push("main".to_owned());

        let expected: Vec<String> = (1..=12)
            .map(|number| format!("feature-{number}"))
            .chain(std::iter::once("main".to_owned()))
            .collect();
        assert_eq!(sorted(&shuffled), expected);
    }

    #[test]
    fn names_that_differ_only_in_case_keep_their_original_order() {
        let names = ["main".to_owned(), "MAIN".to_owned(), "Main".to_owned()];
        assert_eq!(sorted(&names), ["MAIN", "Main", "main"]);
    }

    #[test]
    fn digit_runs_compare_as_numbers_and_letters_ignore_case() {
        assert_eq!(natural_cmp("feature-2", "feature-10"), Ordering::Less);
        assert_eq!(natural_cmp("feature-10", "feature-2"), Ordering::Greater);
        assert_eq!(natural_cmp("feature-2", "feature-2-hotfix"), Ordering::Less);
        assert_eq!(
            natural_cmp("v1.9.0", "v1.10.0"),
            Ordering::Less,
            "a version's components are numbers, not text"
        );
        assert_eq!(natural_cmp("v2.0.0", "v1.10.0"), Ordering::Greater);
        assert_eq!(natural_cmp("topic-1", "topic-a"), Ordering::Less);
        // Only case separates these, so the original text breaks the tie.
        assert_eq!(natural_cmp("Feature-1", "feature-1"), Ordering::Less);
        assert_eq!(natural_cmp("feature-1", "Feature-1"), Ordering::Greater);
        assert_eq!(natural_cmp("feature-1", "feature-1"), Ordering::Equal);
    }

    #[test]
    fn leading_zeros_do_not_change_a_run_s_value() {
        assert_eq!(natural_cmp("feature-007", "feature-8"), Ordering::Less);
        assert_eq!(natural_cmp("feature-010", "feature-9"), Ordering::Greater);
        assert_eq!(
            natural_cmp("feature-07", "feature-7"),
            Ordering::Less,
            "equal numbers still need a stable order"
        );
    }

    #[test]
    fn digit_runs_too_long_for_u64_still_order_by_value() {
        let forty_nines = "9".repeat(40);
        // 41 digits is more than 40, whatever the value.
        let forty_one_digits = format!("1{}", "0".repeat(40));
        assert_eq!(natural_cmp(&forty_nines, &forty_one_digits), Ordering::Less);

        let just_below = format!("{}{}", "9".repeat(39), "8");
        assert_eq!(natural_cmp(&just_below, &forty_nines), Ordering::Less);
        assert_eq!(natural_cmp(&forty_nines, &forty_nines), Ordering::Equal);
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
        branches.flatten(data, upstreams, remotes, &Design::default())
    }

    fn flatten(branches: &Branches, data: SidebarData) -> Vec<SidebarItem> {
        flatten_with(branches, data, &HashMap::new(), &[])
    }

    #[test]
    fn sections_flatten_into_headings_rows_and_placeholders() {
        let branches = Branches::default();
        let mut upstreams = HashMap::new();
        upstreams.insert("main".to_owned(), upstream("origin/main", 2, 1, false));

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
                    index: 0,
                    upstream: Some("↑2 ↓1".into()),
                },
                SidebarItem::LocalBranch {
                    name: "topic".into(),
                    index: 1,
                    upstream: None,
                },
                SidebarItem::SectionHeader {
                    section: Section::Remote,
                    count: 0,
                    open: true,
                },
                SidebarItem::Placeholder {
                    text: "no remotes",
                    place: PlaceholderPlace::Section(Section::Remote),
                },
                SidebarItem::SectionHeader {
                    section: Section::Tags,
                    count: 0,
                    open: true,
                },
                SidebarItem::Placeholder {
                    text: "no tags",
                    place: PlaceholderPlace::Section(Section::Tags),
                },
                SidebarItem::SectionHeader {
                    section: Section::Stashes,
                    count: 0,
                    open: true,
                },
                SidebarItem::Placeholder {
                    text: "no stashes",
                    place: PlaceholderPlace::Section(Section::Stashes),
                },
            ]
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
            !items
                .iter()
                .any(|item| item.is(&ItemKey::SectionEmpty(Section::Tags))),
            "a collapsed section draws no placeholder either"
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
                    text: "no branches fetched yet",
                    place: PlaceholderPlace::Remote("backup".into()),
                },
                &SidebarItem::RemoteGroup {
                    name: "origin".into(),
                    fetch_url: Some("https://example.invalid/repo.git".into()),
                    count: 3,
                    collapsed: false,
                },
                &SidebarItem::RemoteBranch {
                    name: "origin/feature".into(),
                    index: 0,
                    local_name: Some("feature".into()),
                    existing: false,
                },
                &SidebarItem::RemoteBranch {
                    name: "origin/HEAD".into(),
                    index: 1,
                    local_name: None,
                    existing: false,
                },
                &SidebarItem::RemoteBranch {
                    name: "origin/main".into(),
                    index: 2,
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
        let tags: Vec<String> = (1..=10).map(|number| format!("v{number}")).collect();
        let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let before = Rc::new(flatten(&branches, sidebar_data(&[], &[], &tag_refs, &[])));
        branches.restore_scroll(&before);

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
        branches.restore_scroll(&after);

        assert_eq!(
            branches.scroll.0.borrow().base_handle.offset().y,
            px(-168.0),
            "the same tag must stay at the top after one is added above it"
        );
    }

    #[test]
    fn a_refresh_that_keeps_the_list_keeps_the_scroll_offset() {
        let branches = Branches::default();
        let items = Rc::new(flatten(&branches, sidebar_data(&["main"], &[], &[], &[])));
        branches.restore_scroll(&items);
        branches
            .scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.0), px(-56.0)));

        branches.restore_scroll(&items);

        assert_eq!(
            branches.scroll.0.borrow().base_handle.offset().y,
            px(-56.0),
            "an unchanged list must not move under the pointer"
        );
    }
}
