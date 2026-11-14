//! The Git config window: every effective value, where it came from, what it
//! overrode, and the editor that writes one back.
//!
//! `strata-core`'s `repo::config` module reads the files on a background
//! worker, so the UI thread only ever filters and draws the result. Every
//! write is previewed against a temporary copy first and only then saved, so
//! the user sees the exact command and diff and no file changes silently. A
//! repository only exists while a repository view is active, so that view
//! publishes its path and command log to [`CurrentRepository`] and the command
//! palette, the settings window and this module's own action all open the same
//! window for it.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable as _,
    FontWeight, Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, Stateful,
    StatefulInteractiveElement as _, Styled, Subscription, WeakEntity, Window, WindowBounds,
    WindowDecorations, WindowOptions, actions, div, px, size,
};
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme as _, Selectable as _, TitleBar, h_flex, v_flex};
use strata_core::diff::LineKind;
use strata_core::git_cli::{
    CommandLog, ConfigChange, ConfigPreview, ConfigTarget, ConfigWrite, Git,
};
use strata_core::repo::{
    ConfigEntry, ConfigFilePaths, ConfigInclude, ConfigLayer, ConfigValue, GitConfig, ValueRule,
    WellKnownKey,
};

use crate::actions::OpenGitConfig;
use crate::settings::{self, Settings};
use crate::style::{self, Design};
use crate::ui::{self, BadgeTone, RowState, list_row};

/// Key context of the config window, so Escape closes this window and no
/// other.
const VIEW: &str = "GitConfigView";

actions!(strata_git_config, [CloseGitConfig]);

const WIDTH: f32 = 920.0;
const HEIGHT: f32 = 680.0;
const MIN_WIDTH: f32 = 640.0;
const MIN_HEIGHT: f32 = 480.0;

/// The repository whose configuration the window opens, plus the command log
/// its session keeps. Updated by the active [`crate::repo_view::RepoView`];
/// the module is careful to do nothing when no repository has been opened yet.
#[derive(Default)]
struct CurrentRepository {
    path: Option<PathBuf>,
    /// Shared with the repository's session, so a configuration write shows up
    /// in the same log as every other git command.
    command_log: Option<Arc<CommandLog>>,
}

impl Global for CurrentRepository {}

/// The one config window, so a second command raises the existing one instead
/// of stacking identical windows.
#[derive(Default)]
struct OpenConfigWindow {
    window: Option<AnyWindowHandle>,
    view: Option<WeakEntity<GitConfigView>>,
}

impl Global for OpenConfigWindow {}

/// Registers the action and the window. Call once at startup.
pub fn bind(cx: &mut App) {
    cx.set_global(CurrentRepository::default());
    cx.set_global(OpenConfigWindow::default());
    cx.on_action(|_: &OpenGitConfig, cx: &mut App| open(cx));
    crate::keymap::bind_group(cx, crate::keymap::Group::GitConfig);
}

/// Records the repository the next config command should open, and the log a
/// write is recorded in.
pub(crate) fn set_repository(path: PathBuf, command_log: Arc<CommandLog>, cx: &mut App) {
    let repository = CurrentRepository {
        path: Some(path),
        command_log: Some(command_log),
    };
    if cx.has_global::<CurrentRepository>() {
        *cx.global_mut::<CurrentRepository>() = repository;
    } else {
        cx.set_global(repository);
    }
}

/// Whether a repository is available, for the settings window's link.
pub(crate) fn has_repository(cx: &App) -> bool {
    cx.try_global::<CurrentRepository>()
        .is_some_and(|repository| repository.path.is_some())
}

fn open(cx: &mut App) {
    let Some((repository, command_log)) = cx
        .try_global::<CurrentRepository>()
        .and_then(|repository| Some((repository.path.clone()?, repository.command_log.clone())))
    else {
        return;
    };

    if let Some(view) = cx
        .try_global::<OpenConfigWindow>()
        .and_then(|window| window.view.clone())
        && let Some(view) = view.upgrade()
    {
        view.update(cx, |view, cx| {
            view.set_repository(repository, command_log, cx)
        });
        if let Some(handle) = cx
            .try_global::<OpenConfigWindow>()
            .and_then(|window| window.window)
        {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        cx.activate(true);
        return;
    }

    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    let view_slot: Rc<RefCell<Option<Entity<GitConfigView>>>> = Rc::new(RefCell::new(None));
    let slot = view_slot.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(crate::style::window_titlebar("Strata Git Config")),
            window_decorations: Some(WindowDecorations::Client),
            window_min_size: Some(size(px(MIN_WIDTH), px(MIN_HEIGHT))),
            focus: true,
            ..TitleBar::window_options()
        },
        move |window, cx| {
            settings::apply_theme(Some(window), cx);
            let view = cx.new(|cx| GitConfigView::new(repository, command_log, window, cx));
            *slot.borrow_mut() = Some(view.clone());
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        },
    );

    match opened {
        Ok(handle) => {
            let view = view_slot.borrow_mut().take().map(|view| view.downgrade());
            if cx.has_global::<OpenConfigWindow>() {
                let global = cx.global_mut::<OpenConfigWindow>();
                global.window = Some(handle.into());
                global.view = view;
            }
            cx.activate(true);
        }
        Err(error) => log::warn!("cannot open the git config window: {error}"),
    }
}

pub(crate) struct GitConfigView {
    repository: PathBuf,
    /// The session's log, when the window opened from a repository view.
    command_log: Option<Arc<CommandLog>>,
    config: Option<GitConfig>,
    error: Option<String>,
    loading: bool,
    /// Off shows the curated keys only; on shows every key.
    all_keys: bool,
    query: String,
    input: Entity<InputState>,
    expanded: HashSet<String>,
    /// The key the user last picked. A reload keeps it, so the row the write
    /// changed stays where the user left it.
    selected: Option<String>,
    /// The command a write ran, for the window's own status line.
    last_written: Option<String>,
    focus: FocusHandle,
    _appearance: Subscription,
    _input: Subscription,
}

impl GitConfigView {
    fn new(
        repository: PathBuf,
        command_log: Option<Arc<CommandLog>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter keys, values or files"));
        let input_events = cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.query = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        let appearance =
            window.observe_window_appearance(|window, cx| settings::apply_theme(Some(window), cx));
        let mut view = Self {
            repository,
            command_log,
            config: None,
            error: None,
            loading: true,
            all_keys: false,
            query: String::new(),
            input,
            expanded: HashSet::new(),
            selected: None,
            last_written: None,
            focus,
            _appearance: appearance,
            _input: input_events,
        };
        view.load(cx);
        view
    }

    fn set_repository(
        &mut self,
        repository: PathBuf,
        command_log: Option<Arc<CommandLog>>,
        cx: &mut Context<Self>,
    ) {
        self.command_log = command_log;
        if self.repository == repository && (self.config.is_some() || self.loading) {
            return;
        }
        self.repository = repository;
        self.config = None;
        self.error = None;
        self.expanded.clear();
        self.selected = None;
        self.last_written = None;
        self.load(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        let repository = self.repository.clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(
                    async move { GitConfig::load(&repository).map_err(|error| error.to_string()) },
                )
                .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match read {
                    Ok(config) => {
                        this.config = Some(config);
                        this.error = None;
                    }
                    Err(error) => {
                        this.config = None;
                        this.error = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() != Some(key) {
            self.selected = Some(key.to_owned());
            self.expanded.insert(key.to_owned());
        } else if !self.expanded.remove(key) {
            self.expanded.insert(key.to_owned());
        }
        cx.notify();
    }

    /// A write went through: reload, keep the key selected and report the
    /// command.
    fn after_write(&mut self, command: String, key: String, cx: &mut Context<Self>) {
        self.selected = Some(key.clone());
        self.expanded.insert(key);
        self.last_written = Some(command);
        self.load(cx);
    }

    /// Open the editor for `key` with `change`, or for a free key name when
    /// `new_key` is set.
    fn open_editor(
        &mut self,
        key: String,
        change: ConfigChange,
        new_key: bool,
        target: Option<ConfigLayer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = self.config.clone() else {
            return;
        };
        let entry = config.entry(&key).cloned();
        let current = match &change {
            // Removing one value targets the file that holds that value, not
            // the one that wins.
            ConfigChange::UnsetValue(removed) => entry
                .as_ref()
                .and_then(|entry| entry.values.iter().find(|value| &value.value == removed))
                .cloned()
                .or_else(|| entry.as_ref().and_then(ConfigEntry::effective).cloned()),
            _ => entry.as_ref().and_then(ConfigEntry::effective).cloned(),
        };
        let (targets, computed_default) = target_choices(&config, current.as_ref());
        // A helper that names the write it wants (the identity override) can
        // ask for local even when the effective value came from elsewhere.
        let default = target
            .and_then(|layer| {
                targets
                    .iter()
                    .position(|choice| choice.target.layer == layer && !choice.disabled)
            })
            .unwrap_or(computed_default);
        let well_known = strata_core::repo::well_known_key(&key);
        let view = cx.entity().downgrade();
        let dialog = cx.new(|cx| {
            ConfigEditor::new(
                cx,
                view,
                EditorRequest {
                    repository: self.repository.clone(),
                    command_log: self.command_log.clone(),
                    key,
                    change,
                    new_key,
                    well_known,
                    targets,
                    default,
                },
                window,
            )
        });
        // Unset has nothing to fill in: the preview is the whole form.
        if !dialog.read(cx).kind.needs_value() {
            dialog.update(cx, |dialog, cx| dialog.preview(cx));
        }
        let focus = dialog.read(cx).focus_handle(cx);
        let content = dialog.clone();
        let state = dialog.clone();
        window.open_dialog(cx, move |card, _, cx| {
            let previewing = state.read(cx).has_preview();
            let valid = state.read(cx).can_preview(cx);
            let content = content.clone();
            let state_for_ok = state.clone();
            let state_for_back = state.clone();
            let state_for_cancel = state.clone();
            let state_for_enter = state.clone();
            let title = state.read(cx).title();
            card.title(title)
                .w(px(760.0))
                .content(move |body, _, _| body.child(content.clone()))
                .footer(
                    DialogFooter::new()
                        .child(
                            ui::button("config-editor-cancel", "Cancel")
                                .size(ui::ButtonSize::Medium)
                                .on_click(move |_, window, cx| {
                                    // A preview is one step; Cancel there goes
                                    // back to the form instead of closing.
                                    if state_for_cancel.read(cx).has_preview() {
                                        state_for_cancel.update(cx, |dialog, cx| {
                                            dialog.back_to_form(cx);
                                        });
                                    } else {
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .when(previewing, |footer| {
                            let back = state_for_back.clone();
                            footer.child(
                                ui::button("config-editor-write", "Write")
                                    .size(ui::ButtonSize::Medium)
                                    .on_click(move |_, window, cx| {
                                        back.update(cx, |dialog, cx| {
                                            dialog.apply(window, cx);
                                        });
                                    }),
                            )
                        })
                        .when(!previewing, |footer| {
                            footer.child(
                                ui::button("config-editor-preview", "Preview write")
                                    .size(ui::ButtonSize::Medium)
                                    .disabled(!valid)
                                    .on_click(move |_, _, cx| {
                                        state_for_ok.update(cx, |dialog, cx| {
                                            dialog.preview(cx);
                                        });
                                    }),
                            )
                        }),
                )
                .on_ok(move |_, _, cx| {
                    if state_for_enter.read(cx).has_preview() {
                        return false;
                    }
                    if state_for_enter.read(cx).can_preview(cx) {
                        state_for_enter.update(cx, |dialog, cx| dialog.preview(cx));
                    }
                    false
                })
        });
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        cx.notify();
    }

    fn title_bar(&self, design: &Design) -> impl IntoElement {
        TitleBar::new()
            .h(px(crate::style::TITLE_BAR_HEIGHT))
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
                            .gap(px(7.0))
                            .child(style::app_mark(style::BRAND_MARK))
                            .child(style::wordmark(style::BRAND_WORDMARK, design.text)),
                    )
                    .child(div().w(px(1.0)).h(px(18.0)).mx_1().bg(design.border))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Git config")),
            )
    }

    fn identity(
        &self,
        config: &GitConfig,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mismatch = config.identity_needing_local_override().cloned();
        let mut section = v_flex()
            .flex_none()
            .border_b_1()
            .border_color(design.border)
            .bg(design.chrome)
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .text_size(px(design.tiny()))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(design.muted)
                    .child("IDENTITY"),
            )
            .children([
                identity_row("user.name", &config.identity.name, design),
                identity_row("user.email", &config.identity.email, design),
            ]);
        if let Some(value) = mismatch {
            section = section.child(
                h_flex()
                    .px_3()
                    .pb_1()
                    .gap_2()
                    .child(
                        ui::button(
                            "identity-local-override",
                            "Set user.email for this repository…",
                        )
                        .size(ui::ButtonSize::Tiny)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_editor(
                                "user.email".to_owned(),
                                ConfigChange::Set(value.value.clone()),
                                false,
                                Some(ConfigLayer::Local),
                                window,
                                cx,
                            );
                        })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(format!(
                                "user.email comes from {}; a local value overrides it for this repository only.",
                                value.origin.label()
                            )),
                    ),
            );
        }
        section.pb_1().into_any_element()
    }

    fn toolbar(&self, design: &Design, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_none()
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(design.border)
            .child(div().flex_1().min_w_0().child(Input::new(&self.input)))
            .child(
                ui::button("config-add-key", "Add key…")
                    .size(ui::ButtonSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_editor(
                            String::new(),
                            ConfigChange::Set(String::new()),
                            true,
                            None,
                            window,
                            cx,
                        );
                    })),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(design.small()))
                            .text_color(design.muted)
                            .child("All keys"),
                    )
                    .child(
                        Switch::new("all-config-keys")
                            .checked(self.all_keys)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.all_keys = *checked;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn entry_row(
        &self,
        entry: &ConfigEntry,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = self.expanded.contains(&entry.key);
        let key = entry.key.clone();
        let selected = self.selected.as_deref() == Some(entry.key.as_str());
        let mut row = list_row(
            format!("config-entry-{}", entry.key),
            RowState::NORMAL.selected(selected),
            design,
        )
        .h(px(30.0))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| this.toggle(&key, cx)))
        .child(
            div()
                .flex_none()
                .w(px(14.0))
                .text_size(px(10.0))
                .text_color(design.muted)
                .child(if expanded { "▾" } else { "▸" }),
        )
        .child(
            div()
                .flex_none()
                .w(px(280.0))
                .min_w_0()
                .truncate()
                .font_family(design.mono.clone())
                .child(entry.key.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(design.mono.clone())
                .text_color(design.text)
                .child(value_summary(entry)),
        );

        if let Some(value) = entry.effective() {
            row = row.child(layer_badge(value.layer, design));
        }
        if entry.values.len() > 1 {
            row = row.child(crate::ui::status_badge(
                format!("{} values", entry.values.len()),
                BadgeTone::Neutral,
                design,
            ));
        }
        if entry.warning.is_some() {
            row = row.child(crate::ui::status_badge(
                "invalid",
                BadgeTone::Warning,
                design,
            ));
        }

        let details = expanded.then(|| self.entry_details(entry, design, cx));
        v_flex()
            .flex_none()
            .child(row)
            .children(details)
            .into_any_element()
    }

    fn entry_details(
        &self,
        entry: &ConfigEntry,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut details = v_flex().pl(px(34.0)).pr_3().pb_2().gap_1();
        if let Some(well_known) = entry.well_known {
            details = details.child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(well_known.summary),
            );
        }

        let key = entry.key.clone();
        let current = entry
            .effective()
            .map(|value| value.value.clone())
            .unwrap_or_default();
        let edit_key = key.clone();
        let mut actions = h_flex()
            .gap_2()
            .pl(px(150.0))
            .child(
                ui::button(format!("config-edit-{}", entry.key), "Edit value…")
                    .size(ui::ButtonSize::Tiny)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_editor(
                            edit_key.clone(),
                            ConfigChange::Set(current.clone()),
                            false,
                            None,
                            window,
                            cx,
                        );
                    })),
            )
            .child(
                ui::button(format!("config-unset-{}", entry.key), "Unset…")
                    .kind(ui::ButtonKind::Normal)
                    .size(ui::ButtonSize::Tiny)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_editor(key.clone(), ConfigChange::Unset, false, None, window, cx);
                    })),
            );
        if entry.multivalued {
            let key = entry.key.clone();
            actions = actions.child(
                ui::button(format!("config-add-value-{}", entry.key), "Add value…")
                    .kind(ui::ButtonKind::Normal)
                    .size(ui::ButtonSize::Tiny)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_editor(
                            key.clone(),
                            ConfigChange::Add(String::new()),
                            false,
                            None,
                            window,
                            cx,
                        );
                    })),
            );
        }
        details = details.child(actions);

        for (index, value) in entry.values.iter().enumerate() {
            let mut row =
                value_detail(format!("{}:{index}", entry.key), "Effective", value, design);
            if entry.multivalued {
                let key = entry.key.clone();
                let removed = value.value.clone();
                row = row.child(
                    ui::button(format!("config-remove-{}-{index}", entry.key), "Remove")
                        .kind(ui::ButtonKind::Normal)
                        .size(ui::ButtonSize::Tiny)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_editor(
                                key.clone(),
                                ConfigChange::UnsetValue(removed.clone()),
                                false,
                                None,
                                window,
                                cx,
                            );
                        })),
                );
            }
            details = details.child(row);
        }
        let overridden_by = entry
            .effective()
            .map(|value| format!("{}:{}", value.layer.label(), line(value)))
            .unwrap_or_default();
        for (index, value) in entry.overridden.iter().enumerate() {
            details = details.child(value_detail(
                format!("{}:overridden:{index}", entry.key),
                &format!("Overridden by {overridden_by}"),
                value,
                design,
            ));
        }
        if let Some(warning) = &entry.warning {
            details = details.child(ui::banner(ui::BannerKind::Warning, warning.clone(), design));
        }
        details
    }

    fn includes(&self, config: &GitConfig, design: &Design) -> impl IntoElement {
        let mut section = v_flex()
            .flex_none()
            .border_t_1()
            .border_color(design.border)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(design.tiny()))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(design.muted)
                    .child(format!("INCLUDES ({})", config.includes.len())),
            );
        if config.includes.is_empty() {
            section = section.child(
                div()
                    .px_3()
                    .pb_2()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child("No include or includeIf directives were found."),
            );
            return section;
        }
        for (index, include) in config.includes.iter().enumerate() {
            section = section.child(include_row(index, include, design));
        }
        section
    }
}

fn identity_row(
    label: &'static str,
    value: &Option<ConfigValue>,
    design: &Design,
) -> impl IntoElement {
    let id = format!("identity-{label}");
    let mut row = list_row(id, RowState::NORMAL, design).h(px(26.0)).child(
        div()
            .flex_none()
            .w(px(96.0))
            .text_size(px(design.tiny()))
            .text_color(design.muted)
            .child(label),
    );
    match value {
        Some(value) => {
            row = row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(design.mono.clone())
                        .child(value.value.clone()),
                )
                .child(layer_badge(value.layer, design))
                .child(
                    div()
                        .flex_none()
                        .max_w(px(300.0))
                        .truncate()
                        .text_size(px(design.tiny()))
                        .text_color(design.muted)
                        .child(value.origin.label()),
                );
        }
        None => {
            row = row.child(
                div()
                    .flex_1()
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child("not set"),
            );
        }
    }
    row
}

fn value_detail(
    id: String,
    label: &str,
    value: &ConfigValue,
    design: &Design,
) -> Stateful<gpui::Div> {
    let path = value.origin.path.clone();
    h_flex()
        .id(id.clone())
        .gap_2()
        .min_w_0()
        .child(
            div()
                .flex_none()
                .w(px(150.0))
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(label.to_owned()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(design.mono.clone())
                .child(value.value.clone()),
        )
        .child(layer_badge(value.layer, design))
        .child(
            div()
                .flex_none()
                .min_w_0()
                .max_w(px(240.0))
                .truncate()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(value.origin.label()),
        )
        .child(
            crate::ui::button(format!("reveal-{id}"), "Reveal file")
                .size(crate::ui::ButtonSize::Tiny)
                .disabled(path.is_none())
                .on_click(move |_, _, cx| {
                    if let Some(folder) = path.as_deref().and_then(Path::parent) {
                        cx.open_with_system(folder);
                    }
                }),
        )
}

fn include_row(index: usize, include: &ConfigInclude, design: &Design) -> impl IntoElement {
    let tone = if include.matched {
        BadgeTone::Accent
    } else {
        BadgeTone::Neutral
    };
    let status = if include.matched {
        "matched"
    } else {
        "not matched"
    };
    let mut row = list_row(("config-include", index), RowState::NORMAL, design)
        .h(px(30.0))
        .child(crate::ui::status_badge(status, tone, design))
        .child(
            div()
                .flex_none()
                .w(px(330.0))
                .min_w_0()
                .truncate()
                .font_family(design.mono.clone())
                .child(include.condition.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(design.muted)
                .child(
                    include
                        .path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "(unresolved)".to_owned()),
                ),
        )
        .child(layer_badge(include.layer, design));
    if include.matched {
        row = row.child(
            div()
                .flex_none()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(format!("{} values", include.values)),
        );
    }
    if let Some(error) = &include.error {
        row = row
            .child(crate::ui::status_badge("error", BadgeTone::Danger, design))
            .child(
                div()
                    .flex_none()
                    .text_size(px(design.tiny()))
                    .text_color(design.remove_text)
                    .child(error.clone()),
            );
    }
    row
}

fn layer_badge(layer: ConfigLayer, design: &Design) -> impl IntoElement {
    let tone = match layer {
        ConfigLayer::Local | ConfigLayer::Worktree => BadgeTone::Accent,
        ConfigLayer::System => BadgeTone::Neutral,
        _ => BadgeTone::Neutral,
    };
    crate::ui::status_badge(layer.label(), tone, design)
}

fn line(value: &ConfigValue) -> String {
    value
        .origin
        .line
        .map_or_else(String::new, |line| line.to_string())
}

fn value_summary(entry: &ConfigEntry) -> String {
    if entry.values.is_empty() {
        return String::new();
    }
    entry
        .values
        .iter()
        .map(|value| value.value.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// True when `entry` matches the search text in its key, value, layer, curated
/// explanation or source path.
pub(crate) fn matches_query(entry: &ConfigEntry, query: &str) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    if entry.key.to_ascii_lowercase().contains(&query) {
        return true;
    }
    if entry
        .well_known
        .is_some_and(|key| key.summary.to_ascii_lowercase().contains(&query))
    {
        return true;
    }
    entry
        .values
        .iter()
        .chain(entry.overridden.iter())
        .any(|value| value_matches(value, &query))
}

fn value_matches(value: &ConfigValue, query: &str) -> bool {
    value.value.to_ascii_lowercase().contains(query)
        || value.layer.label().to_ascii_lowercase().contains(query)
        || value
            .origin
            .path
            .as_ref()
            .is_some_and(|path| path.to_string_lossy().to_ascii_lowercase().contains(query))
}

/// The rows the window shows, in the configuration's sorted key order.
pub(crate) fn visible_entries<'a>(
    config: &'a GitConfig,
    query: &str,
    all_keys: bool,
) -> Vec<&'a ConfigEntry> {
    config
        .entries
        .iter()
        .filter(|entry| (all_keys || entry.well_known.is_some()) && matches_query(entry, query))
        .collect()
}

impl Render for GitConfigView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let entries = self
            .config
            .as_ref()
            .map(|config| visible_entries(config, &self.query, self.all_keys))
            .unwrap_or_default();

        let content: gpui::AnyElement = if self.loading {
            crate::ui::message("Reading git config…", design.muted, &design).into_any_element()
        } else if let Some(error) = &self.error {
            crate::ui::message(error.clone(), design.remove_text, &design).into_any_element()
        } else {
            let mut list = v_flex().flex_none().py_1();
            for entry in &entries {
                list = list.child(self.entry_row(entry, &design, cx));
            }
            if entries.is_empty() {
                list = list.child(
                    div()
                        .px_3()
                        .py_4()
                        .text_size(px(design.small()))
                        .text_color(design.muted)
                        .child(if self.all_keys {
                            "No configuration keys match the filter."
                        } else {
                            "No well-known keys match the filter. Turn on All keys to see every key."
                        }),
                );
            }
            list.into_any_element()
        };

        let includes = self
            .config
            .as_ref()
            .map(|config| self.includes(config, &design));

        let summary = self
            .config
            .as_ref()
            .map(|config| self.identity(config, &design, cx));

        v_flex()
            .size_full()
            .key_context(VIEW)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &CloseGitConfig, window, _| window.remove_window()))
            .bg(design.background)
            .text_color(design.text)
            .text_size(px(design.ui))
            .child(self.title_bar(&design))
            .children(summary)
            .child(self.toolbar(&design, cx))
            .child(
                div()
                    .id("git-config-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(content)
                    .children(includes),
            )
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .h(px(36.0))
                    .px_3()
                    .border_t_1()
                    .border_color(design.border)
                    .bg(design.chrome)
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(design.mono.clone())
                            .child(
                                self.last_written
                                    .clone()
                                    .unwrap_or_else(|| self.repository.display().to_string()),
                            ),
                    )
                    .child(div().flex_none().child(format!("{} keys", entries.len()))),
            )
    }
}

/// One file the editor can write, with the reason when it cannot.
struct TargetChoice {
    target: ConfigTarget,
    label: &'static str,
    detail: String,
    disabled: bool,
}

/// The files an edit of `key` may write to, and the one selected first.
///
/// The order is the precedence order the window already lists; system comes
/// last and disabled because changing it needs administrator rights.
fn target_choices(config: &GitConfig, current: Option<&ConfigValue>) -> (Vec<TargetChoice>, usize) {
    let files: &ConfigFilePaths = &config.files;
    let mut choices = Vec::new();
    let mut seen = HashSet::new();

    push_target(
        &mut choices,
        &mut seen,
        ConfigTarget::new(files.local.clone(), ConfigLayer::Local),
        "local",
        files.local.display().to_string(),
    );
    if let Some(worktree) = &files.worktree {
        push_target(
            &mut choices,
            &mut seen,
            ConfigTarget::new(worktree.clone(), ConfigLayer::Worktree),
            "worktree",
            format!(
                "{} (enabled by extensions.worktreeConfig)",
                worktree.display()
            ),
        );
    }
    if let Some(global) = &files.global {
        push_target(
            &mut choices,
            &mut seen,
            ConfigTarget::new(global.clone(), ConfigLayer::Global),
            "global",
            global.display().to_string(),
        );
    }

    if let Some(value) = current
        && let Some(path) = &value.origin.path
        && !seen.contains(path)
    {
        let (label, detail) = if value.layer == ConfigLayer::System
            || files.system.as_deref() == Some(path.as_path())
        {
            (
                "system",
                format!("{} — needs administrator rights", path.display()),
            )
        } else if value.origin.include.is_some() {
            let condition = value
                .origin
                .include
                .and_then(|index| config.includes.get(index))
                .map(|include| include.condition.clone())
                .unwrap_or_else(|| "include".to_owned());
            ("include file", format!("{condition} · {}", path.display()))
        } else {
            (value.layer.label(), path.display().to_string())
        };
        push_target(
            &mut choices,
            &mut seen,
            ConfigTarget::new(path.clone(), value.layer),
            label,
            detail,
        );
    }

    if let Some(system) = &files.system
        && !seen.contains(system)
    {
        push_target(
            &mut choices,
            &mut seen,
            ConfigTarget::new(system.clone(), ConfigLayer::System),
            "system",
            format!("{} — needs administrator rights", system.display()),
        );
    }

    let default = current
        .and_then(|value| value.origin.path.as_ref())
        .and_then(|path| {
            choices
                .iter()
                .position(|choice| &choice.target.path == path && !choice.disabled)
        })
        .or_else(|| {
            choices
                .iter()
                .position(|choice| choice.target.layer == ConfigLayer::Local)
        })
        .unwrap_or(0);

    // A key the repository does not set yet starts with a local write, which
    // is the one that travels with the repository.
    (choices, default)
}

fn push_target(
    choices: &mut Vec<TargetChoice>,
    seen: &mut HashSet<PathBuf>,
    target: ConfigTarget,
    label: &'static str,
    detail: String,
) {
    if !seen.insert(target.path.clone()) {
        return;
    }
    let disabled = !target.writable();
    choices.push(TargetChoice {
        target,
        label,
        detail,
        disabled,
    });
}

/// What the editor is about to do, kept apart from the value so a picker can
/// change one without rebuilding the other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteKind {
    Set,
    Add,
    Unset,
    UnsetValue,
}

impl WriteKind {
    fn of(change: &ConfigChange) -> Self {
        match change {
            ConfigChange::Set(_) => Self::Set,
            ConfigChange::Add(_) => Self::Add,
            ConfigChange::Unset => Self::Unset,
            ConfigChange::UnsetValue(_) => Self::UnsetValue,
        }
    }

    fn needs_value(self) -> bool {
        !matches!(self, Self::Unset)
    }
}

/// Everything the editor starts with, gathered by the view from its data.
struct EditorRequest {
    repository: PathBuf,
    command_log: Option<Arc<CommandLog>>,
    key: String,
    change: ConfigChange,
    new_key: bool,
    well_known: Option<&'static WellKnownKey>,
    targets: Vec<TargetChoice>,
    default: usize,
}

/// The write form and its preview.
///
/// The form checks the key, the target and a curated value before it lets the
/// user ask for a preview. The preview runs the exact command against a
/// temporary copy and keeps the file's stamp; Write runs it against the real
/// file only while the stamp still matches.
struct ConfigEditor {
    view: WeakEntity<GitConfigView>,
    repository: PathBuf,
    command_log: Option<Arc<CommandLog>>,
    targets: Vec<TargetChoice>,
    target: usize,
    key: String,
    kind: WriteKind,
    value: String,
    /// The curated entry the key had when the form opened. A free key looks
    /// its rule up again as the name is typed.
    well_known: Option<&'static WellKnownKey>,
    new_key: bool,
    key_input: Entity<InputState>,
    value_input: Entity<InputState>,
    preview: Option<ConfigPreview>,
    error: Option<String>,
    _inputs: Vec<Subscription>,
}

impl ConfigEditor {
    fn new(
        cx: &mut Context<Self>,
        view: WeakEntity<GitConfigView>,
        request: EditorRequest,
        window: &mut Window,
    ) -> Self {
        let EditorRequest {
            repository,
            command_log,
            key,
            change,
            new_key,
            well_known,
            targets,
            default,
        } = request;
        let kind = WriteKind::of(&change);
        let value = change.value().unwrap_or_default().to_owned();
        let key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("section[.subsection].name")
                .default_value(key.clone())
        });
        let value_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("value")
                .default_value(value.clone())
        });
        let key_events = cx.subscribe(&key_input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.key = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        let value_events = cx.subscribe(&value_input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.value = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        Self {
            view,
            repository,
            command_log,
            targets,
            target: default,
            key,
            kind,
            value,
            well_known,
            new_key,
            key_input,
            value_input,
            preview: None,
            error: None,
            _inputs: vec![key_events, value_events],
        }
    }

    fn title(&self) -> String {
        let key = if self.new_key && self.key.trim().is_empty() {
            "new key".to_owned()
        } else {
            self.key.clone()
        };
        match self.kind {
            WriteKind::Set if self.new_key => format!("Add {key}"),
            WriteKind::Set => format!("Edit {key}"),
            WriteKind::Add => format!("Add a value to {key}"),
            WriteKind::Unset => format!("Unset {key}"),
            WriteKind::UnsetValue => format!("Remove a value from {key}"),
        }
    }

    fn has_preview(&self) -> bool {
        self.preview.is_some()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.new_key {
            self.key_input.focus_handle(cx)
        } else {
            self.value_input.focus_handle(cx)
        }
    }

    /// The curated rule the key has right now.
    fn rule(&self) -> Option<&'static WellKnownKey> {
        if self.new_key {
            strata_core::repo::well_known_key(self.key.trim())
        } else {
            self.well_known
        }
    }

    /// The values a picker offers, in the order [`ValueRule`] lists them. An
    /// empty list means the value is free text.
    fn picker_values(&self) -> Vec<&'static str> {
        let Some(rule) = self.rule().map(|well_known| well_known.rule) else {
            return Vec::new();
        };
        match rule {
            ValueRule::Text => Vec::new(),
            ValueRule::Boolean => vec!["true", "false"],
            ValueRule::Enum(values) => values.to_vec(),
            ValueRule::BooleanOrEnum(values) => {
                let mut all = vec!["true", "false"];
                all.extend_from_slice(values);
                all
            }
        }
    }

    /// The write the form describes, refused before git sees it when a key or
    /// a curated value is wrong.
    fn write(&self) -> strata_core::Result<ConfigWrite> {
        let Some(choice) = self.targets.get(self.target) else {
            return Err(strata_core::Error::other("no target file was chosen"));
        };
        let change = match self.kind {
            WriteKind::Set => ConfigChange::Set(self.value.clone()),
            WriteKind::Add => ConfigChange::Add(self.value.clone()),
            WriteKind::Unset => ConfigChange::Unset,
            WriteKind::UnsetValue => ConfigChange::UnsetValue(self.value.clone()),
        };
        let write = ConfigWrite::new(choice.target.clone(), self.key.trim().to_owned(), change);
        write.validate()?;
        Ok(write)
    }

    fn validation_error(&self) -> Option<String> {
        self.write().err().map(|error| error.to_string())
    }

    fn can_preview(&self, _cx: &App) -> bool {
        self.validation_error().is_none() && !self.targets.is_empty()
    }

    fn git(&self, cx: &App, with_log: bool) -> Git {
        let mut git = Git::new(self.repository.clone());
        if let Some(binary) = Settings::get(cx).behaviour.git_executable.clone() {
            git = git.with_git_binary(binary);
        }
        if with_log && let Some(log) = self.command_log.clone() {
            git = git.with_log(log);
        }
        git
    }

    /// Run the command against a copy. The preview does not get the session's
    /// log: it writes a temporary file, and that is not a command the user ran.
    fn preview(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.preview = None;
        let write = match self.write() {
            Ok(write) => write,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        match self.git(cx, false).config_preview(&write) {
            Ok(preview) => self.preview = Some(preview),
            Err(error) => self.error = Some(error_text(error)),
        }
        cx.notify();
    }

    fn back_to_form(&mut self, cx: &mut Context<Self>) {
        self.preview = None;
        self.error = None;
        cx.notify();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.preview.clone() else {
            return;
        };
        match self.git(cx, true).config_apply(&preview) {
            Ok(()) => {
                let command = preview.command.clone();
                let key = preview.write.key.clone();
                self.view
                    .update(cx, |view, cx| view.after_write(command, key, cx))
                    .ok();
                // Closing here would drop this dialog while it is being
                // updated; the same reasoning the reword dialog uses.
                window.defer(cx, |window, cx| window.close_dialog(cx));
            }
            Err(error) => {
                self.preview = None;
                self.error = Some(format!(
                    "{} The file was not changed; preview the write again.",
                    error_text(error)
                ));
                cx.notify();
            }
        }
    }

    fn form(&self, design: &Design, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut form = v_flex().gap_3().w_full();
        if let Some(error) = &self.error {
            form = form.child(ui::banner(ui::BannerKind::Warning, error.clone(), design));
        }

        if self.new_key {
            form = form.child(field("Key", &self.key_input, design));
        }

        let mut chooser = h_flex().flex_wrap().gap_2();
        for (index, choice) in self.targets.iter().enumerate() {
            let selected = self.target == index;
            chooser = chooser.child(
                ui::button(format!("config-target-{index}"), choice.label)
                    .kind(ui::ButtonKind::Normal)
                    .size(ui::ButtonSize::Tiny)
                    .selected(selected)
                    .disabled(choice.disabled)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.target = index;
                        this.preview = None;
                        cx.notify();
                    })),
            );
        }
        form = form
            .child(label("Target file", design))
            .child(chooser)
            .child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(
                        self.targets
                            .get(self.target)
                            .map(|choice| choice.detail.clone())
                            .unwrap_or_else(|| "no writable file".to_owned()),
                    ),
            );

        if let Some(rule) = self.rule() {
            form = form.child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(format!("{} {}", rule.key, rule.summary)),
            );
        }

        if self.kind.needs_value() {
            let picker = self.picker_values();
            form = form.child(label("Value", design));
            if picker.is_empty() {
                form = form.child(Input::new(&self.value_input).w_full().bg(design.field));
            } else {
                let mut values = h_flex().flex_wrap().gap_2();
                for (index, value) in picker.iter().enumerate() {
                    let picked: &'static str = value;
                    let selected = self.value == picked;
                    values = values.child(
                        ui::button(format!("config-value-{index}"), picked)
                            .kind(ui::ButtonKind::Normal)
                            .size(ui::ButtonSize::Tiny)
                            .selected(selected)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.value = picked.to_owned();
                                cx.notify();
                            })),
                    );
                }
                form = form.child(values);
                // An existing value the picker cannot represent has to stay
                // visible: the user may have typed it into the file.
                if !picker.contains(&self.value.as_str()) && !self.value.is_empty() {
                    form = form.child(
                        div()
                            .text_size(px(design.tiny()))
                            .text_color(design.remove_text)
                            .child(format!(
                                "`{}` is not one of the accepted values.",
                                self.value
                            )),
                    );
                }
            }
        }

        if let Some(error) = self.validation_error() {
            form = form.child(ui::banner(
                ui::BannerKind::Warning,
                format!("Cannot save: {error}"),
                design,
            ));
        } else if self.kind.needs_value()
            && self.value.is_empty()
            && self.picker_values().is_empty()
        {
            form = form.child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child("An empty value is written as an empty string."),
            );
        }

        form.into_any_element()
    }

    fn preview_body(&self, design: &Design) -> gpui::AnyElement {
        let Some(preview) = &self.preview else {
            return v_flex()
                .child("No preview is available.")
                .into_any_element();
        };
        let mut body = v_flex().gap_2().w_full();
        body = body
            .child(label("File", design))
            .child(
                div()
                    .font_family(design.mono.clone())
                    .text_size(px(design.small()))
                    .child(preview.write.target.path.display().to_string()),
            )
            .child(label("Command", design))
            .child(
                div()
                    .w_full()
                    .rounded(px(4.0))
                    .bg(design.field)
                    .p_2()
                    .font_family(design.mono.clone())
                    .text_size(px(design.tiny()))
                    .child(preview.command.clone()),
            )
            .child(label("Changes", design));

        if !preview.changed() {
            body = body.child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child("The command would not change this file."),
            );
            return body.into_any_element();
        }

        let mut diff = v_flex()
            .id("config-preview-diff")
            .w_full()
            .max_h(px(340.0))
            .overflow_y_scroll()
            .rounded(px(4.0))
            .border_1()
            .border_color(design.border)
            .bg(design.background)
            .py_1()
            .font_family(design.mono.clone())
            .text_size(px(design.tiny()));
        for hunk in &preview.diff.hunks {
            let header = format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
            );
            diff = diff.child(
                div()
                    .px_2()
                    .bg(design.field)
                    .text_color(design.muted)
                    .child(header),
            );
            for line in &hunk.lines {
                let (sign, background) = match line.kind {
                    LineKind::Context => (' ', gpui::Hsla::transparent_black()),
                    LineKind::Added => ('+', design.diff_added_bg),
                    LineKind::Removed => ('-', design.diff_removed_bg),
                };
                diff = diff.child(
                    div()
                        .px_2()
                        .bg(background)
                        .child(format!("{sign}{}", line.content)),
                );
            }
        }
        body.child(diff).into_any_element()
    }
}

fn error_text(error: strata_core::Error) -> String {
    match error {
        strata_core::Error::GitCommand { stderr, .. } if !stderr.trim().is_empty() => stderr,
        other => other.to_string(),
    }
}

fn label(text: &'static str, design: &Design) -> impl IntoElement {
    div()
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(design.muted)
        .child(text)
}

/// A labelled text field, the same two-line shape the clone dialogs draw.
fn field(text: &'static str, input: &Entity<InputState>, design: &Design) -> impl IntoElement {
    v_flex()
        .gap_1()
        .w_full()
        .child(label(text, design))
        .child(Input::new(input).w_full().bg(design.field))
}

impl Render for ConfigEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        div()
            .w_full()
            .text_size(px(design.ui))
            .text_color(design.text)
            .child(match self.preview {
                Some(_) => self.preview_body(&design),
                None => self.form(&design, cx),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::repo::{ConfigOrigin, well_known_key};

    fn entry(key: &str, value: &str, layer: ConfigLayer) -> ConfigEntry {
        ConfigEntry {
            key: key.to_owned(),
            values: vec![ConfigValue {
                value: value.to_owned(),
                layer,
                origin: ConfigOrigin {
                    path: Some(PathBuf::from("/tmp/config")),
                    line: Some(3),
                    include: None,
                },
            }],
            overridden: Vec::new(),
            multivalued: false,
            well_known: well_known_key(key),
            warning: None,
        }
    }

    fn config(entries: Vec<ConfigEntry>) -> GitConfig {
        GitConfig {
            entries,
            includes: Vec::new(),
            identity: Default::default(),
            files: Default::default(),
        }
    }

    #[test]
    fn the_well_known_toggle_hides_uncatalogued_keys() {
        let config = config(vec![
            entry("user.name", "Ada", ConfigLayer::Global),
            entry("branch.main.remote", "origin", ConfigLayer::Local),
        ]);
        let curated = visible_entries(&config, "", false);
        assert_eq!(curated.len(), 1);
        assert_eq!(curated[0].key, "user.name");

        let all = visible_entries(&config, "", true);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn search_matches_keys_values_layers_and_paths() {
        let config = config(vec![
            entry("user.name", "Ada Lovelace", ConfigLayer::Global),
            entry("core.editor", "vim", ConfigLayer::Local),
        ]);
        assert_eq!(visible_entries(&config, "editor", true).len(), 1);
        assert_eq!(visible_entries(&config, "lovelace", true).len(), 1);
        assert_eq!(visible_entries(&config, "local", true).len(), 1);
        assert_eq!(visible_entries(&config, "tmp/config", true).len(), 2);
        assert!(visible_entries(&config, "missing", true).is_empty());
    }

    #[test]
    fn search_ignores_case_and_blank_space() {
        let config = config(vec![entry(
            "user.email",
            "dev@example.com",
            ConfigLayer::Local,
        )]);
        assert_eq!(
            visible_entries(&config, "  DEV@EXAMPLE.COM  ", true).len(),
            1
        );
        assert_eq!(visible_entries(&config, "   ", false).len(), 1);
    }

    #[test]
    fn the_summary_and_description_are_searchable() {
        let config = config(vec![entry("pull.rebase", "true", ConfigLayer::Local)]);
        assert_eq!(visible_entries(&config, "git pull", true).len(), 1);
        assert_eq!(visible_entries(&config, "pull.rebase", true).len(), 1);
    }

    fn value(path: &str, layer: ConfigLayer, include: Option<usize>) -> ConfigValue {
        ConfigValue {
            value: "value".to_owned(),
            layer,
            origin: ConfigOrigin {
                path: Some(PathBuf::from(path)),
                line: Some(1),
                include,
            },
        }
    }

    fn with_files() -> GitConfig {
        GitConfig {
            files: ConfigFilePaths {
                local: PathBuf::from("/repo/.git/config"),
                worktree: Some(PathBuf::from("/repo/.git/config.worktree")),
                global: Some(PathBuf::from("/home/user/.gitconfig")),
                system: Some(PathBuf::from("/etc/gitconfig")),
            },
            ..config(Vec::new())
        }
    }

    #[test]
    fn a_new_key_starts_with_a_local_write() {
        let (choices, default) = target_choices(&with_files(), None);
        assert_eq!(choices[default].target.layer, ConfigLayer::Local);
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.label)
                .collect::<Vec<_>>(),
            ["local", "worktree", "global", "system"]
        );
        let system = choices
            .iter()
            .find(|choice| choice.target.layer == ConfigLayer::System)
            .expect("system is shown");
        assert!(system.disabled, "system is never writable");
        assert!(system.detail.contains("administrator"));
    }

    #[test]
    fn an_entry_defaults_to_the_file_its_effective_value_came_from() {
        let current = value("/home/user/.gitconfig", ConfigLayer::Global, None);
        let (choices, default) = target_choices(&with_files(), Some(&current));
        assert_eq!(
            choices[default].target.path,
            PathBuf::from("/home/user/.gitconfig")
        );
        assert_eq!(choices[default].target.layer, ConfigLayer::Global);
    }

    #[test]
    fn an_include_file_the_entry_came_from_is_a_choice() {
        let mut config = with_files();
        config.includes = vec![ConfigInclude {
            condition: "gitdir:/repo/.git/".to_owned(),
            condition_kind: strata_core::repo::IncludeConditionKind::GitDir,
            path: Some(PathBuf::from("/home/user/work.gitconfig")),
            layer: ConfigLayer::Global,
            origin: ConfigOrigin::default(),
            matched: true,
            values: 2,
            error: None,
        }];
        let current = value("/home/user/work.gitconfig", ConfigLayer::Global, Some(0));

        let (choices, default) = target_choices(&config, Some(&current));

        assert_eq!(
            choices[default].target.path,
            PathBuf::from("/home/user/work.gitconfig")
        );
        let include = choices
            .iter()
            .find(|choice| choice.label == "include file")
            .expect("the include file is offered");
        assert!(include.detail.contains("gitdir:/repo/.git/"));
    }

    #[test]
    fn a_system_value_shows_system_but_defaults_to_a_file_it_can_write() {
        let current = value("/etc/gitconfig", ConfigLayer::System, None);
        let (choices, default) = target_choices(&with_files(), Some(&current));
        assert_eq!(choices[default].target.layer, ConfigLayer::Local);
        let system = choices
            .iter()
            .find(|choice| choice.target.layer == ConfigLayer::System)
            .expect("system is offered");
        assert!(system.disabled);
    }
}
