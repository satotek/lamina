//! The Git config window: every effective value, where it came from, and what
//! it overrode.
//!
//! The window is read-only. `strata-core`'s `repo::config` module reads the
//! files on a background worker, so the UI thread only ever filters and draws
//! the result. A repository only exists while a repository view is active, so
//! that view publishes its path to [`CurrentRepository`] and the command
//! palette, the settings window and this module's own action all open the
//! same window for it.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, FocusHandle, FontWeight,
    Global, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled, Subscription, WeakEntity, Window, WindowBounds,
    WindowDecorations, WindowOptions, actions, div, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme as _, TitleBar, h_flex, v_flex};
use strata_core::repo::{ConfigEntry, ConfigInclude, ConfigLayer, ConfigValue, GitConfig};

use crate::actions::OpenGitConfig;
use crate::settings::{self, Settings};
use crate::style::{self, Design};
use crate::ui::{BadgeTone, RowState, list_row};

/// Key context of the config window, so Escape closes this window and no
/// other.
const VIEW: &str = "GitConfigView";

actions!(strata_git_config, [CloseGitConfig]);

const WIDTH: f32 = 920.0;
const HEIGHT: f32 = 680.0;
const MIN_WIDTH: f32 = 640.0;
const MIN_HEIGHT: f32 = 480.0;

/// The repository whose configuration the window opens. Updated by the
/// active [`crate::repo_view::RepoView`]; the module is careful to do nothing
/// when no repository has been opened yet.
#[derive(Default)]
struct CurrentRepository(Option<PathBuf>);

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

/// Records the repository the next config command should open.
pub(crate) fn set_repository(path: PathBuf, cx: &mut App) {
    if cx.has_global::<CurrentRepository>() {
        cx.global_mut::<CurrentRepository>().0 = Some(path);
    } else {
        cx.set_global(CurrentRepository(Some(path)));
    }
}

/// Whether a repository is available, for the settings window's link.
pub(crate) fn has_repository(cx: &App) -> bool {
    cx.try_global::<CurrentRepository>()
        .is_some_and(|repository| repository.0.is_some())
}

fn open(cx: &mut App) {
    let Some(repository) = cx
        .try_global::<CurrentRepository>()
        .and_then(|repository| repository.0.clone())
    else {
        return;
    };

    if let Some(view) = cx
        .try_global::<OpenConfigWindow>()
        .and_then(|window| window.view.clone())
        && let Some(view) = view.upgrade()
    {
        view.update(cx, |view, cx| view.set_repository(repository, cx));
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
            let view = cx.new(|cx| GitConfigView::new(repository, window, cx));
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
    config: Option<GitConfig>,
    error: Option<String>,
    loading: bool,
    /// Off shows the curated keys only; on shows every key.
    all_keys: bool,
    query: String,
    input: Entity<InputState>,
    expanded: HashSet<String>,
    focus: FocusHandle,
    _appearance: Subscription,
    _input: Subscription,
}

impl GitConfigView {
    fn new(repository: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            config: None,
            error: None,
            loading: true,
            all_keys: false,
            query: String::new(),
            input,
            expanded: HashSet::new(),
            focus,
            _appearance: appearance,
            _input: input_events,
        };
        view.load(cx);
        view
    }

    fn set_repository(&mut self, repository: PathBuf, cx: &mut Context<Self>) {
        if self.repository == repository && (self.config.is_some() || self.loading) {
            return;
        }
        self.repository = repository;
        self.config = None;
        self.error = None;
        self.expanded.clear();
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
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_owned());
        }
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

    fn identity(&self, config: &GitConfig, design: &Design) -> impl IntoElement {
        v_flex()
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
            ])
            .pb_1()
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
        let mut row = list_row(
            format!("config-entry-{}", entry.key),
            RowState::NORMAL,
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

        let details = expanded.then(|| self.entry_details(entry, design));
        v_flex()
            .flex_none()
            .child(row)
            .children(details)
            .into_any_element()
    }

    fn entry_details(&self, entry: &ConfigEntry, design: &Design) -> impl IntoElement {
        let mut details = v_flex().pl(px(34.0)).pr_3().pb_2().gap_1();
        if let Some(well_known) = entry.well_known {
            details = details.child(
                div()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(well_known.summary),
            );
        }
        for (index, value) in entry.values.iter().enumerate() {
            details = details.child(value_detail(
                format!("{}:{index}", entry.key),
                "Effective",
                value,
                design,
            ));
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
            details = details.child(crate::ui::banner(
                crate::ui::BannerKind::Warning,
                warning.clone(),
                design,
            ));
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

fn value_detail(id: String, label: &str, value: &ConfigValue, design: &Design) -> impl IntoElement {
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
                .max_w(px(260.0))
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
            .map(|config| self.identity(config, &design));

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
                            .child(self.repository.display().to_string()),
                    )
                    .child(div().flex_none().child(format!("{} keys", entries.len()))),
            )
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
            well_known: well_known_key(key),
            warning: None,
        }
    }

    fn config(entries: Vec<ConfigEntry>) -> GitConfig {
        GitConfig {
            entries,
            includes: Vec::new(),
            identity: Default::default(),
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
}
