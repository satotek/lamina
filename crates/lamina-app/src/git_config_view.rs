//! The Git config window: every effective value, where it came from, what it
//! overrode, and the editor that writes one back.
//!
//! `lamina-core`'s `repo::config` module reads the files on a background
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
    WindowDecorations, WindowOptions, actions, div, px,
};
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme as _, Selectable as _, TitleBar, h_flex, v_flex};
use lamina_core::diff::LineKind;
use lamina_core::git_cli::{
    CommandLog, ConfigChange, ConfigPreview, ConfigTarget, ConfigWrite, Git, GitConfigAccess,
};
use lamina_core::repo::{
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

actions!(lamina_git_config, [CloseGitConfig]);

const WIDTH: f32 = 920.0;
const HEIGHT: f32 = 680.0;
const MIN_WIDTH: f32 = 640.0;
const MIN_HEIGHT: f32 = 480.0;

/// Where a configuration read or write runs.
///
/// A local tab runs `git config` itself. A remote tab hands the same command
/// to the helper, so this window never touches a path it cannot see. The label
/// is what the window shows for the repository.
#[derive(Clone)]
pub(crate) struct ConfigSource {
    label: String,
    /// A remote source is tied to the session that published it, so a
    /// reconnect has to reload even though the printed location is the same.
    remote: bool,
    access: Arc<dyn GitConfigAccess>,
}

impl ConfigSource {
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
}

/// A local checkout's configuration, read and written by this machine's `git`.
struct LocalConfig {
    repository: PathBuf,
    binary: Option<PathBuf>,
    /// The session's log, so the save shows up beside every other command.
    /// The preview is left out: it writes a temporary file the user never asked
    /// to change.
    log: Option<Arc<CommandLog>>,
}

impl LocalConfig {
    fn git(&self, with_log: bool) -> Git {
        let mut git = Git::new(self.repository.clone());
        if let Some(binary) = &self.binary {
            git = git.with_git_binary(binary.clone());
        }
        if with_log && let Some(log) = &self.log {
            git = git.with_log(Arc::clone(log));
        }
        git
    }
}

impl GitConfigAccess for LocalConfig {
    fn load_config(&self) -> lamina_core::Result<GitConfig> {
        GitConfig::load(&self.repository)
    }

    fn preview_config(&self, write: &ConfigWrite) -> lamina_core::Result<ConfigPreview> {
        self.git(false).config_preview(write)
    }

    fn apply_config(&self, preview: &ConfigPreview) -> lamina_core::Result<()> {
        self.git(true).config_apply(preview)
    }
}

/// The repository whose configuration the window opens. Updated by the active
/// [`crate::repo_view::RepoView`]; the module is careful to do nothing when no
/// repository has been opened yet.
#[derive(Default)]
struct CurrentRepository {
    source: Option<ConfigSource>,
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

/// Records the local repository the next config command should open, and the
/// log a write is recorded in.
pub(crate) fn set_repository(path: PathBuf, command_log: Arc<CommandLog>, cx: &mut App) {
    set_source(
        ConfigSource {
            label: path.display().to_string(),
            remote: false,
            access: Arc::new(LocalConfig {
                repository: path,
                binary: Settings::get(cx).behaviour.git_executable.clone(),
                log: Some(command_log),
            }),
        },
        cx,
    );
}

/// Records a remote tab's configuration, reached through its session.
///
/// The label is the `ssh://` location, so the window says which machine the
/// values come from.
pub(crate) fn set_remote_repository(access: Arc<dyn GitConfigAccess>, label: String, cx: &mut App) {
    set_source(
        ConfigSource {
            label,
            remote: true,
            access,
        },
        cx,
    );
}

fn set_source(source: ConfigSource, cx: &mut App) {
    if cx.has_global::<CurrentRepository>() {
        cx.global_mut::<CurrentRepository>().source = Some(source);
    } else {
        cx.set_global(CurrentRepository {
            source: Some(source),
        });
    }
}

/// Whether a repository is available, for the settings window's link.
pub(crate) fn has_repository(cx: &App) -> bool {
    cx.try_global::<CurrentRepository>()
        .is_some_and(|repository| repository.source.is_some())
}

/// Forgets the published repository. A remote tab that has not connected has
/// nothing to read, and leaving the previous path set would open that
/// repository's config instead.
///
/// A unit test that never called [`bind`] has no global; that is not an error.
pub(crate) fn clear_repository(cx: &mut App) {
    if !cx.has_global::<CurrentRepository>() {
        return;
    }
    cx.global_mut::<CurrentRepository>().source = None;
}

fn open(cx: &mut App) {
    let Some(source) = cx
        .try_global::<CurrentRepository>()
        .and_then(|repository| repository.source.clone())
    else {
        return;
    };

    if let Some(view) = cx
        .try_global::<OpenConfigWindow>()
        .and_then(|window| window.view.clone())
        && let Some(view) = view.upgrade()
    {
        view.update(cx, |view, cx| view.set_repository(source, cx));
        if let Some(handle) = cx
            .try_global::<OpenConfigWindow>()
            .and_then(|window| window.window)
        {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        cx.activate(true);
        return;
    }

    let bounds = Bounds::centered(None, crate::zoom::window_size(WIDTH, HEIGHT, cx), cx);
    let view_slot: Rc<RefCell<Option<Entity<GitConfigView>>>> = Rc::new(RefCell::new(None));
    let slot = view_slot.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(crate::style::window_titlebar(
                t!("git_config.window_title").to_string(),
            )),
            window_decorations: Some(WindowDecorations::Client),
            window_min_size: Some(crate::zoom::window_size(MIN_WIDTH, MIN_HEIGHT, cx)),
            focus: true,
            ..TitleBar::window_options()
        },
        move |window, cx| {
            settings::apply_theme(Some(window), cx);
            let view = cx.new(|cx| GitConfigView::new(source, window, cx));
            *slot.borrow_mut() = Some(view.clone());
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        },
    );

    match opened {
        Ok(handle) => {
            crate::window_registry::track(handle.window_id(), cx);
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
    /// Where the values come from and where a write goes.
    source: ConfigSource,
    config: Option<GitConfig>,
    error: Option<String>,
    loading: bool,
    /// Bumped by every load. A read that finishes after a newer one started
    /// belongs to the previous repository and must not replace its values.
    load_generation: u64,
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
    /// The appearance last applied, so unrelated setting changes do not reload
    /// the palette.
    appearance: crate::settings::Appearance,
    /// Repaints this window when another one changes the theme.
    _settings: Subscription,
    _appearance: Subscription,
    _input: Subscription,
}

impl GitConfigView {
    fn new(source: ConfigSource, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let input = cx
            .new(|cx| InputState::new(window, cx).placeholder(t!("git_config.filter").to_string()));
        let input_events = cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.query = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        let appearance =
            window.observe_window_appearance(|window, cx| settings::apply_theme(Some(window), cx));
        let current_appearance = Settings::get(cx).appearance.clone();
        let settings = cx.observe_global::<Settings>(|this, cx| {
            let next = Settings::get(cx).appearance.clone();
            if next != this.appearance {
                this.appearance = next;
                settings::apply_theme(None, cx);
            }
            cx.notify();
        });
        let mut view = Self {
            source,
            config: None,
            error: None,
            loading: true,
            load_generation: 0,
            all_keys: false,
            query: String::new(),
            input,
            expanded: HashSet::new(),
            selected: None,
            last_written: None,
            focus,
            appearance: current_appearance,
            _settings: settings,
            _appearance: appearance,
            _input: input_events,
        };
        view.load(cx);
        view
    }

    fn set_repository(&mut self, source: ConfigSource, cx: &mut Context<Self>) {
        let same_target = self.source.remote == source.remote
            && if source.remote {
                Arc::ptr_eq(&self.source.access, &source.access)
            } else {
                self.source.label() == source.label()
            };
        // Reopening a repository keeps its path but starts a new session, and
        // with it a new command log. The access always follows the latest
        // publish so a save is recorded in the live log, while only a
        // different read target makes the loaded values stale.
        let reload = !same_target || (self.config.is_none() && !self.loading);
        self.source = source;
        if !reload {
            return;
        }
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
        self.load_generation += 1;
        let generation = self.load_generation;
        let access = Arc::clone(&self.source.access);
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { access.load_config().map_err(|error| error.to_string()) })
                .await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
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
        let well_known = lamina_core::repo::well_known_key(&key);
        let view = cx.entity().downgrade();
        let dialog = cx.new(|cx| {
            ConfigEditor::new(
                cx,
                view,
                EditorRequest {
                    source: self.source.clone(),
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
            let zoom = Settings::get(cx).appearance.zoom();
            card.title(title)
                .w(px(760.0 * zoom))
                .content(move |body, _, _| body.child(content.clone()))
                .footer(
                    DialogFooter::new()
                        .child(
                            ui::button("config-editor-cancel", t!("git_config.cancel").to_string())
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
                                ui::button(
                                    "config-editor-write",
                                    t!("git_config.write").to_string(),
                                )
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
                                ui::button(
                                    "config-editor-preview",
                                    t!("git_config.preview_write").to_string(),
                                )
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
            .h(design.px(crate::style::TITLE_BAR_HEIGHT))
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
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("git_config.title").to_string()),
                    ),
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
                    .child(t!("git_config.identity").to_string()),
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
                            t!("git_config.set_local_email").to_string(),
                        )
                        .size(ui::ButtonSize::Tiny)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_editor(
                                    "user.email".to_owned(),
                                    ConfigChange::Set(value.value.clone()),
                                    false,
                                    Some(ConfigLayer::Local),
                                    window,
                                    cx,
                                );
                            },
                        )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(
                                t!("git_config.local_email_note", source = value.origin.label())
                                    .to_string(),
                            ),
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
                ui::button("config-add-key", t!("git_config.add_key").to_string())
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
                            .child(t!("git_config.all_keys").to_string()),
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
        .h(design.px(30.0))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| this.toggle(&key, cx)))
        .child(
            div()
                .flex_none()
                .w(design.px(14.0))
                .text_size(design.px(10.0))
                .text_color(design.muted)
                .child(if expanded { "▾" } else { "▸" }),
        )
        .child(
            div()
                .flex_none()
                .w(design.px(280.0))
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
                t!(
                    &crate::i18n::count_key("git_config.values_count", entry.values.len()),
                    count = entry.values.len()
                )
                .to_string(),
                BadgeTone::Neutral,
                design,
            ));
        }
        if entry.warning.is_some() {
            row = row.child(crate::ui::status_badge(
                t!("git_config.invalid").to_string(),
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
        let mut details = v_flex().pl(design.px(34.0)).pr_3().pb_2().gap_1();
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
            .pl(design.px(150.0))
            .child(
                ui::button(
                    format!("config-edit-{}", entry.key),
                    t!("git_config.edit_value").to_string(),
                )
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
                ui::button(
                    format!("config-unset-{}", entry.key),
                    t!("git_config.unset").to_string(),
                )
                .kind(ui::ButtonKind::Normal)
                .size(ui::ButtonSize::Tiny)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_editor(key.clone(), ConfigChange::Unset, false, None, window, cx);
                })),
            );
        if entry.multivalued {
            let key = entry.key.clone();
            actions = actions.child(
                ui::button(
                    format!("config-add-value-{}", entry.key),
                    t!("git_config.add_value").to_string(),
                )
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
            let mut row = value_detail(
                format!("{}:{index}", entry.key),
                &t!("git_config.effective"),
                value,
                design,
            );
            if entry.multivalued {
                let key = entry.key.clone();
                let removed = value.value.clone();
                row = row.child(
                    ui::button(
                        format!("config-remove-{}-{index}", entry.key),
                        t!("git_config.remove").to_string(),
                    )
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
                &t!("git_config.overridden_by", by = overridden_by),
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
                    .child(t!("git_config.includes", count = config.includes.len()).to_string()),
            );
        if config.includes.is_empty() {
            section = section.child(
                div()
                    .px_3()
                    .pb_2()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(t!("git_config.no_includes").to_string()),
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
    let mut row = list_row(id, RowState::NORMAL, design)
        .h(design.px(26.0))
        .child(
            div()
                .flex_none()
                .w(design.px(96.0))
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
                        .max_w(design.px(300.0))
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
                    .child(t!("git_config.not_set").to_string()),
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
                .w(design.px(150.0))
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
                .max_w(design.px(240.0))
                .truncate()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(value.origin.label()),
        )
        .child(
            crate::ui::button(
                format!("reveal-{id}"),
                t!("git_config.reveal_file").to_string(),
            )
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
        t!("git_config.matched").to_string()
    } else {
        t!("git_config.not_matched").to_string()
    };
    let mut row = list_row(("config-include", index), RowState::NORMAL, design)
        .h(design.px(30.0))
        .child(crate::ui::status_badge(status, tone, design))
        .child(
            div()
                .flex_none()
                .w(design.px(330.0))
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
                        .unwrap_or_else(|| t!("git_config.unresolved").to_string()),
                ),
        )
        .child(layer_badge(include.layer, design));
    if include.matched {
        row = row.child(
            div()
                .flex_none()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(
                    t!(
                        &crate::i18n::count_key("git_config.values_count", include.values),
                        count = include.values
                    )
                    .to_string(),
                ),
        );
    }
    if let Some(error) = &include.error {
        row = row
            .child(crate::ui::status_badge(
                t!("git_config.error").to_string(),
                BadgeTone::Danger,
                design,
            ))
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
    crate::ui::status_badge(layer_label(layer), tone, design)
}

/// The badge text for a layer. `ConfigLayer::label` lives in the core crate,
/// which has no catalogue of its own, so the app maps it here.
fn layer_label(layer: ConfigLayer) -> String {
    match layer {
        ConfigLayer::System => t!("git_config.layer.system"),
        ConfigLayer::Xdg => t!("git_config.layer.xdg"),
        ConfigLayer::Global => t!("git_config.layer.global"),
        ConfigLayer::Local => t!("git_config.layer.local"),
        ConfigLayer::Worktree => t!("git_config.layer.worktree"),
        ConfigLayer::Environment => t!("git_config.layer.environment"),
        ConfigLayer::CommandLine => t!("git_config.layer.command_line"),
    }
    .to_string()
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
            crate::ui::message(t!("git_config.reading").to_string(), design.muted, &design)
                .into_any_element()
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
                            t!("git_config.no_match").to_string()
                        } else {
                            t!("git_config.no_well_known").to_string()
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

        let mut root = v_flex()
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
                    .h(design.px(36.0))
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
                                    .unwrap_or_else(|| self.source.label().to_owned()),
                            ),
                    )
                    .child(
                        div().flex_none().child(
                            t!(
                                &crate::i18n::count_key("git_config.keys_count", entries.len()),
                                count = entries.len()
                            )
                            .to_string(),
                        ),
                    ),
            );
        root.text_style().font_fallbacks = Some(crate::style::font_fallbacks());
        root
    }
}

/// One file the editor can write, with the reason when it cannot.
struct TargetChoice {
    target: ConfigTarget,
    label: String,
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
            t!(
                "git_config.layer.worktree_detail",
                path = worktree.display()
            )
            .to_string(),
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
                t!("git_config.layer.system").to_string(),
                t!("git_config.layer.system_detail", path = path.display()).to_string(),
            )
        } else if value.origin.include.is_some() {
            let condition = value
                .origin
                .include
                .and_then(|index| config.includes.get(index))
                .map(|include| include.condition.clone())
                .unwrap_or_else(|| "include".to_owned());
            (
                t!("git_config.include_file").to_string(),
                t!(
                    "git_config.include_detail",
                    condition = condition,
                    path = path.display()
                )
                .to_string(),
            )
        } else {
            (layer_label(value.layer), path.display().to_string())
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
            t!("git_config.layer.system_detail", path = system.display()).to_string(),
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
    label: impl Into<String>,
    detail: String,
) {
    if !seen.insert(target.path.clone()) {
        return;
    }
    let disabled = !target.writable();
    choices.push(TargetChoice {
        target,
        label: label.into(),
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
    source: ConfigSource,
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
    source: ConfigSource,
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
    /// A preview or a save is running. Both can cross a machine, so neither
    /// may hold the window while it waits.
    busy: bool,
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
            source,
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
                .placeholder(t!("git_config.key_placeholder").to_string())
                .default_value(key.clone())
        });
        let value_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("git_config.value_placeholder").to_string())
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
            source,
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
            busy: false,
            _inputs: vec![key_events, value_events],
        }
    }

    fn title(&self) -> String {
        let key = if self.new_key && self.key.trim().is_empty() {
            t!("git_config.new_key").to_string()
        } else {
            self.key.clone()
        };
        match self.kind {
            WriteKind::Set if self.new_key => t!("git_config.add_key_title", key = key).to_string(),
            WriteKind::Set => t!("git_config.edit_key_title", key = key).to_string(),
            WriteKind::Add => t!("git_config.add_value_title", key = key).to_string(),
            WriteKind::Unset => t!("git_config.unset_title", key = key).to_string(),
            WriteKind::UnsetValue => t!("git_config.remove_value_title", key = key).to_string(),
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
            lamina_core::repo::well_known_key(self.key.trim())
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
    fn write(&self) -> lamina_core::Result<ConfigWrite> {
        let Some(choice) = self.targets.get(self.target) else {
            return Err(lamina_core::Error::other(
                t!("git_config.no_target").to_string(),
            ));
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
        !self.busy && self.validation_error().is_none() && !self.targets.is_empty()
    }

    /// Run the command against a copy. The source runs it, so a remote tab's
    /// preview happens on the host and only the texts come back.
    fn preview(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
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
        let access = Arc::clone(&self.source.access);
        self.busy = true;
        cx.spawn(async move |this, cx| {
            let preview = cx
                .background_executor()
                .spawn(async move { access.preview_config(&write) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match preview {
                    Ok(preview) => this.preview = Some(preview),
                    Err(error) => this.error = Some(error_text(error)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn back_to_form(&mut self, cx: &mut Context<Self>) {
        self.preview = None;
        self.error = None;
        cx.notify();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(preview) = self.preview.clone() else {
            return;
        };
        let access = Arc::clone(&self.source.access);
        let handle = window.window_handle();
        let write = preview.clone();
        self.busy = true;
        cx.spawn(async move |this, cx| {
            let applied = cx
                .background_executor()
                .spawn(async move { access.apply_config(&write) })
                .await;
            match applied {
                Ok(()) => {
                    let command = preview.command.clone();
                    let key = preview.write.key.clone();
                    this.update(cx, |this, cx| {
                        this.busy = false;
                        this.view
                            .update(cx, |view, cx| view.after_write(command, key, cx))
                            .ok();
                        cx.notify();
                    })
                    .ok();
                    // The dialog belongs to a window this task does not hold;
                    // its handle closes it once the write is done.
                    let _ = handle.update(cx, |_, window, cx| window.close_dialog(cx));
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.busy = false;
                        this.preview = None;
                        this.error = Some(
                            t!("git_config.not_changed_again", reason = error_text(error))
                                .to_string(),
                        );
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn form(&self, design: &Design, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut form = v_flex().gap_3().w_full();
        if let Some(error) = &self.error {
            form = form.child(ui::banner(ui::BannerKind::Warning, error.clone(), design));
        }

        if self.new_key {
            form = form.child(field(
                t!("git_config.key_placeholder").to_string(),
                &self.key_input,
                design,
            ));
        }

        let mut chooser = h_flex().flex_wrap().gap_2();
        for (index, choice) in self.targets.iter().enumerate() {
            let selected = self.target == index;
            chooser = chooser.child(
                ui::button(format!("config-target-{index}"), choice.label.clone())
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
            .child(label(t!("git_config.target_file").to_string(), design))
            .child(chooser)
            .child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(
                        self.targets
                            .get(self.target)
                            .map(|choice| choice.detail.clone())
                            .unwrap_or_else(|| t!("git_config.no_writable_file").to_string()),
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
            form = form.child(label(t!("git_config.value").to_string(), design));
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
                            .child(t!("git_config.not_accepted", value = self.value).to_string()),
                    );
                }
            }
        }

        if let Some(error) = self.validation_error() {
            form = form.child(ui::banner(
                ui::BannerKind::Warning,
                t!("git_config.cannot_save", error = error).to_string(),
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
                    .child(t!("git_config.empty_value").to_string()),
            );
        }

        form.into_any_element()
    }

    fn preview_body(&self, design: &Design) -> gpui::AnyElement {
        let Some(preview) = &self.preview else {
            return v_flex()
                .child(t!("git_config.no_preview").to_string())
                .into_any_element();
        };
        let mut body = v_flex().gap_2().w_full();
        body = body
            .child(label(t!("git_config.file").to_string(), design))
            .child(
                div()
                    .font_family(design.mono.clone())
                    .text_size(px(design.small()))
                    .child(preview.write.target.path.display().to_string()),
            )
            .child(label(t!("git_config.command").to_string(), design))
            .child(
                div()
                    .w_full()
                    .rounded(design.px(4.0))
                    .bg(design.field)
                    .p_2()
                    .font_family(design.mono.clone())
                    .text_size(px(design.tiny()))
                    .child(preview.command.clone()),
            )
            .child(label(t!("git_config.changes").to_string(), design));

        if !preview.changed() {
            body = body.child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child(t!("git_config.no_change").to_string()),
            );
            return body.into_any_element();
        }

        let mut diff = v_flex()
            .id("config-preview-diff")
            .w_full()
            .max_h(design.px(340.0))
            .overflow_y_scroll()
            .rounded(design.px(4.0))
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

fn error_text(error: lamina_core::Error) -> String {
    match error {
        lamina_core::Error::GitCommand { stderr, .. } if !stderr.trim().is_empty() => stderr,
        other => other.to_string(),
    }
}

fn label(text: impl Into<gpui::SharedString>, design: &Design) -> impl IntoElement {
    div()
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(design.muted)
        .child(text.into())
}

/// A labelled text field, the same two-line shape the clone dialogs draw.
fn field(
    text: impl Into<gpui::SharedString>,
    input: &Entity<InputState>,
    design: &Design,
) -> impl IntoElement {
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use lamina_core::repo::{ConfigOrigin, well_known_key};

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
                .map(|choice| choice.label.clone())
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
            condition_kind: lamina_core::repo::IncludeConditionKind::GitDir,
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

    /// A source that reads an empty configuration. The test only checks which
    /// session the window ends up reading and writing through.
    struct FakeAccess {
        loads: Arc<AtomicUsize>,
    }

    impl GitConfigAccess for FakeAccess {
        fn load_config(&self) -> lamina_core::Result<GitConfig> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(config(Vec::new()))
        }

        fn preview_config(&self, _: &ConfigWrite) -> lamina_core::Result<ConfigPreview> {
            Err(lamina_core::Error::other("the test never previews"))
        }

        fn apply_config(&self, _: &ConfigPreview) -> lamina_core::Result<()> {
            Ok(())
        }
    }

    fn fake_access(loads: &Arc<AtomicUsize>) -> Arc<dyn GitConfigAccess> {
        Arc::new(FakeAccess {
            loads: Arc::clone(loads),
        })
    }

    fn local_source(path: &str, access: Arc<dyn GitConfigAccess>) -> ConfigSource {
        ConfigSource {
            label: path.to_owned(),
            remote: false,
            access,
        }
    }

    /// Reads one fixed config, so a test can tell which repository's values
    /// the window ended up showing.
    struct FixedAccess(GitConfig);

    impl GitConfigAccess for FixedAccess {
        fn load_config(&self) -> lamina_core::Result<GitConfig> {
            Ok(self.0.clone())
        }

        fn preview_config(&self, _: &ConfigWrite) -> lamina_core::Result<ConfigPreview> {
            Err(lamina_core::Error::other("the test never previews"))
        }

        fn apply_config(&self, _: &ConfigPreview) -> lamina_core::Result<()> {
            Ok(())
        }
    }

    fn named_access(name: &str) -> Arc<dyn GitConfigAccess> {
        Arc::new(FixedAccess(config(vec![entry(
            "user.name",
            name,
            ConfigLayer::Local,
        )])))
    }

    /// Switching repository while the first read is still in flight starts a
    /// second read. Whichever finishes last used to win, so the window could
    /// show the first repository's values under the second one's label and
    /// save into the wrong file. Several scheduler seeds cover both orders.
    #[gpui::test(iterations = 20)]
    fn a_read_for_the_previous_repository_does_not_replace_the_current_one(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
        });
        let window = cx.add_window(|window, cx| {
            let mut view = GitConfigView::new(local_source("/a", named_access("A")), window, cx);
            view.set_repository(local_source("/b", named_access("B")), cx);
            view
        });
        let view = window.root(cx).expect("the config view");
        let visual_cx = gpui::VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();
        let shown = view.read_with(&visual_cx, |view, _| {
            view.config
                .as_ref()
                .and_then(|config| config.entries.first())
                .and_then(|entry| entry.values.first())
                .map(|value| value.value.clone())
        });
        assert_eq!(shown.as_deref(), Some("B"));
    }

    /// A reopened repository keeps its path but starts a new session, so the
    /// window must take the new access even though its loaded values are still
    /// valid. Keeping the old one sent every save to a closed session's log.
    #[gpui::test]
    fn reopening_the_same_path_takes_the_new_session(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
        });
        let loads = Arc::new(AtomicUsize::new(0));
        let closed = fake_access(&loads);
        let live = fake_access(&loads);
        let window = cx.add_window(|window, cx| {
            GitConfigView::new(local_source("/repo", Arc::clone(&closed)), window, cx)
        });
        let view = window.root(cx).expect("the config view");
        let mut visual_cx = gpui::VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();
        assert!(
            view.read_with(&visual_cx, |view, _| view.config.is_some()),
            "the first load did not land"
        );

        view.update(&mut visual_cx, |view, cx| {
            view.set_repository(local_source("/repo", Arc::clone(&live)), cx);
        });
        assert!(
            view.read_with(&visual_cx, |view, _| Arc::ptr_eq(
                &view.source.access,
                &live
            )),
            "the window kept the closed session's command log"
        );
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "the same path does not need to be read again"
        );
    }
}
