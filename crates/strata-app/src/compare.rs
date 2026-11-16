//! Standalone Compare tool for arbitrary files and folders (2-way and 3-way),
//! independent of any git repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    ScrollHandle, ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled,
    Subscription, UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
};
use gpui_component::Selectable as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Icon, TitleBar, WindowExt as _};
use gpui_kit_assets::IconName;
use strata_core::Oid;
use strata_core::compare::{
    FolderCompareEntry, FolderCompareOptions, FolderCompareResult, FolderEntryKind,
    FolderEntryStatus, ThreeWayDiffKind, ThreeWayFileCompare, ThreeWayImageCompare, ThreeWaySide,
    ThreeWayTreeChange, ThreeWayTreeCompare, TwoWayFileCompare, compare_folders,
    compare_three_optional_files, compare_three_sides, compare_two_optional_files,
};
use strata_core::diff::{DiffOptions, GapAction};
use strata_core::image::ImageKind;
use strata_core::lfs::format_size;
use strata_core::session::RepoReader;
use strata_core::text::display_width_with_tabs;

use crate::data;
use crate::diff_view::{self, DiffColors, DiffContext, DiffHighlights, DiffStyle, Row as DiffRow};
use crate::image_diff;
use crate::scrollbar::WithScrollbar as _;
use crate::settings::Settings;
use crate::style::{self, Design};
use crate::ui::{self, BadgeTone, ButtonKind, ButtonSize, RowState};
use crate::workspace::Workspace;

/// Validated target for a standalone 2-way or 3-way comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompareTarget {
    TwoWayFiles {
        left: PathBuf,
        right: PathBuf,
    },
    ThreeWayFiles {
        left: PathBuf,
        middle: PathBuf,
        right: PathBuf,
    },
    TwoWayFolders {
        left: PathBuf,
        right: PathBuf,
    },
    ThreeWayFolders {
        left: PathBuf,
        middle: PathBuf,
        right: PathBuf,
    },
}

impl CompareTarget {
    /// Returns a concise title made from the compared file or folder names.
    pub(crate) fn title(&self) -> String {
        match self {
            CompareTarget::TwoWayFiles { left, right }
            | CompareTarget::TwoWayFolders { left, right } => {
                format!("{} ↔ {}", path_label(left), path_label(right))
            }
            CompareTarget::ThreeWayFiles {
                left,
                middle,
                right,
            }
            | CompareTarget::ThreeWayFolders {
                left,
                middle,
                right,
            } => format!(
                "{} ↔ {} ↔ {}",
                path_label(left),
                path_label(middle),
                path_label(right)
            ),
        }
    }

    pub(crate) fn paths(&self) -> Vec<PathBuf> {
        match self {
            CompareTarget::TwoWayFiles { left, right }
            | CompareTarget::TwoWayFolders { left, right } => vec![left.clone(), right.clone()],
            CompareTarget::ThreeWayFiles {
                left,
                middle,
                right,
            }
            | CompareTarget::ThreeWayFolders {
                left,
                middle,
                right,
            } => vec![left.clone(), middle.clone(), right.clone()],
        }
    }
}

fn path_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

// ---------------------------------------------------------------------------
// Repository revision comparison
// ---------------------------------------------------------------------------

/// One side of a repository revision comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompareRevision {
    pub oid: Oid,
    /// Branch name, or empty when the commit has none. The short id is always
    /// shown beside it.
    pub label: String,
}

impl CompareRevision {
    pub(crate) fn named(label: impl Into<String>, oid: Oid) -> Self {
        Self {
            oid,
            label: label.into(),
        }
    }

    /// `main (a1b2c3d)`, or just the short id when there is no branch name.
    pub(crate) fn display(&self) -> String {
        let short = data::short_oid(self.oid);
        if self.label.is_empty() || self.label == short {
            short
        } else {
            format!("{} ({})", self.label, short)
        }
    }
}

/// The three commits of a repository comparison and the reader they are read
/// through, so the work runs on a background executor.
#[derive(Clone)]
pub(crate) struct RepoThreeWaySource {
    pub reader: Arc<dyn RepoReader>,
    /// Repository root, for the tab's hint path.
    pub root: PathBuf,
    pub left: CompareRevision,
    pub base: CompareRevision,
    pub right: CompareRevision,
}

impl RepoThreeWaySource {
    fn title(&self) -> String {
        format!(
            "{} ↔ {} ↔ {}",
            self.left.display(),
            self.base.display(),
            self.right.display()
        )
    }

    /// What the pane headers call each side.
    fn pane_labels(&self) -> [String; 3] {
        [
            self.left.display(),
            self.base.display(),
            self.right.display(),
        ]
    }
}

/// What the compare view shows: arbitrary paths, or three repository commits.
enum CompareRoot {
    Paths(CompareTarget),
    Revisions(Box<RepoThreeWaySource>),
}

impl CompareRoot {
    fn title(&self) -> String {
        match self {
            Self::Paths(target) => target.title(),
            Self::Revisions(source) => source.title(),
        }
    }

    /// The path line under the title.
    fn subtitle(&self) -> String {
        match self {
            Self::Paths(target) => target
                .paths()
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("  ↔  "),
            Self::Revisions(source) => source.root.display().to_string(),
        }
    }

    fn hint_path(&self) -> Option<&Path> {
        match self {
            Self::Paths(target) => match target {
                CompareTarget::TwoWayFiles { left, .. }
                | CompareTarget::ThreeWayFiles { left, .. }
                | CompareTarget::TwoWayFolders { left, .. }
                | CompareTarget::ThreeWayFolders { left, .. } => Some(left.as_path()),
            },
            Self::Revisions(source) => Some(source.root.as_path()),
        }
    }

    fn revisions(&self) -> Option<&RepoThreeWaySource> {
        match self {
            Self::Paths(_) => None,
            Self::Revisions(source) => Some(source),
        }
    }
}

/// Classification and real-conflict flag for one row of a revision tree.
#[derive(Debug, Clone, Copy)]
struct RepoTreeMark {
    change: ThreeWayTreeChange,
    conflicting: bool,
}

/// Which side should become the base after a swap in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseChoice {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompareValidationError {
    MissingRequired,
    WrongArgCount(usize),
    PathNotFound(PathBuf),
    NotFileOrFolder(PathBuf),
    MixedTypes,
}

impl CompareValidationError {
    pub(crate) fn message(&self) -> Option<String> {
        match self {
            CompareValidationError::MissingRequired => None,
            CompareValidationError::WrongArgCount(count) => {
                Some(t!("compare.validation.wrong_count", count = *count).to_string())
            }
            CompareValidationError::PathNotFound(path) => Some(
                t!(
                    "compare.validation.not_found",
                    path = path.display().to_string()
                )
                .to_string(),
            ),
            CompareValidationError::NotFileOrFolder(path) => Some(
                t!(
                    "compare.validation.invalid_kind",
                    path = path.display().to_string()
                )
                .to_string(),
            ),
            CompareValidationError::MixedTypes => {
                Some(t!("compare.validation.mixed_types").to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    File,
    Folder,
}

fn classify_path(path: &Path) -> Result<PathKind, CompareValidationError> {
    let meta =
        fs::metadata(path).map_err(|_| CompareValidationError::PathNotFound(path.to_path_buf()))?;
    if meta.is_file() {
        Ok(PathKind::File)
    } else if meta.is_dir() {
        Ok(PathKind::Folder)
    } else {
        Err(CompareValidationError::NotFileOrFolder(path.to_path_buf()))
    }
}

/// Validates an ordered list of 2 (`[left, right]`) or 3 (`[left, middle, right]`)
/// filesystem paths (such as from `--compare A B [C]`).
pub(crate) fn validate_ordered_paths(
    paths: &[PathBuf],
) -> Result<CompareTarget, CompareValidationError> {
    if !(2..=3).contains(&paths.len()) {
        return Err(CompareValidationError::WrongArgCount(paths.len()));
    }
    let mut kinds = Vec::with_capacity(paths.len());
    for path in paths {
        kinds.push(classify_path(path)?);
    }
    let first_kind = kinds[0];
    if !kinds.iter().all(|kind| *kind == first_kind) {
        return Err(CompareValidationError::MixedTypes);
    }
    match (first_kind, paths.len()) {
        (PathKind::File, 2) => Ok(CompareTarget::TwoWayFiles {
            left: paths[0].clone(),
            right: paths[1].clone(),
        }),
        (PathKind::File, 3) => Ok(CompareTarget::ThreeWayFiles {
            left: paths[0].clone(),
            middle: paths[1].clone(),
            right: paths[2].clone(),
        }),
        (PathKind::Folder, 2) => Ok(CompareTarget::TwoWayFolders {
            left: paths[0].clone(),
            right: paths[1].clone(),
        }),
        (PathKind::Folder, 3) => Ok(CompareTarget::ThreeWayFolders {
            left: paths[0].clone(),
            middle: paths[1].clone(),
            right: paths[2].clone(),
        }),
        _ => unreachable!(),
    }
}

/// Validates the setup dialog's three text fields (`left`, `right`, and optional
/// `middle`). When `middle` is blank, performs a 2-way comparison (`left` vs
/// `right`); when filled, compares `left | middle | right`.
pub(crate) fn validate_dialog_inputs(
    left: &str,
    right: &str,
    middle: &str,
) -> Result<CompareTarget, CompareValidationError> {
    let left = left.trim();
    let right = right.trim();
    let middle = middle.trim();
    if left.is_empty() || right.is_empty() {
        return Err(CompareValidationError::MissingRequired);
    }
    let paths = if middle.is_empty() {
        vec![PathBuf::from(left), PathBuf::from(right)]
    } else {
        vec![
            PathBuf::from(left),
            PathBuf::from(middle),
            PathBuf::from(right),
        ]
    };
    validate_ordered_paths(&paths)
}

// ---------------------------------------------------------------------------
// Compare setup dialog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum DialogField {
    Left,
    Right,
    Middle,
}

/// The dialog's dismiss button. Cancel is never the action the dialog wants
/// the user to take, so it stays the ordinary kind even when Compare is
/// disabled and the primary button is not available.
fn dialog_cancel_button() -> ui::UiButton {
    ui::button("compare-dialog-cancel", t!("common.cancel").to_string())
        .kind(ButtonKind::Normal)
        .size(ButtonSize::Medium)
        .on_click(|_, window, cx| window.close_dialog(cx))
}

/// Opens the modal Compare setup dialog over the workspace window.
pub(crate) fn open_compare_dialog(workspace: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let dialog = cx.new(|cx| CompareDialog::new(workspace.downgrade(), window, cx));
    let focus = dialog.read(cx).left_focus(cx);
    let content = dialog.clone();
    let submit_dialog = dialog.clone();
    let ok_dialog = dialog;
    window.open_dialog(cx, move |card, _, cx| {
        let content = content.clone();
        let submit_dialog = submit_dialog.clone();
        let ok_dialog = ok_dialog.clone();
        let can_submit = content.read(cx).validated(cx).is_ok();
        let zoom = Settings::get(cx).appearance.zoom();
        card.title(t!("compare.dialog.title").to_string())
            .w(px(640.0 * zoom))
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                DialogFooter::new().child(dialog_cancel_button()).child(
                    ui::button(
                        "compare-dialog-submit",
                        t!("compare.dialog.compare").to_string(),
                    )
                    .size(ButtonSize::Medium)
                    .disabled(!can_submit)
                    .on_click(move |_, window, cx| {
                        submit_dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                    }),
                ),
            )
            .on_ok(move |_, window, cx| {
                ok_dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                false
            })
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

struct CompareDialog {
    workspace: WeakEntity<Workspace>,
    left: Entity<InputState>,
    right: Entity<InputState>,
    middle: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl CompareDialog {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let left = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("compare.dialog.left_placeholder").to_string())
        });
        let right = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("compare.dialog.right_placeholder").to_string())
        });
        let middle = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("compare.dialog.middle_placeholder").to_string())
        });

        let mut subscriptions = Vec::new();
        for input in [&left, &right, &middle] {
            let sub =
                cx.subscribe_in(
                    input,
                    window,
                    |this, _, event: &InputEvent, window, cx| match event {
                        InputEvent::Change => cx.notify(),
                        InputEvent::PressEnter { .. } => this.submit(window, cx),
                        _ => {}
                    },
                );
            subscriptions.push(sub);
        }

        Self {
            workspace,
            left,
            right,
            middle,
            _subscriptions: subscriptions,
        }
    }

    fn left_focus(&self, cx: &App) -> FocusHandle {
        self.left.focus_handle(cx)
    }

    fn validated(&self, cx: &App) -> Result<CompareTarget, CompareValidationError> {
        validate_dialog_inputs(
            self.left.read(cx).value().as_ref(),
            self.right.read(cx).value().as_ref(),
            self.middle.read(cx).value().as_ref(),
        )
    }

    fn browse(&mut self, field: DialogField, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = match field {
            DialogField::Left => t!("compare.dialog.browse_left").to_string(),
            DialogField::Right => t!("compare.dialog.browse_right").to_string(),
            DialogField::Middle => t!("compare.dialog.browse_middle").to_string(),
        };
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        let input = match field {
            DialogField::Left => self.left.clone(),
            DialogField::Right => self.right.clone(),
            DialogField::Middle => self.middle.clone(),
        };
        let entity = cx.weak_entity();
        window
            .spawn(cx, async move |cx| {
                let Ok(Ok(Some(paths))) = receiver.await else {
                    return;
                };
                let Some(chosen) = paths.into_iter().next() else {
                    return;
                };
                let text = chosen.display().to_string();
                let _ = cx.update(|window, cx| {
                    input.update(cx, |state, cx| state.set_value(text, window, cx));
                    let _ = entity.update(cx, |_, cx| cx.notify());
                });
            })
            .detach();
    }

    fn swap_left_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let a = self.left.read(cx).value().to_string();
        let b = self.right.read(cx).value().to_string();
        self.left
            .update(cx, |state, cx| state.set_value(b, window, cx));
        self.right
            .update(cx, |state, cx| state.set_value(a, window, cx));
        cx.notify();
    }

    fn swap_right_middle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let b = self.right.read(cx).value().to_string();
        let c = self.middle.read(cx).value().to_string();
        self.right
            .update(cx, |state, cx| state.set_value(c, window, cx));
        self.middle
            .update(cx, |state, cx| state.set_value(b, window, cx));
        cx.notify();
    }

    fn clear_middle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.middle
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(target) = self.validated(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.open_compare_target(target, window, cx);
        });
    }

    fn path_row(
        &self,
        id: &'static str,
        label: String,
        input: &Entity<InputState>,
        field: DialogField,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let clearable = matches!(field, DialogField::Middle);
        let has_value = !input.read(cx).value().trim().is_empty();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(label),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(input).w_full().bg(design.field)),
                    )
                    .when(clearable && has_value, |row| {
                        row.child(
                            ui::icon_button(format!("{id}-clear"), IconName::X)
                                .ghost()
                                .size(ButtonSize::Small)
                                .tooltip(t!("compare.dialog.clear_middle").to_string())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.clear_middle(window, cx);
                                })),
                        )
                    })
                    .child(
                        ui::button(
                            format!("{id}-browse"),
                            t!("compare.dialog.browse").to_string(),
                        )
                        .kind(ButtonKind::Normal)
                        .size(ButtonSize::Small)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.browse(field, window, cx);
                            },
                        )),
                    ),
            )
            .into_any_element()
    }
}

impl Render for CompareDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let validation = self.validated(cx);
        let error_message = validation.err().and_then(|err| err.message());

        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(self.path_row(
                "compare-left",
                t!("compare.dialog.left_label").to_string(),
                &self.left,
                DialogField::Left,
                &design,
                cx,
            ))
            .child(
                div().flex().items_center().justify_end().gap_1p5().child(
                    ui::button(
                        "compare-swap-lr",
                        t!("compare.dialog.swap_left_right").to_string(),
                    )
                    .ghost()
                    .size(ButtonSize::Tiny)
                    .icon(IconName::Replace)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.swap_left_right(window, cx);
                    })),
                ),
            )
            .child(self.path_row(
                "compare-right",
                t!("compare.dialog.right_label").to_string(),
                &self.right,
                DialogField::Right,
                &design,
                cx,
            ))
            .child(
                div().flex().items_center().justify_end().gap_1p5().child(
                    ui::button(
                        "compare-swap-rm",
                        t!("compare.dialog.swap_right_middle").to_string(),
                    )
                    .ghost()
                    .size(ButtonSize::Tiny)
                    .icon(IconName::Replace)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.swap_right_middle(window, cx);
                    })),
                ),
            )
            .child(self.path_row(
                "compare-middle",
                t!("compare.dialog.middle_label").to_string(),
                &self.middle,
                DialogField::Middle,
                &design,
                cx,
            ))
            .when_some(error_message, |col, msg| {
                col.child(ui::banner(ui::BannerKind::Danger, msg, &design))
            })
    }
}

// ---------------------------------------------------------------------------
// Compare workspace tab view
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FolderFilter {
    All,
    Different,
    OnlyLeft,
    OnlyMiddle,
    OnlyRight,
    Identical,
    TypeMismatch,
}

impl FolderFilter {
    fn matches(self, status: FolderEntryStatus) -> bool {
        match self {
            FolderFilter::All => true,
            FolderFilter::Different => status == FolderEntryStatus::Different,
            FolderFilter::OnlyLeft => status == FolderEntryStatus::OnlyLeft,
            FolderFilter::OnlyMiddle => status == FolderEntryStatus::OnlyMiddle,
            FolderFilter::OnlyRight => status == FolderEntryStatus::OnlyRight,
            FolderFilter::Identical => status == FolderEntryStatus::Identical,
            FolderFilter::TypeMismatch => status == FolderEntryStatus::TypeMismatch,
        }
    }
}

enum CompareState {
    Loading,
    Error(String),
    TwoWayFile(Box<TwoWayFileCompare>),
    ThreeWayFile(Box<ThreeWayFileCompare>),
    Folder(FolderCompareResult),
}

enum BackgroundOutput {
    TwoWayFile(Result<Option<TwoWayFileCompare>, String>),
    ThreeWayFile(Result<Option<ThreeWayFileCompare>, String>),
    Folder(Result<Option<FolderCompareResult>, String>),
    RepoTree(Result<Option<ThreeWayTreeCompare>, String>),
}

pub(crate) struct CompareView {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    active: bool,
    root: CompareRoot,
    respect_gitignore: bool,
    state: CompareState,
    /// Cached folder comparison result preserved while inspecting one file so
    /// clicking "Back" returns immediately without rescanning.
    folder_cache: Option<FolderCompareResult>,
    /// The revision tree the current folder list came from, kept so "Back"
    /// restores the change classification without re-reading the trees.
    repo_tree: Option<Box<ThreeWayTreeCompare>>,
    /// Classification per folder-list entry index while a revision tree is
    /// shown.
    repo_marks: Option<Rc<Vec<RepoTreeMark>>>,
    /// Relative path inside `folder_cache` when drilled into a file.
    drilled_relative_path: Option<String>,
    cancel_flag: Option<Arc<AtomicBool>>,
    generation: u64,
    // Two-way diff state
    diff_context: DiffContext,
    diff_rows: Rc<Vec<DiffRow>>,
    two_way_scroll: UniformListScrollHandle,
    image_state: image_diff::State,
    // Three-way diff state
    three_way_scroll: UniformListScrollHandle,
    /// One horizontal offset shared by all three text panes, so a wide line
    /// scrolls to the same column in every pane.
    three_way_h_scroll: ScrollHandle,
    /// Widest line of the current three-way comparison, in font columns.
    three_way_columns: usize,
    three_way_image_state: image_diff::State,
    three_way_svg_rendered: bool,
    active_change: Option<usize>,
    // Folder list state
    folder_scroll: UniformListScrollHandle,
    folder_filter: FolderFilter,
    filtered_indices: Vec<usize>,
    selected_folder_entry: Option<usize>,
}

impl Drop for CompareView {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl CompareView {
    pub(crate) fn new(
        target: CompareTarget,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_root(CompareRoot::Paths(target), workspace, cx)
    }

    /// A comparison of three repository commits. Reads go through the source's
    /// reader on the background executor, like a filesystem compare.
    pub(crate) fn new_revisions(
        source: RepoThreeWaySource,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_root(CompareRoot::Revisions(Box::new(source)), workspace, cx)
    }

    fn new_with_root(
        root: CompareRoot,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            active: true,
            root,
            respect_gitignore: true,
            state: CompareState::Loading,
            folder_cache: None,
            repo_tree: None,
            repo_marks: None,
            drilled_relative_path: None,
            cancel_flag: None,
            generation: 0,
            diff_context: DiffContext::default(),
            diff_rows: Rc::new(Vec::new()),
            two_way_scroll: UniformListScrollHandle::default(),
            image_state: image_diff::State::new(),
            three_way_scroll: UniformListScrollHandle::default(),
            three_way_h_scroll: ScrollHandle::default(),
            three_way_columns: 0,
            three_way_image_state: image_diff::State::new(),
            three_way_svg_rendered: false,
            active_change: None,
            folder_scroll: UniformListScrollHandle::default(),
            folder_filter: FolderFilter::All,
            filtered_indices: Vec::new(),
            selected_folder_entry: None,
        };
        view.start_root(cx);
        view
    }

    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn view_focus(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub(crate) fn tab_title(&self) -> String {
        match &self.drilled_relative_path {
            Some(rel) => format!("{} — {}", path_label(Path::new(rel)), self.root.title()),
            None => self.root.title(),
        }
    }

    pub(crate) fn window_title(&self) -> String {
        format!("{} — {}", t!("compare.tab.prefix"), self.tab_title())
    }

    pub(crate) fn hint_path(&self) -> Option<&Path> {
        self.root.hint_path()
    }

    /// The three pane labels while a repository comparison is shown.
    #[cfg(test)]
    pub(crate) fn pane_labels(&self) -> Option<Vec<String>> {
        self.root
            .revisions()
            .map(|source| source.pane_labels().to_vec())
    }

    pub(crate) fn is_loading(&self) -> bool {
        matches!(self.state, CompareState::Loading)
    }

    /// Horizontal offset of the three-way text panes, for tests that drive the
    /// shared scroll handle.
    #[cfg(test)]
    pub(crate) fn horizontal_scroll_offset(&self) -> f32 {
        f32::from(self.three_way_h_scroll.offset().x)
    }

    /// Vertical offset of the three-way row list. Tests use this to tell a
    /// horizontal pan apart from the rows moving.
    #[cfg(test)]
    pub(crate) fn vertical_scroll_offset(&self) -> f32 {
        f32::from(self.three_way_scroll.0.borrow().base_handle.offset().y)
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(flag) = self.cancel_flag.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }

    fn start_root(&mut self, cx: &mut Context<Self>) {
        let job = match &self.root {
            CompareRoot::Paths(target) => CompareJob::from_target(target, self.respect_gitignore),
            CompareRoot::Revisions(source) => CompareJob::RepoTree((**source).clone()),
        };
        self.start_job(job, cx);
    }

    fn start_job(&mut self, job: CompareJob, cx: &mut Context<Self>) {
        self.cancel();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(Arc::clone(&cancel));
        self.state = CompareState::Loading;
        self.diff_context.reset();
        self.diff_rows = Rc::new(Vec::new());
        self.active_change = None;
        self.three_way_svg_rendered = false;
        self.three_way_h_scroll = ScrollHandle::default();
        self.three_way_columns = 0;
        self.repo_marks = None;
        self.refresh_workspace_tab_strip(cx);
        cx.notify();

        let background = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let output = background.spawn(async move { job.run(&cancel) }).await;
            let _ = this.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                view.cancel_flag = None;
                view.apply_output(output, cx);
            });
        })
        .detach();
    }

    fn refresh_workspace_tab_strip(&self, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        // Defer so callers constructing CompareView inside a Workspace update do not double-lease Workspace.
        cx.defer(move |cx| {
            if let Some(ws) = workspace.upgrade() {
                ws.update(cx, |_, cx| cx.notify());
            }
        });
    }

    fn apply_output(&mut self, output: BackgroundOutput, cx: &mut Context<Self>) {
        match output {
            BackgroundOutput::TwoWayFile(Ok(Some(res))) => {
                let rows = self.diff_context.rows_parts(
                    &res.diff,
                    &res.left_text,
                    &res.right_text,
                    &res.gaps,
                );
                self.diff_rows = Rc::new(rows);
                self.state = CompareState::TwoWayFile(Box::new(res));
            }
            BackgroundOutput::ThreeWayFile(Ok(Some(res))) => {
                let tab_width = usize::from(Settings::get(cx).appearance.tab_width.max(1));
                self.three_way_columns = three_way_max_columns(
                    &res.left_lines,
                    &res.middle_lines,
                    &res.right_lines,
                    tab_width,
                );
                self.state = CompareState::ThreeWayFile(Box::new(res));
            }
            BackgroundOutput::Folder(Ok(Some(res))) => {
                self.folder_cache = Some(res.clone());
                self.state = CompareState::Folder(res);
                self.rebuild_folder_filter();
            }
            BackgroundOutput::RepoTree(Ok(Some(tree))) => {
                self.apply_repo_tree(tree);
            }
            BackgroundOutput::TwoWayFile(Ok(None))
            | BackgroundOutput::ThreeWayFile(Ok(None))
            | BackgroundOutput::Folder(Ok(None))
            | BackgroundOutput::RepoTree(Ok(None)) => {
                // Cancelled; keep existing or loading state superseded by the next job.
            }
            BackgroundOutput::TwoWayFile(Err(err))
            | BackgroundOutput::ThreeWayFile(Err(err))
            | BackgroundOutput::Folder(Err(err))
            | BackgroundOutput::RepoTree(Err(err)) => {
                self.state = CompareState::Error(err);
            }
        }
        self.refresh_workspace_tab_strip(cx);
        cx.notify();
    }

    fn apply_repo_tree(&mut self, tree: ThreeWayTreeCompare) {
        let Some(source) = self.root.revisions().cloned() else {
            return;
        };
        let folder = folder_result_from_tree(&tree, &source);
        self.repo_marks = Some(Rc::new(
            tree.entries
                .iter()
                .map(|entry| RepoTreeMark {
                    change: entry.change,
                    conflicting: entry.conflicting,
                })
                .collect(),
        ));
        self.repo_tree = Some(Box::new(tree));
        self.folder_cache = Some(folder.clone());
        self.state = CompareState::Folder(folder);
        self.rebuild_folder_filter();
    }

    fn rebuild_folder_filter(&mut self) {
        let CompareState::Folder(res) = &self.state else {
            self.filtered_indices.clear();
            return;
        };
        self.filtered_indices = res
            .entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| self.folder_filter.matches(entry.status).then_some(idx))
            .collect();
        self.selected_folder_entry = None;
    }

    pub(crate) fn open_folder_entry(
        &mut self,
        entry_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(folder) = self.folder_cache.as_ref() else {
            return;
        };
        let Some(entry) = folder.entries.get(entry_index) else {
            return;
        };
        if entry.kind != FolderEntryKind::File {
            return;
        }
        let rel = entry.relative_path.clone();
        self.selected_folder_entry = Some(entry_index);

        if let Some(source) = self.root.revisions().cloned() {
            self.drilled_relative_path = Some(rel.clone());
            self.start_job(CompareJob::RepoFile { source, path: rel }, cx);
            self.sync_window_title(window, cx);
            return;
        }

        let side_path = |side_idx: usize| -> Option<PathBuf> {
            let side = entry.sides.get(side_idx)?.as_ref()?;
            if side.kind != FolderEntryKind::File {
                return None;
            }
            Some(folder.roots.get(side_idx)?.join(&rel))
        };
        let job = if folder.roots.len() == 2 {
            CompareJob::TwoWayOptionalFiles {
                left: side_path(0),
                right: side_path(1),
            }
        } else {
            CompareJob::ThreeWayOptionalFiles {
                left: side_path(0),
                middle: side_path(1),
                right: side_path(2),
            }
        };
        self.drilled_relative_path = Some(rel);
        self.start_job(job, cx);
        self.sync_window_title(window, cx);
    }

    pub(crate) fn back_to_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cached) = self.folder_cache.clone() else {
            return;
        };
        self.cancel();
        self.generation = self.generation.wrapping_add(1);
        self.drilled_relative_path = None;
        self.diff_rows = Rc::new(Vec::new());
        self.active_change = None;
        self.state = CompareState::Folder(cached);
        if let Some(tree) = &self.repo_tree {
            self.repo_marks = Some(Rc::new(
                tree.entries
                    .iter()
                    .map(|entry| RepoTreeMark {
                        change: entry.change,
                        conflicting: entry.conflicting,
                    })
                    .collect(),
            ));
        }
        self.rebuild_folder_filter();
        self.refresh_workspace_tab_strip(cx);
        self.sync_window_title(window, cx);
        cx.notify();
    }

    fn toggle_gitignore(&mut self, cx: &mut Context<Self>) {
        self.respect_gitignore = !self.respect_gitignore;
        self.folder_cache = None;
        self.drilled_relative_path = None;
        self.start_root(cx);
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(rel) = self.drilled_relative_path.clone()
            && let Some(folder) = self.folder_cache.as_ref()
            && let Some(entry_idx) = folder
                .entries
                .iter()
                .position(|entry| entry.relative_path == rel)
        {
            self.open_folder_entry(entry_idx, window, cx);
            return;
        }
        self.start_root(cx);
    }

    /// Swaps the middle and the chosen side, so the comparison is re-read with
    /// the other commit as the base.
    fn use_side_as_base(&mut self, side: BaseChoice, window: &mut Window, cx: &mut Context<Self>) {
        {
            let CompareRoot::Revisions(source) = &mut self.root else {
                return;
            };
            let already = match side {
                BaseChoice::Left => source.base.oid == source.left.oid,
                BaseChoice::Right => source.base.oid == source.right.oid,
            };
            if already {
                return;
            }
            match side {
                BaseChoice::Left => std::mem::swap(&mut source.left, &mut source.base),
                BaseChoice::Right => std::mem::swap(&mut source.right, &mut source.base),
            }
        }
        self.folder_cache = None;
        self.repo_tree = None;
        self.repo_marks = None;
        self.drilled_relative_path = None;
        self.selected_folder_entry = None;
        self.start_root(cx);
        self.sync_window_title(window, cx);
    }

    /// Re-applies this tab's window title after drilling into a file or going
    /// back, through the workspace so an inactive tab never replaces it.
    ///
    /// Deferred: the workspace reads this view's title, which it cannot do
    /// while the drill-in update still holds it.
    fn sync_window_title(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.refresh_window_title(window, cx);
            });
        });
    }

    fn expand_two_way_gap(&mut self, gap: usize, action: GapAction, cx: &mut Context<Self>) {
        let CompareState::TwoWayFile(res) = &self.state else {
            return;
        };
        if self
            .diff_context
            .expand_parts(gap, action, &res.gaps, &res.left_text, &res.right_text)
        {
            let rows =
                self.diff_context
                    .rows_parts(&res.diff, &res.left_text, &res.right_text, &res.gaps);
            self.diff_rows = Rc::new(rows);
            self.active_change = None;
            cx.notify();
        } else if self.diff_context.notice().is_some() {
            cx.notify();
        }
    }

    fn toggle_two_way_whole_file(&mut self, cx: &mut Context<Self>) {
        let CompareState::TwoWayFile(res) = &self.state else {
            return;
        };
        if self
            .diff_context
            .toggle_whole_file_parts(&res.left_text, &res.right_text)
        {
            let rows =
                self.diff_context
                    .rows_parts(&res.diff, &res.left_text, &res.right_text, &res.gaps);
            self.diff_rows = Rc::new(rows);
            self.active_change = None;
            cx.notify();
        } else if self.diff_context.notice().is_some() {
            cx.notify();
        }
    }

    fn jump_two_way_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        let targets = diff_view::hunk_scroll_targets(&self.diff_rows, cx);
        if targets.is_empty() {
            return;
        }
        let next = step_index(self.active_change, targets.len(), forward);
        self.active_change = Some(next);
        self.two_way_scroll
            .scroll_to_item(targets[next], ScrollStrategy::Center);
        cx.notify();
    }

    fn jump_three_way_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        let CompareState::ThreeWayFile(res) = &self.state else {
            return;
        };
        let rows: Vec<usize> = res
            .change_regions()
            .filter_map(|region| (!region.row_range.is_empty()).then_some(region.row_range.start))
            .collect();
        if rows.is_empty() {
            return;
        }
        let next = step_index(self.active_change, rows.len(), forward);
        self.active_change = Some(next);
        self.three_way_scroll
            .scroll_to_item(rows[next], ScrollStrategy::Center);
        cx.notify();
    }

    fn header(&self, design: &Design, cx: &mut Context<Self>) -> AnyElement {
        let subtitle = self.root.subtitle();

        TitleBar::new()
            .h(design.px(style::TITLE_BAR_HEIGHT))
            .bg(design.chrome)
            .border_color(design.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(design.px(7.0))
                            .child(style::app_mark(design.scale(style::BRAND_MARK)))
                            .child(style::wordmark(
                                design.scale(style::BRAND_WORDMARK),
                                design.text,
                            )),
                    )
                    .child(div().w(px(1.0)).h(design.px(18.0)).mx_1().bg(design.border))
                    .when(self.drilled_relative_path.is_some(), |bar| {
                        bar.child(
                            ui::button(
                                "compare-back-folder",
                                t!("compare.back_to_folder").to_string(),
                            )
                            .ghost()
                            .size(ButtonSize::Small)
                            .icon(IconName::ArrowLeft)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.back_to_folder(window, cx);
                                },
                            )),
                        )
                    })
                    .child(
                        Icon::new(IconName::Replace)
                            .size(design.px(14.0))
                            .text_color(design.text),
                    )
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(px(design.ui))
                            .child(self.tab_title()),
                    )
                    .when_some(self.root.revisions(), |bar, source| {
                        let base_is_left = source.base.oid == source.left.oid;
                        let base_is_right = source.base.oid == source.right.oid;
                        bar.child(
                            ui::button(
                                "compare-use-left-base",
                                t!("compare.three_way.use_left_as_base").to_string(),
                            )
                            .ghost()
                            .size(ButtonSize::Tiny)
                            .icon(IconName::Replace)
                            .disabled(base_is_left)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.use_side_as_base(BaseChoice::Left, window, cx);
                                },
                            )),
                        )
                        .child(
                            ui::button(
                                "compare-use-right-base",
                                t!("compare.three_way.use_right_as_base").to_string(),
                            )
                            .ghost()
                            .size(ButtonSize::Tiny)
                            .icon(IconName::Replace)
                            .disabled(base_is_right)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.use_side_as_base(BaseChoice::Right, window, cx);
                                },
                            )),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .text_size(design.px(11.0))
                    .text_color(design.muted)
                    .truncate()
                    .child(subtitle),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .flex_none()
                    .occlude()
                    .child(
                        ui::icon_button("compare-refresh", IconName::RefreshCw)
                            .ghost()
                            .size(ButtonSize::Small)
                            .tooltip(t!("compare.refresh").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    )
                    .child(
                        ui::button("compare-new", t!("compare.new_compare").to_string())
                            .ghost()
                            .size(ButtonSize::Small)
                            .icon(IconName::Replace)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(ws) = this.workspace.upgrade() {
                                    ws.update(cx, |ws, cx| ws.open_compare_dialog(window, cx));
                                }
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_two_way(
        &self,
        res: &TwoWayFileCompare,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let encoding = style::encoding_label_for(res.left_encoding, res.right_encoding);
        let notify = image_diff::notify_entity(cx.entity());
        let svg_toggle =
            image_diff::svg_toggle_for(&self.image_state, res.image.as_ref(), notify.clone());
        let hunk_count = res.diff.hunks.len();

        let subheader = div()
            .flex()
            .items_center()
            .gap_2()
            .h(design.px(38.0))
            .flex_none()
            .px_3p5()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .text_size(design.px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(design.text)
                    .child(format!(
                        "{}  ↔  {}",
                        display_or_missing(&res.left_path),
                        display_or_missing(&res.right_path)
                    )),
            )
            .when_some(encoding, |bar, label| {
                bar.child(ui::status_badge(label, BadgeTone::Neutral, design))
            })
            .when_some(svg_toggle, |bar, btn| bar.child(btn))
            .when(!res.diff.binary && !res.too_large, |bar| {
                let whole = self.diff_context.is_whole_file();
                bar.child(
                    div()
                        .flex_none()
                        .text_size(design.px(11.0))
                        .text_color(design.muted)
                        .child(t!("compare.hunks_count", count = hunk_count).to_string()),
                )
                .when(hunk_count > 0, |bar| {
                    bar.child(
                        ui::icon_button("compare-prev-hunk", IconName::ChevronUp)
                            .ghost()
                            .size(ButtonSize::Tiny)
                            .tooltip(t!("compare.prev_change").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.jump_two_way_change(false, cx);
                            })),
                    )
                    .child(
                        ui::icon_button("compare-next-hunk", IconName::ChevronDown)
                            .ghost()
                            .size(ButtonSize::Tiny)
                            .tooltip(t!("compare.next_change").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.jump_two_way_change(true, cx);
                            })),
                    )
                })
                .child(
                    ui::button("compare-whole-file", t!("history.whole_file").to_string())
                        .size(ButtonSize::Tiny)
                        .ghost()
                        .selected(whole)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_two_way_whole_file(cx);
                        })),
                )
            });

        let body: AnyElement = if res.too_large {
            ui::empty_state(t!("compare.file_too_large").to_string(), design).into_any_element()
        } else if let Some(image) = res.image.as_ref()
            && let Some(image_body) =
                image_diff::body_for(&self.image_state, image, None, design, notify, cx)
        {
            image_body
        } else if res.diff.binary {
            ui::empty_state(t!("compare.binary_files").to_string(), design).into_any_element()
        } else if self.diff_rows.is_empty() {
            ui::empty_state(t!("compare.files_identical").to_string(), design).into_any_element()
        } else {
            let entity = cx.weak_entity();
            let on_gap: diff_view::GapHandler = Rc::new(move |gap, action, _window, cx| {
                let _ = entity.update(cx, |this, cx| this.expand_two_way_gap(gap, action, cx));
            });
            let colors = DiffColors::from_design(design);
            let highlights = DiffHighlights {
                old: Arc::clone(&res.left_highlights),
                new: Arc::clone(&res.right_highlights),
            };
            diff_view::diff_list_with_highlights(
                "compare-two-way-diff",
                Rc::clone(&self.diff_rows),
                Some(highlights),
                colors,
                design.mono.clone(),
                &self.two_way_scroll,
                Some(on_gap),
                cx,
            )
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(subheader)
            .when_some(self.diff_context.notice(), |col, notice| {
                col.child(ui::banner(ui::BannerKind::Info, notice.to_owned(), design))
            })
            .child(body)
            .into_any_element()
    }

    fn render_three_way(
        &self,
        res: &ThreeWayFileCompare,
        design: &Design,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let counts = res.counts();
        let total_changes = counts.total_changes();
        let style = DiffStyle::from_settings(cx, &design.mono);
        let diff_colors = DiffColors::from_design(design);
        // Every pane reserves the width of the widest line in the comparison,
        // not just its own, so one shared scroll offset can never be clamped
        // by a pane with shorter lines.
        let content_width =
            crate::style::monospace_advance(window, style.mono(), px(style.font_size()))
                * self.three_way_columns as f32
                + style.px(24.0);

        let encoding_badge = [res.left_encoding, res.middle_encoding, res.right_encoding]
            .into_iter()
            .find(|e| !e.is_utf8())
            .map(|e| e.label());

        let is_svg = res
            .images
            .as_ref()
            .is_some_and(|img| img.kind == ImageKind::Svg);

        let subheader = div()
            .flex()
            .items_center()
            .gap_2()
            .h(design.px(38.0))
            .flex_none()
            .px_3p5()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .text_size(design.px(12.0))
            .child(ui::status_badge(
                t!("compare.three_way.left_count", count = counts.left_changed).to_string(),
                BadgeTone::Accent,
                design,
            ))
            .child(ui::status_badge(
                t!(
                    "compare.three_way.right_count",
                    count = counts.right_changed
                )
                .to_string(),
                BadgeTone::Accent,
                design,
            ))
            .child(ui::status_badge(
                t!("compare.three_way.agree_count", count = counts.both_agree).to_string(),
                BadgeTone::Neutral,
                design,
            ))
            .child(ui::status_badge(
                t!(
                    "compare.three_way.conflict_count",
                    count = counts.both_disagree
                )
                .to_string(),
                if counts.both_disagree > 0 {
                    BadgeTone::Danger
                } else {
                    BadgeTone::Neutral
                },
                design,
            ))
            .when_some(encoding_badge, |bar, label| {
                bar.child(ui::status_badge(label, BadgeTone::Neutral, design))
            })
            .child(div().flex_1())
            .when(is_svg, |bar| {
                bar.child(
                    ui::button("compare-3way-svg", t!("image_diff.rendered").to_string())
                        .size(ButtonSize::Tiny)
                        .ghost()
                        .selected(self.three_way_svg_rendered)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.three_way_svg_rendered = !this.three_way_svg_rendered;
                            cx.notify();
                        })),
                )
            })
            .when(!res.binary && !res.too_large && total_changes > 0, |bar| {
                let current_label = self
                    .active_change
                    .map(|idx| format!("{} / {}", idx + 1, total_changes))
                    .unwrap_or_else(|| {
                        t!("compare.changes_count", count = total_changes).to_string()
                    });
                bar.child(
                    div()
                        .text_size(design.px(11.0))
                        .text_color(design.muted)
                        .child(current_label),
                )
                .child(
                    ui::icon_button("compare-3way-prev", IconName::ChevronUp)
                        .ghost()
                        .size(ButtonSize::Tiny)
                        .tooltip(t!("compare.prev_change").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.jump_three_way_change(false, cx);
                        })),
                )
                .child(
                    ui::icon_button("compare-3way-next", IconName::ChevronDown)
                        .ghost()
                        .size(ButtonSize::Tiny)
                        .tooltip(t!("compare.next_change").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.jump_three_way_change(true, cx);
                        })),
                )
            });

        let pane_headers = div()
            .flex()
            .w_full()
            .h(design.px(28.0))
            .flex_none()
            .bg(design.panel)
            .border_b_1()
            .border_color(design.border)
            .child(three_way_pane_header(
                t!("compare.side.left").to_string(),
                &res.left_name,
                design,
                true,
            ))
            .child(three_way_pane_header(
                t!("compare.side.middle_base").to_string(),
                &res.middle_name,
                design,
                true,
            ))
            .child(three_way_pane_header(
                t!("compare.side.right").to_string(),
                &res.right_name,
                design,
                false,
            ));

        let show_images = res
            .images
            .as_ref()
            .is_some_and(|images| images.kind == ImageKind::Raster || self.three_way_svg_rendered);

        let body: AnyElement = if res.too_large {
            ui::empty_state(t!("compare.file_too_large").to_string(), design).into_any_element()
        } else if show_images && let Some(images) = res.images.as_ref() {
            render_three_way_images(&self.three_way_image_state, images, design)
        } else if res.binary {
            ui::empty_state(t!("compare.binary_files").to_string(), design).into_any_element()
        } else if res.rows.is_empty() {
            ui::empty_state(t!("compare.files_identical").to_string(), design).into_any_element()
        } else {
            let rows = Arc::new(res.rows.clone());
            let left_lines = Arc::clone(&res.left_lines);
            let middle_lines = Arc::clone(&res.middle_lines);
            let right_lines = Arc::clone(&res.right_lines);
            let left_hl = Arc::clone(&res.left_highlights);
            let middle_hl = Arc::clone(&res.middle_highlights);
            let right_hl = Arc::clone(&res.right_highlights);
            let design_copy = design.clone();
            let h_scroll = self.three_way_h_scroll.clone();
            let count = rows.len();

            // A scroller with only vertical overflow copies a pure horizontal
            // wheel delta onto Y. The panes already consume that delta, and
            // they do not stop it, so the rows would walk unless this list
            // refuses the other axis. The helper that sets this lives on
            // stateful elements; a uniform list is not one, but the flag is
            // the same style field.
            let mut list = uniform_list(
                "compare-three-way-list",
                count,
                move |range: std::ops::Range<usize>, _window, _cx| {
                    range
                        .map(|idx| {
                            let row = rows[idx];
                            render_three_way_row(
                                row,
                                &left_lines,
                                &middle_lines,
                                &right_lines,
                                &left_hl,
                                &middle_hl,
                                &right_hl,
                                &diff_colors,
                                &style,
                                &design_copy,
                                content_width,
                                &h_scroll,
                            )
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.three_way_scroll);
            list.style().restrict_scroll_to_axis = Some(true);
            list.with_scrollbar(&self.three_way_scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(subheader)
            .child(pane_headers)
            .child(body)
            .into_any_element()
    }

    fn render_folder(
        &self,
        res: &FolderCompareResult,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_three_way = res.roots.len() == 3;
        let revisions = self.root.revisions().is_some();
        let counts = &res.counts;
        let mut filter_bar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1p5()
            .px_3p5()
            .py_2()
            .flex_none()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border)
            .child(self.filter_chip(
                "filter-all",
                FolderFilter::All,
                t!("compare.folder.filter_all").to_string(),
                counts.total,
                design,
                cx,
            ))
            .child(self.filter_chip(
                "filter-diff",
                FolderFilter::Different,
                t!("compare.folder.filter_different").to_string(),
                counts.different,
                design,
                cx,
            ))
            .child(self.filter_chip(
                "filter-left",
                FolderFilter::OnlyLeft,
                t!("compare.folder.filter_only_left").to_string(),
                counts.only_left,
                design,
                cx,
            ));
        if is_three_way {
            filter_bar = filter_bar.child(self.filter_chip(
                "filter-middle",
                FolderFilter::OnlyMiddle,
                t!("compare.folder.filter_only_middle").to_string(),
                counts.only_middle,
                design,
                cx,
            ));
        }
        filter_bar = filter_bar
            .child(self.filter_chip(
                "filter-right",
                FolderFilter::OnlyRight,
                t!("compare.folder.filter_only_right").to_string(),
                counts.only_right,
                design,
                cx,
            ))
            .child(self.filter_chip(
                "filter-identical",
                FolderFilter::Identical,
                t!("compare.folder.filter_identical").to_string(),
                counts.identical,
                design,
                cx,
            ))
            .when(counts.type_mismatch > 0, |bar| {
                bar.child(self.filter_chip(
                    "filter-mismatch",
                    FolderFilter::TypeMismatch,
                    t!("compare.folder.filter_mismatch").to_string(),
                    counts.type_mismatch,
                    design,
                    cx,
                ))
            })
            .child(div().flex_1())
            .when(!revisions, |bar| {
                bar.child(
                    ui::checkbox("compare-respect-gitignore", self.respect_gitignore)
                        .label(t!("compare.folder.respect_gitignore").to_string())
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_gitignore(cx))),
                )
            });

        let list: AnyElement = if self.filtered_indices.is_empty() {
            ui::empty_state(t!("compare.folder.empty").to_string(), design).into_any_element()
        } else {
            let entries = Arc::new(res.entries.clone());
            let indices = Arc::new(self.filtered_indices.clone());
            let marks = self.repo_marks.clone();
            let selected = self.selected_folder_entry;
            let design_copy = design.clone();
            let entity = cx.weak_entity();
            let count = indices.len();

            uniform_list(
                "compare-folder-entries",
                count,
                move |range: std::ops::Range<usize>, _window, _cx| {
                    range
                        .map(|list_idx| {
                            let entry_idx = indices[list_idx];
                            let entry = &entries[entry_idx];
                            let is_selected = selected == Some(entry_idx);
                            let mark = marks
                                .as_ref()
                                .and_then(|marks| marks.get(entry_idx))
                                .copied();
                            render_folder_row(
                                list_idx,
                                entry_idx,
                                entry,
                                is_three_way,
                                revisions,
                                mark,
                                is_selected,
                                &design_copy,
                                entity.clone(),
                            )
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.folder_scroll)
            .with_scrollbar(&self.folder_scroll)
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(filter_bar)
            .child(list)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn filter_chip(
        &self,
        id: &'static str,
        filter: FolderFilter,
        label: String,
        count: usize,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.folder_filter == filter;
        ui::button(id, format!("{label} ({count})"))
            .size(ButtonSize::Tiny)
            .ghost()
            .selected(active)
            .text_color(if active { design.text } else { design.muted })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.folder_filter = filter;
                this.rebuild_folder_filter();
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Focusable for CompareView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CompareView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let title = self.tab_title();
        let loading = self.is_loading();

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(design.background)
            .text_color(design.text)
            .track_focus(&self.focus_handle)
            .child(self.header(&design, cx));
        root.text_style().font_fallbacks = Some(style::font_fallbacks());

        if let Some(workspace) = self.workspace.upgrade()
            && let Some(strip) =
                Workspace::render_tab_strip(&workspace, Some(&title), loading, &design, cx)
        {
            root = root.child(strip);
        }

        let body: AnyElement = match &self.state {
            CompareState::Loading => {
                ui::loading_state(t!("compare.loading").to_string(), &design).into_any_element()
            }
            CompareState::Error(error) => div()
                .flex()
                .flex_col()
                .flex_1()
                .p_4()
                .child(ui::banner(ui::BannerKind::Danger, error.clone(), &design))
                .into_any_element(),
            CompareState::TwoWayFile(res) => self.render_two_way(res, &design, cx),
            CompareState::ThreeWayFile(res) => self.render_three_way(res, &design, window, cx),
            CompareState::Folder(res) => self.render_folder(res, &design, cx),
        };

        root.child(body)
    }
}

enum CompareJob {
    TwoWayFiles {
        left: PathBuf,
        right: PathBuf,
    },
    TwoWayOptionalFiles {
        left: Option<PathBuf>,
        right: Option<PathBuf>,
    },
    ThreeWayFiles {
        left: PathBuf,
        middle: PathBuf,
        right: PathBuf,
    },
    ThreeWayOptionalFiles {
        left: Option<PathBuf>,
        middle: Option<PathBuf>,
        right: Option<PathBuf>,
    },
    Folders {
        roots: Vec<PathBuf>,
        options: FolderCompareOptions,
    },
    /// The tree-level list of a repository commit comparison.
    RepoTree(RepoThreeWaySource),
    /// One file of a repository commit comparison, read from the blobs at
    /// `path` in each commit.
    RepoFile {
        source: RepoThreeWaySource,
        path: String,
    },
}

impl CompareJob {
    fn from_target(target: &CompareTarget, respect_gitignore: bool) -> Self {
        match target {
            CompareTarget::TwoWayFiles { left, right } => Self::TwoWayFiles {
                left: left.clone(),
                right: right.clone(),
            },
            CompareTarget::ThreeWayFiles {
                left,
                middle,
                right,
            } => Self::ThreeWayFiles {
                left: left.clone(),
                middle: middle.clone(),
                right: right.clone(),
            },
            CompareTarget::TwoWayFolders { left, right } => Self::Folders {
                roots: vec![left.clone(), right.clone()],
                options: FolderCompareOptions { respect_gitignore },
            },
            CompareTarget::ThreeWayFolders {
                left,
                middle,
                right,
            } => Self::Folders {
                roots: vec![left.clone(), middle.clone(), right.clone()],
                options: FolderCompareOptions { respect_gitignore },
            },
        }
    }

    fn run(self, cancel: &AtomicBool) -> BackgroundOutput {
        let diff_opts = DiffOptions::default();
        match self {
            Self::TwoWayFiles { left, right } => BackgroundOutput::TwoWayFile(
                compare_two_optional_files(Some(&left), Some(&right), &diff_opts, cancel)
                    .map_err(|e| e.to_string()),
            ),
            Self::TwoWayOptionalFiles { left, right } => BackgroundOutput::TwoWayFile(
                compare_two_optional_files(left.as_deref(), right.as_deref(), &diff_opts, cancel)
                    .map_err(|e| e.to_string()),
            ),
            Self::ThreeWayFiles {
                left,
                middle,
                right,
            } => BackgroundOutput::ThreeWayFile(
                compare_three_optional_files(Some(&left), Some(&middle), Some(&right), cancel)
                    .map_err(|e| e.to_string()),
            ),
            Self::ThreeWayOptionalFiles {
                left,
                middle,
                right,
            } => BackgroundOutput::ThreeWayFile(
                compare_three_optional_files(
                    left.as_deref(),
                    middle.as_deref(),
                    right.as_deref(),
                    cancel,
                )
                .map_err(|e| e.to_string()),
            ),
            Self::Folders { roots, options } => {
                let refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
                BackgroundOutput::Folder(
                    compare_folders(&refs, options, cancel).map_err(|e| e.to_string()),
                )
            }
            Self::RepoTree(source) => BackgroundOutput::RepoTree(
                source
                    .reader
                    .three_way_tree(source.left.oid, source.base.oid, source.right.oid)
                    .map(Some)
                    .map_err(|error| error.to_string()),
            ),
            Self::RepoFile { source, path } => {
                let blobs = source.reader.three_way_blobs(
                    source.left.oid,
                    source.base.oid,
                    source.right.oid,
                    &path,
                );
                match blobs {
                    Ok([left, base, right]) => {
                        let [left_name, base_name, right_name] = source.pane_labels();
                        let hint = Some(path);
                        BackgroundOutput::ThreeWayFile(Ok(compare_three_sides(
                            ThreeWaySide::new(left_name, hint.clone(), left),
                            ThreeWaySide::new(base_name, hint.clone(), base),
                            ThreeWaySide::new(right_name, hint, right),
                            cancel,
                        )))
                    }
                    Err(error) => BackgroundOutput::ThreeWayFile(Err(error.to_string())),
                }
            }
        }
    }
}

/// Presents a revision tree with the folder list's types, so the same filter
/// chips, rows and drill-in work for both.
fn folder_result_from_tree(
    tree: &ThreeWayTreeCompare,
    source: &RepoThreeWaySource,
) -> FolderCompareResult {
    FolderCompareResult {
        roots: source
            .pane_labels()
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        entries: tree
            .entries
            .iter()
            .map(|entry| FolderCompareEntry {
                relative_path: entry.relative_path.clone(),
                depth: 0,
                kind: entry.kind,
                status: entry.status,
                sides: entry.sides.clone(),
            })
            .collect(),
        counts: tree.counts.clone(),
    }
}

fn display_or_missing(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        t!("compare.side.missing").to_string()
    } else {
        path_label(path)
    }
}

fn step_index(current: Option<usize>, total: usize, forward: bool) -> usize {
    match current {
        None => {
            if forward {
                0
            } else {
                total.saturating_sub(1)
            }
        }
        Some(idx) => {
            if forward {
                (idx + 1) % total
            } else {
                idx.checked_sub(1).unwrap_or(total - 1)
            }
        }
    }
}

fn display_name_or_missing(name: &str) -> String {
    if name.is_empty() {
        t!("compare.side.missing").to_string()
    } else {
        name.to_owned()
    }
}

fn three_way_pane_header(
    role: String,
    name: &str,
    design: &Design,
    border_right: bool,
) -> AnyElement {
    let tooltip_text: SharedString = name.to_owned().into();
    div()
        .id(SharedString::from(format!("compare-3way-hdr-{role}")))
        .flex()
        .items_center()
        .gap_1p5()
        .flex_1()
        .min_w_0()
        .h_full()
        .px_2p5()
        .text_size(design.px(11.0))
        .when(border_right, |cell| {
            cell.border_r_1().border_color(design.border)
        })
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(design.text)
                .child(role),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(design.muted)
                .child(display_name_or_missing(name)),
        )
        .when(!name.is_empty(), |cell| {
            cell.tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        })
        .into_any_element()
}

fn render_three_way_images(
    state: &image_diff::State,
    images: &ThreeWayImageCompare,
    design: &Design,
) -> AnyElement {
    let side_view = |side: Option<&strata_core::image::ImageSide>, label: String| -> AnyElement {
        if let Some(side) = side {
            image_diff::labelled_picture(state, side, label, design.text, design, false)
        } else {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .child(ui::empty_state(
                    t!("compare.side.missing").to_string(),
                    design,
                ))
                .into_any_element()
        }
    };

    div()
        .flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(side_view(
            images.left.as_ref(),
            t!("compare.side.left").to_string(),
        ))
        .child(div().flex_none().w(px(1.0)).h_full().bg(design.border))
        .child(side_view(
            images.middle.as_ref(),
            t!("compare.side.middle_base").to_string(),
        ))
        .child(div().flex_none().w(px(1.0)).h_full().bg(design.border))
        .child(side_view(
            images.right.as_ref(),
            t!("compare.side.right").to_string(),
        ))
        .into_any_element()
}

/// Wide enough for the widest line of any of the three sides, in columns.
fn three_way_max_columns(
    left: &[String],
    middle: &[String],
    right: &[String],
    tab_width: usize,
) -> usize {
    left.iter()
        .chain(middle.iter())
        .chain(right.iter())
        .map(|line| display_width_with_tabs(line, tab_width))
        .max()
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn render_three_way_row(
    row: strata_core::compare::ThreeWayRow,
    left_lines: &[String],
    middle_lines: &[String],
    right_lines: &[String],
    left_hl: &[strata_core::highlight::LineHighlights],
    middle_hl: &[strata_core::highlight::LineHighlights],
    right_hl: &[strata_core::highlight::LineHighlights],
    diff: &DiffColors,
    style: &DiffStyle,
    design: &Design,
    content_width: gpui::Pixels,
    h_scroll: &ScrollHandle,
) -> AnyElement {
    let (left_tint, middle_tint, right_tint) = three_way_tints(row.kind, design);
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(style.row_height()))
        .font_family(style.mono().clone())
        .text_size(px(style.font_size()))
        .text_color(diff.text)
        .child(three_way_cell(
            row.left,
            left_lines,
            left_hl,
            left_tint,
            diff,
            style,
            design,
            0,
            content_width,
            h_scroll,
        ))
        .child(three_way_cell(
            row.middle,
            middle_lines,
            middle_hl,
            middle_tint,
            diff,
            style,
            design,
            1,
            content_width,
            h_scroll,
        ))
        .child(three_way_cell(
            row.right,
            right_lines,
            right_hl,
            right_tint,
            diff,
            style,
            design,
            2,
            content_width,
            h_scroll,
        ))
        .into_any_element()
}

fn three_way_tints(
    kind: ThreeWayDiffKind,
    design: &Design,
) -> (Option<Hsla>, Option<Hsla>, Option<Hsla>) {
    match kind {
        ThreeWayDiffKind::Unchanged => (None, None, None),
        ThreeWayDiffKind::LeftChanged => (Some(design.diff_added_bg), None, None),
        ThreeWayDiffKind::RightChanged => (None, None, Some(design.diff_added_bg)),
        ThreeWayDiffKind::BothAgree => (Some(design.accent_soft), None, Some(design.accent_soft)),
        ThreeWayDiffKind::BothDisagree => (
            Some(design.diff_removed_bg),
            Some(design.diff_hunk_header),
            Some(design.diff_removed_bg),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn three_way_cell(
    line: Option<u32>,
    lines: &[String],
    highlights: &[strata_core::highlight::LineHighlights],
    tint: Option<Hsla>,
    diff: &DiffColors,
    style: &DiffStyle,
    design: &Design,
    side: usize,
    content_width: gpui::Pixels,
    h_scroll: &ScrollHandle,
) -> AnyElement {
    let mut cell = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .when_some(tint, |cell, tint| cell.bg(tint))
        .when(side < 2, |cell| {
            cell.border_r_1().border_color(design.border)
        });

    if let Some(number) = line {
        let text = lines
            .get(number as usize - 1)
            .map(String::as_str)
            .unwrap_or_default();
        let spans = highlights.get(number as usize - 1);
        cell = cell
            .child(
                div()
                    .flex_none()
                    .w(style.px(38.0))
                    .pr_1()
                    .text_size(px(style.font_size() - style.scale(2.0)))
                    .text_color(diff.gutter)
                    .text_right()
                    .child(number.to_string()),
            )
            .child(
                // The line numbers stay put; only the code pans, so the three
                // panes can share this handle and keep their columns aligned.
                div()
                    .id(("compare-3way-pane", number as usize * 3 + side))
                    .debug_selector({
                        let name = match side {
                            0 => "compare-3way-left",
                            1 => "compare-3way-middle",
                            _ => "compare-3way-right",
                        };
                        move || name.to_string()
                    })
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .items_center()
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(h_scroll)
                    .child(
                        div()
                            .flex_none()
                            .w(content_width)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(diff_view::styled_line(text, style, spans, diff)),
                    ),
            );
    }
    cell.into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_folder_row(
    list_idx: usize,
    entry_idx: usize,
    entry: &FolderCompareEntry,
    is_three_way: bool,
    revisions: bool,
    mark: Option<RepoTreeMark>,
    selected: bool,
    design: &Design,
    entity: WeakEntity<CompareView>,
) -> AnyElement {
    let icon_name = match entry.kind {
        FolderEntryKind::Directory => IconName::Folder,
        FolderEntryKind::Symlink => IconName::Unlink,
        FolderEntryKind::File | FolderEntryKind::Other => IconName::FileText,
    };
    let name = if revisions {
        entry.relative_path.clone()
    } else {
        Path::new(&entry.relative_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.relative_path.clone())
    };
    let (status_label, status_tone) = match entry.status {
        FolderEntryStatus::Identical => (
            t!("compare.status.identical").to_string(),
            BadgeTone::Neutral,
        ),
        FolderEntryStatus::Different => (
            t!("compare.status.different").to_string(),
            BadgeTone::Warning,
        ),
        FolderEntryStatus::OnlyLeft => (
            t!("compare.status.only_left").to_string(),
            BadgeTone::Accent,
        ),
        FolderEntryStatus::OnlyMiddle => (
            t!("compare.status.only_middle").to_string(),
            BadgeTone::Accent,
        ),
        FolderEntryStatus::OnlyRight => (
            t!("compare.status.only_right").to_string(),
            BadgeTone::Accent,
        ),
        FolderEntryStatus::TypeMismatch => (
            t!("compare.status.type_mismatch").to_string(),
            BadgeTone::Danger,
        ),
    };
    let clickable = entry.kind == FolderEntryKind::File;
    let indent = entry.depth as f32 * 16.0;

    let sizes_text = format_entry_sizes(entry, is_three_way);

    let mut row = ui::list_row(
        ("compare-folder-row", list_idx),
        RowState::NORMAL.selected(selected),
        design,
    )
    .pl(design.px(12.0 + indent))
    .pr(design.px(12.0))
    .gap_2()
    .child(Icon::new(icon_name).size(design.px(14.0)).text_color(
        if entry.kind == FolderEntryKind::Directory {
            design.accent_text
        } else {
            design.muted
        },
    ))
    .child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(design.px(12.0))
            .text_color(design.text)
            .font_weight(if entry.kind == FolderEntryKind::Directory {
                gpui::FontWeight::MEDIUM
            } else {
                gpui::FontWeight::NORMAL
            })
            .child(name),
    )
    .child(
        div()
            .flex_none()
            .text_size(design.px(11.0))
            .text_color(design.muted)
            .child(sizes_text),
    )
    .child(ui::status_badge(status_label, status_tone, design));

    if let Some(mark) = mark {
        let (change_label, change_tone) = match mark.change {
            ThreeWayTreeChange::LeftChanged => (
                t!("compare.three_way.change.left").to_string(),
                BadgeTone::Accent,
            ),
            ThreeWayTreeChange::RightChanged => (
                t!("compare.three_way.change.right").to_string(),
                BadgeTone::Accent,
            ),
            ThreeWayTreeChange::BothAgree => (
                t!("compare.three_way.change.agree").to_string(),
                BadgeTone::Neutral,
            ),
            ThreeWayTreeChange::BothDisagree => (
                t!("compare.three_way.change.disagree").to_string(),
                BadgeTone::Warning,
            ),
        };
        row = row.child(ui::status_badge(change_label, change_tone, design));
        if mark.conflicting {
            row = row.child(ui::status_badge(
                t!("compare.three_way.conflict_badge").to_string(),
                BadgeTone::Danger,
                design,
            ));
        }
    }

    if clickable {
        row = row.cursor_pointer().on_click(move |_, window, cx| {
            let _ = entity.update(cx, |view, cx| view.open_folder_entry(entry_idx, window, cx));
        });
    }

    row.into_any_element()
}

fn format_entry_sizes(entry: &FolderCompareEntry, is_three_way: bool) -> String {
    if entry.kind != FolderEntryKind::File {
        return String::new();
    }
    let fmt = |idx: usize| -> String {
        match entry.sides.get(idx).and_then(|s| s.as_ref()) {
            Some(side) => side.size.map(format_size).unwrap_or_else(|| "—".to_owned()),
            None => "—".to_owned(),
        }
    };
    if is_three_way {
        format!("{} | {} | {}", fmt(0), fmt(1), fmt(2))
    } else {
        format!("{} | {}", fmt(0), fmt(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn dialog_validation_accepts_two_or_three_files_or_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let file_a = tmp.path().join("a.txt");
        let file_b = tmp.path().join("b.txt");
        let file_c = tmp.path().join("c.txt");
        let dir_a = tmp.path().join("dir_a");
        let dir_b = tmp.path().join("dir_b");
        let dir_c = tmp.path().join("dir_c");
        fs::write(&file_a, "a").unwrap();
        fs::write(&file_b, "b").unwrap();
        fs::write(&file_c, "c").unwrap();
        fs::create_dir(&dir_a).unwrap();
        fs::create_dir(&dir_b).unwrap();
        fs::create_dir(&dir_c).unwrap();

        assert_eq!(
            validate_dialog_inputs(&file_a.to_string_lossy(), &file_b.to_string_lossy(), ""),
            Ok(CompareTarget::TwoWayFiles {
                left: file_a.clone(),
                right: file_b.clone(),
            })
        );
        assert_eq!(
            validate_dialog_inputs(
                &file_a.to_string_lossy(),
                &file_c.to_string_lossy(),
                &file_b.to_string_lossy(),
            ),
            Ok(CompareTarget::ThreeWayFiles {
                left: file_a.clone(),
                middle: file_b.clone(),
                right: file_c.clone(),
            })
        );
        assert_eq!(
            validate_dialog_inputs(&dir_a.to_string_lossy(), &dir_b.to_string_lossy(), " "),
            Ok(CompareTarget::TwoWayFolders {
                left: dir_a.clone(),
                right: dir_b.clone(),
            })
        );
        assert_eq!(
            validate_dialog_inputs(
                &dir_a.to_string_lossy(),
                &dir_c.to_string_lossy(),
                &dir_b.to_string_lossy(),
            ),
            Ok(CompareTarget::ThreeWayFolders {
                left: dir_a.clone(),
                middle: dir_b.clone(),
                right: dir_c.clone(),
            })
        );
    }

    #[test]
    fn dialog_validation_rejects_missing_nonexistent_or_mixed_paths() {
        let tmp = TempDir::new().expect("tempdir");
        let file_a = tmp.path().join("a.txt");
        let dir_b = tmp.path().join("dir_b");
        let missing = tmp.path().join("missing.txt");
        fs::write(&file_a, "a").unwrap();
        fs::create_dir(&dir_b).unwrap();

        assert_eq!(
            validate_dialog_inputs("", &file_a.to_string_lossy(), ""),
            Err(CompareValidationError::MissingRequired)
        );
        assert_eq!(
            validate_dialog_inputs(&file_a.to_string_lossy(), "", ""),
            Err(CompareValidationError::MissingRequired)
        );
        assert_eq!(
            validate_dialog_inputs(&file_a.to_string_lossy(), &missing.to_string_lossy(), ""),
            Err(CompareValidationError::PathNotFound(missing.clone()))
        );
        assert_eq!(
            validate_dialog_inputs(&file_a.to_string_lossy(), &dir_b.to_string_lossy(), ""),
            Err(CompareValidationError::MixedTypes)
        );
        assert_eq!(
            validate_dialog_inputs(
                &file_a.to_string_lossy(),
                &file_a.to_string_lossy(),
                &dir_b.to_string_lossy(),
            ),
            Err(CompareValidationError::MixedTypes)
        );
    }

    #[test]
    fn dialog_cancel_is_a_secondary_button() {
        assert_eq!(dialog_cancel_button().button_kind(), ButtonKind::Normal);
    }

    #[test]
    fn shared_three_way_width_covers_the_widest_side() {
        let left = vec!["short".to_owned()];
        let middle = vec!["\tindented".to_owned()];
        let right = vec!["日本語".to_owned(), "x".to_owned()];

        // Four columns before the tab stop, plus four columns of indent.
        assert_eq!(three_way_max_columns(&left, &middle, &right, 4), 12);
        assert_eq!(three_way_max_columns(&left, &middle, &right, 8), 16);
        assert_eq!(
            three_way_max_columns(&left, &middle, &right, 1),
            display_width_with_tabs("\tindented", 1)
        );
    }

    #[test]
    fn ordered_path_validation_checks_arg_count() {
        let tmp = TempDir::new().expect("tempdir");
        let file = tmp.path().join("a.txt");
        fs::write(&file, "a").unwrap();

        assert_eq!(
            validate_ordered_paths(std::slice::from_ref(&file)),
            Err(CompareValidationError::WrongArgCount(1))
        );
        assert_eq!(
            validate_ordered_paths(&[file.clone(), file.clone(), file.clone(), file]),
            Err(CompareValidationError::WrongArgCount(4))
        );
    }
}
