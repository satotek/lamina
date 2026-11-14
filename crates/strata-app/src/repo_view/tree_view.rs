//! The commit's file tree, in place of the diff pane.
//!
//! The listing and the blob contents come from the repository handle on the
//! background executor; this module only keeps the result that still belongs
//! to the open commit and directory. Every request carries an epoch and the
//! commit it was started for, so a slow listing from a directory the user has
//! left cannot replace the one on screen.
//!
//! The file viewer is read-only: syntax spans come from the same
//! [`Highlighter`](strata_core::highlight::Highlighter) the diff pane uses and
//! are painted with [`DiffColors`], but blobs over [`MAX_FILE_BYTES`] and
//! binary files show a notice instead of content.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, HighlightStyle, IntoElement, KeyBinding, ParentElement, StyledText,
    UniformListScrollHandle, Window, actions, div, px, uniform_list,
};
use gpui_component::ActiveTheme as _;
use gpui_kit_assets::IconName;
use strata_core::encoding::{TextEncoding, decode_for_display};
use strata_core::highlight::{Highlighter, LineHighlights, language_for_path};
use strata_core::lfs::{LfsInfo, LfsPointer, format_size, read_object, short_label};
use strata_core::{Oid, TreeEntry, TreeEntryKind};

use super::RepoView;
use super::common::icon;
use crate::data;
use crate::diff_view::{DiffColors, DiffStyle};
use crate::scrollbar::WithScrollbar as _;
use crate::settings::Settings;
use crate::style::Design;

/// Key context of the tree pane. Escape is an action here because macOS sends
/// it to the text input context first, where a raw key listener never sees it.
const TREE_VIEW: &str = "TreeBrowser";

actions!(strata_tree_view, [CloseTreeBrowser]);

/// Blobs larger than this are not loaded into the viewer.
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
/// Width of the line-number gutter in the file viewer.
const GUTTER_WIDTH: f32 = 52.0;

/// The tree shown at one commit: the current directory's listing and, when a
/// file is opened, its read-only contents.
pub(super) struct TreeBrowser {
    open: bool,
    commit: Option<Oid>,
    dir: String,
    entries: Option<Vec<TreeEntry>>,
    /// LFS metadata for the entries of the current listing, keyed by path.
    lfs: HashMap<String, LfsInfo>,
    error: Option<String>,
    /// Bumped for every listing request; a result for another epoch is stale.
    listing_epoch: u64,
    file: Option<OpenFile>,
    /// Bumped for every file request, including leaving the file.
    file_epoch: u64,
    /// Shared by every file request so a language's query is compiled once.
    highlighter: Arc<Mutex<Highlighter>>,
    scroll: UniformListScrollHandle,
}

impl Default for TreeBrowser {
    fn default() -> Self {
        Self {
            open: false,
            commit: None,
            dir: String::new(),
            entries: None,
            lfs: HashMap::new(),
            error: None,
            listing_epoch: 0,
            file: None,
            file_epoch: 0,
            highlighter: Arc::new(Mutex::new(Highlighter::new())),
            scroll: UniformListScrollHandle::default(),
        }
    }
}

struct OpenFile {
    path: String,
    content: FileContent,
    scroll: UniformListScrollHandle,
}

enum FileContent {
    Loading,
    Text {
        lines: Arc<Vec<TextLine>>,
        encoding: TextEncoding,
    },
    Notice(String),
}

struct TextLine {
    text: String,
    highlights: LineHighlights,
}

impl TreeBrowser {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    fn commit(&self) -> Option<Oid> {
        self.commit
    }

    /// The path in the breadcrumb: the open file, or the current directory.
    fn path(&self) -> &str {
        self.file
            .as_ref()
            .map_or(self.dir.as_str(), |file| file.path.as_str())
    }

    /// The encoding of the open file, when it is not UTF-8.
    fn non_utf8_encoding(&self) -> Option<TextEncoding> {
        match &self.file.as_ref()?.content {
            FileContent::Text { encoding, .. } if !encoding.is_utf8() => Some(*encoding),
            _ => None,
        }
    }

    /// Resets to the root of `commit` and returns the listing epoch to load.
    fn open(&mut self, commit: Oid) -> u64 {
        self.open = true;
        self.commit = Some(commit);
        self.dir.clear();
        self.reset_listing()
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.commit = None;
        self.dir.clear();
        self.entries = None;
        self.lfs.clear();
        self.error = None;
        self.file = None;
        self.listing_epoch += 1;
        self.file_epoch += 1;
    }

    /// Shows `dir` next; drops the open file and any listing in flight.
    fn enter_dir(&mut self, dir: String) -> u64 {
        self.dir = dir;
        self.reset_listing()
    }

    fn reset_listing(&mut self) -> u64 {
        self.entries = None;
        self.lfs.clear();
        self.error = None;
        self.file = None;
        self.file_epoch += 1;
        self.listing_epoch += 1;
        self.listing_epoch
    }

    /// Leaves the open file for its directory listing, which is still loaded.
    /// True when a file was open.
    fn leave_file(&mut self) -> bool {
        if self.file.take().is_some() {
            self.file_epoch += 1;
            true
        } else {
            false
        }
    }

    fn begin_file(&mut self, path: String) -> u64 {
        self.file = Some(OpenFile {
            path,
            content: FileContent::Loading,
            scroll: UniformListScrollHandle::default(),
        });
        self.file_epoch += 1;
        self.file_epoch
    }

    fn open_notice(&mut self, path: String, notice: String) {
        self.file = Some(OpenFile {
            path,
            content: FileContent::Notice(notice),
            scroll: UniformListScrollHandle::default(),
        });
        self.file_epoch += 1;
    }

    fn accept_listing(
        &mut self,
        epoch: u64,
        commit: Oid,
        entries: Vec<TreeEntry>,
        lfs: HashMap<String, LfsInfo>,
    ) -> bool {
        if !self.is_current_listing(epoch, commit) {
            return false;
        }
        self.entries = Some(entries);
        self.lfs = lfs;
        self.error = None;
        true
    }

    fn fail_listing(&mut self, epoch: u64, commit: Oid, error: String) -> bool {
        if !self.is_current_listing(epoch, commit) {
            return false;
        }
        self.entries = None;
        self.error = Some(error);
        true
    }

    fn is_current_listing(&self, epoch: u64, commit: Oid) -> bool {
        self.open && self.commit == Some(commit) && epoch == self.listing_epoch
    }

    fn accept_file(&mut self, epoch: u64, commit: Oid, path: &str, content: FileContent) -> bool {
        if !self.open
            || self.commit != Some(commit)
            || epoch != self.file_epoch
            || self.file.as_ref().is_none_or(|file| file.path != path)
        {
            return false;
        }
        if let Some(file) = self.file.as_mut() {
            file.content = content;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Pure helpers.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Crumb {
    label: String,
    path: String,
}

/// The breadcrumb for a directory or file path, root first.
fn breadcrumbs(path: &str) -> Vec<Crumb> {
    let mut crumbs = vec![Crumb {
        label: "root".to_owned(),
        path: String::new(),
    }];
    let mut current = String::new();
    for component in path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
    {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(component);
        crumbs.push(Crumb {
            label: component.to_owned(),
            path: current.clone(),
        });
    }
    crumbs
}

/// The directory above `dir`; the root when there is none.
fn parent_dir(dir: &str) -> String {
    dir.rsplit_once('/')
        .map_or_else(String::new, |(parent, _)| parent.to_owned())
}

/// Directories first, then by name, matching what `Repo::tree_entries`
/// promises. The UI sorts again so its tests pin the order the rows show.
fn sort_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|a, b| {
        entry_rank(a)
            .cmp(&entry_rank(b))
            .then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
    });
}

fn entry_rank(entry: &TreeEntry) -> u8 {
    u8::from(entry.kind != TreeEntryKind::Directory)
}

fn entry_icon(kind: TreeEntryKind) -> IconName {
    match kind {
        TreeEntryKind::Directory => IconName::Folder,
        TreeEntryKind::File => IconName::FileText,
        TreeEntryKind::Symlink => IconName::ArrowRight,
        TreeEntryKind::Submodule => IconName::ExternalLink,
    }
}

/// What trails an entry's name: a size for files, the kind for the entries
/// whose content the viewer cannot list.
fn entry_detail(entry: &TreeEntry) -> String {
    match entry.kind {
        TreeEntryKind::Directory => String::new(),
        TreeEntryKind::Symlink => "symlink".to_owned(),
        TreeEntryKind::Submodule => "submodule".to_owned(),
        TreeEntryKind::File => entry.size.map_or_else(String::new, format_bytes),
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f < KB {
        format!("{bytes} B")
    } else if bytes_f < MB {
        format!("{:.1} KB", bytes_f / KB)
    } else {
        format!("{:.1} MB", bytes_f / MB)
    }
}

/// Tabs expand to the next stop and the syntax spans are remapped onto the
/// rewritten text, so highlighting survives the expansion.
fn expand_tabs(line: &str, tab_width: usize, spans: LineHighlights) -> (String, LineHighlights) {
    if !line.contains('\t') {
        return (line.to_owned(), spans);
    }
    let tab_width = tab_width.max(1);
    let mut text = String::with_capacity(line.len());
    let mut offsets = vec![0; line.len() + 1];
    let mut column = 0;
    for (index, character) in line.char_indices() {
        for offset in &mut offsets[index..index + character.len_utf8()] {
            *offset = text.len();
        }
        if character == '\t' {
            let spaces = tab_width - column % tab_width;
            text.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            text.push(character);
            column += 1;
        }
    }
    offsets[line.len()] = text.len();
    let map = |offset: usize| offsets[offset.min(offsets.len() - 1)];
    let spans = spans
        .into_iter()
        .map(|(range, kind)| (map(range.start)..map(range.end), kind))
        .collect();
    (text, spans)
}

/// Splits a blob into display lines. A single trailing newline does not add an
/// empty last row, which is how editors show the end of a file.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = text.split('\n').collect::<Vec<_>>();
    if text.ends_with('\n') {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

fn file_content(
    path: &str,
    bytes: Vec<u8>,
    attribute: Option<String>,
    highlighter: &Mutex<Highlighter>,
    tab_width: usize,
) -> FileContent {
    if bytes.len() > MAX_FILE_BYTES {
        return FileContent::Notice(format!(
            "File is {} — the viewer shows files up to {}.",
            format_bytes(bytes.len() as u64),
            format_bytes(MAX_FILE_BYTES as u64)
        ));
    }
    let (decoded, encoding) = decode_for_display(&bytes, || attribute);
    // A UTF-16 file contains NUL bytes in its code units; only another
    // encoding's NUL marks a binary file.
    if !encoding.is_utf16() && bytes.contains(&0) {
        return FileContent::Notice("Binary file — contents are not shown.".to_owned());
    }
    let text = decoded.as_ref();

    let highlight = language_for_path(path)
        .and_then(|language| {
            let mut highlighter = highlighter.lock().ok()?;
            Some(highlighter.highlight(language, text))
        })
        .unwrap_or_default();
    let lines = split_lines(text)
        .into_iter()
        .enumerate()
        .map(|(index, raw)| {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            let spans = highlight.get(index).cloned().unwrap_or_default();
            let (text, highlights) = expand_tabs(raw, tab_width, spans);
            TextLine { text, highlights }
        })
        .collect();

    FileContent::Text {
        lines: Arc::new(lines),
        encoding,
    }
}

// ---------------------------------------------------------------------------
// RepoView glue.
// ---------------------------------------------------------------------------

impl RepoView {
    /// Opens the tree of the selected commit at its root.
    pub(super) fn open_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_tree_at(None, None, window, cx);
    }

    /// Opens the tree of the selected commit in `dir`, or on `file` with the
    /// file's directory loaded so Backspace returns to the surrounding listing.
    pub(super) fn open_tree_at(
        &mut self,
        dir: Option<String>,
        file: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit) = self.selection_commit else {
            return;
        };
        if self.session.is_none() {
            return;
        }
        self.discard_blame();
        let epoch = self.tree.open(commit);
        let dir = dir.unwrap_or_else(|| file.as_deref().map_or_else(String::new, parent_dir));
        let epoch = if dir.is_empty() {
            epoch
        } else {
            self.tree.enter_dir(dir)
        };
        if let Some(path) = file {
            let file_epoch = self.tree.begin_file(path.clone());
            self.request_tree_file(file_epoch, path, cx);
        }
        cx.bind_keys([KeyBinding::new("escape", CloseTreeBrowser, Some(TREE_VIEW))]);
        self.focus_root(window, cx);
        self.request_tree_listing(epoch, cx);
        cx.notify();
    }

    pub(super) fn close_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tree.is_open() {
            return;
        }
        self.tree.close();
        self.focus_root(window, cx);
        cx.notify();
    }

    /// Enters a directory from the breadcrumb, leaving any open file.
    fn open_tree_dir(&mut self, dir: String, cx: &mut Context<Self>) {
        let epoch = self.tree.enter_dir(dir);
        self.request_tree_listing(epoch, cx);
        cx.notify();
    }

    /// Backspace: leave the file for its directory, or go up one level.
    pub(super) fn tree_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree.leave_file() {
            cx.notify();
            return;
        }
        let dir = self.tree.dir.clone();
        if dir.is_empty() {
            self.close_tree(window, cx);
            return;
        }
        let parent = parent_dir(&dir);
        let epoch = self.tree.enter_dir(parent);
        self.request_tree_listing(epoch, cx);
        cx.notify();
    }

    fn tree_activate(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self
            .tree
            .entries
            .as_ref()
            .and_then(|entries| entries.get(index))
            .cloned()
        else {
            return;
        };
        match entry.kind {
            TreeEntryKind::Directory => {
                let epoch = self.tree.enter_dir(entry.path);
                self.request_tree_listing(epoch, cx);
            }
            TreeEntryKind::Submodule => self.tree.open_notice(
                entry.path,
                "Submodule — its contents live in another repository.".to_owned(),
            ),
            TreeEntryKind::File | TreeEntryKind::Symlink => {
                let epoch = self.tree.begin_file(entry.path.clone());
                self.request_tree_file(epoch, entry.path, cx);
            }
        }
        cx.notify();
    }

    fn request_tree_listing(&mut self, epoch: u64, cx: &mut Context<Self>) {
        let (Some(commit), Some(session)) = (self.tree.commit(), self.session.as_ref()) else {
            return;
        };
        let dir = self.tree.dir.clone();
        let repo = session.repo().clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move {
                    repo.tree_entries(commit, &dir)
                        .map(|mut entries| {
                            sort_entries(&mut entries);
                            let mut lfs = HashMap::new();
                            for entry in &entries {
                                if entry.kind == TreeEntryKind::File
                                    && let Ok(Some(info)) = repo.lfs_info_at(commit, &entry.path)
                                {
                                    lfs.insert(entry.path.clone(), info);
                                }
                            }
                            (entries, lfs)
                        })
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                let accepted = match read {
                    Ok((entries, lfs)) => this.tree.accept_listing(epoch, commit, entries, lfs),
                    Err(error) => this.tree.fail_listing(epoch, commit, error),
                };
                if accepted {
                    this.notify_if_active(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn request_tree_file(&mut self, epoch: u64, path: String, cx: &mut Context<Self>) {
        let (Some(commit), Some(session)) = (self.tree.commit(), self.session.as_ref()) else {
            return;
        };
        let repo = session.repo().clone();
        let highlighter = Arc::clone(&self.tree.highlighter);
        let tab_width = usize::from(Settings::get(cx).appearance.tab_width.clamp(1, 8));
        let read_path = path.clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move {
                    let mut bytes = repo
                        .blob_at(commit, &read_path)
                        .map_err(|error| error.to_string())?;
                    if let Some(pointer) = LfsPointer::parse(&bytes) {
                        match read_object(repo.common_dir(), &pointer, MAX_FILE_BYTES)
                            .map_err(|error| error.to_string())?
                        {
                            Some(real) => bytes = real,
                            None => {
                                return Ok::<FileContent, String>(FileContent::Notice(format!(
                                    "Git LFS object {} ({}) is not downloaded",
                                    short_label(&pointer.oid),
                                    format_size(pointer.size),
                                )));
                            }
                        }
                    }
                    Ok::<FileContent, String>(file_content(
                        &read_path,
                        bytes,
                        repo.working_tree_encoding(&read_path),
                        &highlighter,
                        tab_width,
                    ))
                })
                .await;
            this.update(cx, |this, cx| {
                let content = read.unwrap_or_else(FileContent::Notice);
                if this.tree.accept_file(epoch, commit, &path, content) {
                    this.notify_if_active(cx);
                }
            })
            .ok();
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

impl RepoView {
    /// The tree browser's pane, filling the area the diff would use.
    pub(super) fn tree_pane(&self, design: &Design, cx: &mut App) -> AnyElement {
        let key_entity = self.entity();
        let action_entity = self.entity();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(design.background)
            .track_focus(&self.view_focus())
            .key_context(TREE_VIEW)
            .on_action(move |_: &CloseTreeBrowser, window, cx| {
                let entity = action_entity.clone();
                entity.update(cx, |this, cx| this.close_tree(window, cx));
            })
            .on_key_down(move |event, window, cx| {
                let entity = key_entity.clone();
                match event.keystroke.key.as_str() {
                    "escape" => {
                        entity.update(cx, |this, cx| this.close_tree(window, cx));
                    }
                    "backspace" => {
                        entity.update(cx, |this, cx| this.tree_back(window, cx));
                    }
                    _ => {}
                }
            })
            .child(self.tree_path_bar(design))
            .child(self.tree_body(design, cx))
            .into_any_element()
    }

    fn tree_path_bar(&self, design: &Design) -> AnyElement {
        let entity = self.entity();
        let back = crate::ui::button("tree-back-to-changes", "Back to changes")
            .icon(IconName::ArrowLeft)
            .size(crate::ui::ButtonSize::Tiny)
            .ghost()
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.close_tree(window, cx));
            });

        let path = self.tree.path().to_owned();
        let blame = self.tree.commit().and_then(|commit| {
            self.tree.file.as_ref().map(|file| {
                let entity = self.entity();
                let path = file.path.clone();
                crate::ui::button("tree-blame", "Blame")
                    .icon(IconName::FileText)
                    .size(crate::ui::ButtonSize::Tiny)
                    .ghost()
                    .on_click(move |_, window, cx| {
                        let path = path.clone();
                        entity.update(cx, |this, cx| {
                            this.open_blame(commit, path, window, cx);
                        });
                    })
            })
        });
        let crumbs = breadcrumbs(&path);
        let count = crumbs.len();
        let mut trail = div()
            .flex()
            .items_center()
            .gap(px(3.0))
            .flex_1()
            .min_w_0()
            .overflow_hidden();
        for (index, crumb) in crumbs.into_iter().enumerate() {
            if index > 0 {
                trail = trail.child(icon(IconName::ChevronRight, 10.0, design.muted));
            }
            let last = index + 1 == count;
            let mut label = div()
                .id(("tree-crumb", index))
                .flex_none()
                .px_1()
                .rounded(px(4.0))
                .text_size(px(design.small()))
                .text_color(if last { design.text } else { design.muted })
                .child(crumb.label);
            if !last {
                let entity = self.entity();
                let target = crumb.path;
                label = label
                    .cursor_pointer()
                    .hover(|style| style.bg(design.field))
                    .on_click(move |_, _, cx| {
                        let target = target.clone();
                        entity.update(cx, |this, cx| this.open_tree_dir(target, cx));
                    });
            }
            trail = trail.child(label);
        }

        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(40.0))
            .flex_none()
            .px_2()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .child(back)
            .child(trail)
            .when_some(self.tree.non_utf8_encoding(), |bar, encoding| {
                bar.child(crate::ui::status_badge(
                    encoding.label(),
                    crate::ui::BadgeTone::Neutral,
                    design,
                ))
            })
            .when_some(blame, |bar, blame| bar.child(blame))
            .when_some(self.tree.commit(), |bar, commit| {
                bar.child(
                    div()
                        .flex_none()
                        .font_family(design.mono.clone())
                        .text_size(px(design.tiny()))
                        .text_color(design.muted)
                        .child(data::short_oid(commit)),
                )
            })
            .into_any_element()
    }

    fn tree_body(&self, design: &Design, cx: &mut App) -> AnyElement {
        if let Some(file) = &self.tree.file {
            return self.tree_file_view(file, design, cx);
        }
        if let Some(error) = &self.tree.error {
            return crate::ui::message(error.clone(), design.remove_text, design)
                .into_any_element();
        }
        let Some(entries) = &self.tree.entries else {
            return crate::ui::message("reading tree…", design.muted, design).into_any_element();
        };
        if entries.is_empty() {
            return crate::ui::message("Empty directory", design.muted, design).into_any_element();
        }

        let entries = Rc::new(entries.clone());
        let lfs = self.tree.lfs.clone();
        let count = entries.len();
        let entity = self.entity();
        let list_design = design.clone();
        uniform_list("tree-entries", count, move |range, _window, _cx| {
            range
                .map(|index| {
                    let entry = &entries[index];
                    let detail = entry_detail(entry);
                    let row = crate::ui::list_row(
                        ("tree-entry", index),
                        crate::ui::RowState::NORMAL,
                        &list_design,
                    )
                    .child(icon(entry_icon(entry.kind), 14.0, list_design.muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(entry.name.clone()),
                    )
                    .when(lfs.contains_key(&entry.path), |row| {
                        row.child(crate::ui::lfs_badge(&list_design))
                    })
                    .when(!detail.is_empty(), |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(list_design.muted)
                                .child(detail),
                        )
                    });
                    let click_entity = entity.clone();
                    row.cursor_pointer()
                        .on_click(move |_, _window, cx| {
                            click_entity.update(cx, |this, cx| this.tree_activate(index, cx));
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.tree.scroll)
        .with_scrollbar(&self.tree.scroll)
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn tree_file_view(&self, file: &OpenFile, design: &Design, cx: &mut App) -> AnyElement {
        match &file.content {
            FileContent::Loading => {
                crate::ui::message("reading file…", design.muted, design).into_any_element()
            }
            FileContent::Notice(notice) => {
                crate::ui::message(notice.clone(), design.muted, design).into_any_element()
            }
            FileContent::Text { lines, encoding: _ } => {
                let lines = Arc::clone(lines);
                let colors = DiffColors::from_theme(cx.theme());
                // The same font and size source as the diff list, so the tree
                // viewer and the diff pane never drift apart.
                let style = DiffStyle::from_settings(cx, &design.mono);
                let height = style.row_height();
                uniform_list("tree-file", lines.len(), move |range, _window, _cx| {
                    range
                        .map(|index| {
                            let line = &lines[index];
                            let runs = if style.shows_syntax() {
                                line.highlights
                                    .iter()
                                    .map(|(range, kind)| {
                                        (
                                            range.clone(),
                                            HighlightStyle {
                                                color: Some(colors.syntax(*kind)),
                                                ..HighlightStyle::default()
                                            },
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            } else {
                                Vec::new()
                            };
                            div()
                                .flex()
                                .items_center()
                                .h(px(height))
                                .w_full()
                                .flex_none()
                                .font_family(style.mono().clone())
                                .text_size(px(style.font_size()))
                                .text_color(colors.text)
                                .child(
                                    div()
                                        .flex_none()
                                        .w(px(GUTTER_WIDTH))
                                        .pr_2()
                                        .text_color(colors.gutter)
                                        .child((index + 1).to_string()),
                                )
                                .child(div().flex_none().whitespace_nowrap().child(
                                    StyledText::new(line.text.clone()).with_highlights(runs),
                                ))
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                })
                .track_scroll(&file.scroll)
                .with_scrollbar(&file.scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn entry(name: &str, kind: TreeEntryKind) -> TreeEntry {
        TreeEntry {
            name: name.to_owned(),
            path: name.to_owned(),
            kind,
            mode: 0o100644,
            size: Some(12),
        }
    }

    #[test]
    fn breadcrumbs_start_at_the_root_and_name_every_directory() {
        let crumbs = breadcrumbs("crates/strata-app/src");
        assert_eq!(
            crumbs.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["root", "crates", "strata-app", "src"]
        );
        assert_eq!(
            crumbs.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            ["", "crates", "crates/strata-app", "crates/strata-app/src"]
        );
    }

    #[test]
    fn breadcrumbs_ignore_empty_and_current_directory_components() {
        assert_eq!(breadcrumbs("").len(), 1);
        assert_eq!(breadcrumbs("src/").last().unwrap().path, "src");
        assert_eq!(breadcrumbs("/src/./ui").last().unwrap().path, "src/ui");
        assert_eq!(parent_dir("crates/strata-app"), "crates");
        assert_eq!(parent_dir("src"), "");
        assert_eq!(parent_dir(""), "");
    }

    #[test]
    fn directories_sort_before_files_and_names_stay_in_byte_order() {
        let mut entries = vec![
            entry("src", TreeEntryKind::Directory),
            entry("Cargo.toml", TreeEntryKind::File),
            entry("assets", TreeEntryKind::Directory),
            entry("README.md", TreeEntryKind::File),
        ];
        sort_entries(&mut entries);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["assets", "src", "Cargo.toml", "README.md"]
        );
    }

    #[test]
    fn entries_label_sizes_and_the_kinds_that_have_none() {
        let mut file = entry("main.rs", TreeEntryKind::File);
        file.size = Some(1536);
        assert_eq!(entry_detail(&file), "1.5 KB");
        file.size = Some(2 * 1024 * 1024);
        assert_eq!(entry_detail(&file), "2.0 MB");
        file.size = None;
        assert_eq!(entry_detail(&file), "");

        assert_eq!(entry_detail(&entry("src", TreeEntryKind::Directory)), "");
        assert_eq!(
            entry_detail(&entry("link", TreeEntryKind::Symlink)),
            "symlink"
        );
        assert_eq!(
            entry_detail(&entry("vendor", TreeEntryKind::Submodule)),
            "submodule"
        );
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn tabs_expand_and_still_carry_their_syntax_spans() {
        let spans = vec![(1..2, strata_core::highlight::HighlightKind::Keyword)];
        let (text, spans) = expand_tabs("\tif", 4, spans);
        assert_eq!(text, "    if");
        assert_eq!(
            spans,
            vec![(4..5, strata_core::highlight::HighlightKind::Keyword)]
        );
    }

    #[test]
    fn a_single_trailing_newline_does_not_add_an_empty_row() {
        assert_eq!(split_lines("one\ntwo\n"), ["one", "two"]);
        assert_eq!(split_lines("one\ntwo"), ["one", "two"]);
        assert_eq!(split_lines(""), [""]);
    }

    #[test]
    fn a_listing_for_an_old_directory_or_commit_is_ignored() {
        let commit = oid(1);
        let mut tree = TreeBrowser::default();
        tree.open(commit);
        let root_epoch = tree.listing_epoch;
        let src_epoch = tree.enter_dir("src".to_owned());

        assert!(!tree.accept_listing(root_epoch, commit, Vec::new(), HashMap::new()));
        assert!(
            tree.entries.is_none(),
            "the stale listing must not be stored"
        );
        assert!(!tree.accept_listing(src_epoch, oid(2), Vec::new(), HashMap::new()));
        assert!(tree.accept_listing(
            src_epoch,
            commit,
            vec![entry("main.rs", TreeEntryKind::File)],
            HashMap::new()
        ));
    }

    #[test]
    fn a_file_answer_is_dropped_after_leaving_it_or_the_tree() {
        let commit = oid(1);
        let mut tree = TreeBrowser::default();
        tree.open(commit);
        let epoch = tree.begin_file("src/main.rs".to_owned());

        assert!(!tree.accept_file(
            epoch,
            commit,
            "src/lib.rs",
            FileContent::Notice("wrong file".to_owned())
        ));
        // Opening another file supersedes the first request.
        tree.begin_file("src/other.rs".to_owned());
        assert!(!tree.accept_file(
            epoch,
            commit,
            "src/main.rs",
            FileContent::Notice("superseded".to_owned())
        ));

        let epoch = tree.begin_file("src/main.rs".to_owned());
        assert!(tree.leave_file());
        assert!(!tree.accept_file(
            epoch,
            commit,
            "src/main.rs",
            FileContent::Notice("too late".to_owned())
        ));

        tree.open(commit);
        let epoch = tree.begin_file("src/main.rs".to_owned());
        tree.close();
        assert!(!tree.accept_file(
            epoch,
            commit,
            "src/main.rs",
            FileContent::Notice("closed".to_owned())
        ));
    }

    #[test]
    fn binary_and_oversized_blobs_show_a_notice() {
        let highlighter = Mutex::new(Highlighter::new());
        assert!(matches!(
            file_content("logo.png", vec![0, 1, 2], None, &highlighter, 4),
            FileContent::Notice(_)
        ));
        assert!(matches!(
            file_content(
                "big.txt",
                vec![b'a'; MAX_FILE_BYTES + 1],
                None,
                &highlighter,
                4
            ),
            FileContent::Notice(_)
        ));
        let FileContent::Text { lines, .. } =
            file_content("a.txt", b"ok\n".to_vec(), None, &highlighter, 4)
        else {
            panic!("text file must load");
        };
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "ok");
    }
}
