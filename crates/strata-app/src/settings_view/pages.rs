//! The six settings pages, built from [`SettingField`]s that read and write the
//! [`Settings`] global directly.
//!
//! Every control writes through [`settings::update`], so there is no Save
//! button: the value is stored, saved in the background and the open windows
//! re-render on the same click. The rules for each value (clamping, blank
//! meaning "default", the offered fetch intervals) live in
//! [`super::change`].

use std::path::Path;

use gpui::{
    App, AppContext as _, Entity, IntoElement, ParentElement, PathPromptOptions, SharedString,
    StyleRefinement, Styled, Subscription, Window, div, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::setting::{
    NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage,
};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit_assets::IconName;

use super::change::{self, Section};
use crate::settings::{self, DateFormat, DiffLayout, Settings, ThemeMode};

/// Font families offered beside the free-text font fields.
const UI_FONTS: &[&str] = &["Inter", "SF Pro Text", "Segoe UI"];
const CODE_FONTS: &[&str] = &["JetBrains Mono", "Cascadia Code", "Menlo", "Consolas"];

/// Width every control on the right of a settings row shares, so the right
/// edge lines up down a page and across pages.
const CONTROL_WIDTH: f32 = 260.0;

/// The icon the sidebar shows beside a section, matching the main sidebar's
/// use of one per list. Every name here is in the asset bundle the app embeds,
/// so a section cannot end up with a blank space where its icon belongs.
fn icon(section: Section) -> IconName {
    match section {
        Section::Appearance => IconName::Palette,
        Section::Diff => IconName::Replace,
        Section::History => IconName::FileText,
        Section::Behaviour => IconName::Settings2,
        Section::Tools => IconName::SquareTerminal,
        Section::Privacy => IconName::EyeOff,
    }
}

/// Every page, in the order the sidebar lists them.
pub(super) fn pages() -> Vec<SettingPage> {
    Section::ALL.into_iter().map(page).collect()
}

fn page(section: Section) -> SettingPage {
    match section {
        Section::Appearance => appearance(),
        Section::Diff => diff(),
        Section::History => history(),
        Section::Behaviour => behaviour(),
        Section::Tools => tools(),
        Section::Privacy => privacy(),
    }
}

fn appearance() -> SettingPage {
    shell(Section::Appearance)
        .description("Preferences for Strata itself. Nothing here writes git config.")
        .group(SettingGroup::new().title("Theme").item(
            SettingItem::new("Theme", theme_field())
                .description("Follow the system, or pin light or dark. Every window follows immediately."),
        ))
        .group(
            SettingGroup::new()
                .title("Fonts")
                .items([
                    SettingItem::new(
                        "Interface font",
                        text_field(
                            "System default",
                            UI_FONTS,
                            |settings| settings.appearance.ui_font.clone(),
                            change::ui_font,
                        ),
                    )
                    .description("Lists, labels and the status bar. Leave empty for the platform font."),
                    SettingItem::new(
                        "Interface size",
                        number(
                            change::FONT_SIZE,
                            0.5,
                            |settings| f64::from(settings.appearance.ui_font_size),
                            change::ui_font_size,
                        ),
                    ),
                    SettingItem::new(
                        "Code font",
                        text_field(
                            "Bundled monospace",
                            CODE_FONTS,
                            |settings| settings.appearance.code_font.clone(),
                            change::code_font,
                        ),
                    )
                    .description("Diffs, object ids and git commands. Leave empty for the bundled monospace face."),
                    SettingItem::new(
                        "Code size",
                        number(
                            change::FONT_SIZE,
                            0.5,
                            |settings| f64::from(settings.appearance.code_font_size),
                            change::code_font_size,
                        ),
                    ),
                    SettingItem::new(
                        "Tab width",
                        number(
                            change::TAB_WIDTH,
                            1.0,
                            |settings| f64::from(settings.appearance.tab_width),
                            change::tab_width,
                        ),
                    )
                    .description("Columns a tab expands to in a diff."),
                ]),
        )
}

fn diff() -> SettingPage {
    shell(Section::Diff)
        .description("How a diff is drawn, in every pane that shows one.")
        .group(
            SettingGroup::new().title("Layout").items([
                SettingItem::new(
                    "Layout",
                    choice(
                        &[("unified", "Unified"), ("split", "Side by side")],
                        |settings| match settings.diff.layout {
                            DiffLayout::Unified => "unified".to_owned(),
                            DiffLayout::Split => "split".to_owned(),
                        },
                        |settings, value| {
                            let layout = match value {
                                "split" => DiffLayout::Split,
                                _ => DiffLayout::Unified,
                            };
                            change::diff_layout(settings, layout);
                        },
                    ),
                )
                .description(
                    "Side by side shows the old file in the left column and the new one beside it.",
                ),
                SettingItem::new(
                    "Wrap long lines",
                    toggle(|settings| settings.diff.wrap_lines, change::wrap_lines),
                )
                .description("Long lines wrap onto the next line instead of running off the pane."),
            ]),
        )
        .group(
            SettingGroup::new().title("Text").items([
                SettingItem::new(
                    "Syntax highlighting",
                    toggle(
                        |settings| settings.diff.syntax_highlighting,
                        change::syntax_highlighting,
                    ),
                )
                .description(
                    "Colour keywords, strings, comments and the rest. Off draws plain text.",
                ),
                SettingItem::new(
                    "Show whitespace",
                    toggle(
                        |settings| settings.diff.show_whitespace,
                        change::show_whitespace,
                    ),
                )
                .description("Spaces as · and tabs as →, drawn dimmed."),
            ]),
        )
        .group(
            SettingGroup::new()
                .title("Comparison")
                .description("Applied to every history diff. The staging diff keeps git's default context and whitespace, because staging picks real file lines.")
                .items([
                    SettingItem::new(
                        "Ignore whitespace",
                        toggle(
                            |settings| settings.diff.ignore_whitespace,
                            change::ignore_whitespace,
                        ),
                    )
                    .description("Compare lines without their whitespace, as git diff -w."),
                    SettingItem::new(
                        "Context lines",
                        number(
                            change::CONTEXT_LINES,
                            1.0,
                            |settings| f64::from(settings.diff.context_lines),
                            change::context_lines,
                        ),
                    )
                    .description("Unchanged lines shown around each change (git's default is 3)."),
                ]),
        )
}

fn history() -> SettingPage {
    shell(Section::History)
        .description("What the commit list and the sidebar show.")
        .group(
            SettingGroup::new().title("Refs").items([
                SettingItem::new(
                    "Remote branches",
                    toggle(
                        |settings| settings.history.show_remote_branches,
                        change::show_remote_branches,
                    ),
                )
                .description("Badge remote-tracking branches and list them in the sidebar."),
                SettingItem::new(
                    "Tags",
                    toggle(|settings| settings.history.show_tags, change::show_tags),
                )
                .description("Badge tagged commits and list tags in the sidebar."),
                SettingItem::new(
                    "Stashes",
                    toggle(
                        |settings| settings.history.show_stashes,
                        change::show_stashes,
                    ),
                )
                .description("List the stash in the sidebar."),
            ]),
        )
        .group(
            SettingGroup::new().title("Dates").item(
                SettingItem::new(
                    "Date format",
                    choice(
                        &[("relative", "Relative"), ("absolute", "Absolute")],
                        |settings| match settings.history.date_format {
                            DateFormat::Relative => "relative".to_owned(),
                            DateFormat::Absolute => "absolute".to_owned(),
                        },
                        |settings, value| {
                            let format = match value {
                                "absolute" => DateFormat::Absolute,
                                _ => DateFormat::Relative,
                            };
                            change::date_format(settings, format);
                        },
                    ),
                )
                .description("Relative says \"2 hours ago\"; absolute says 2026-11-13 14:05."),
            ),
        )
}

fn behaviour() -> SettingPage {
    shell(Section::Behaviour)
        .description("Background work and the questions Strata asks before it acts.")
        .group(
            SettingGroup::new().title("Fetching").items([
                SettingItem::new(
                    "Auto-fetch",
                    choice(
                        &[
                            ("0", "Off"),
                            ("5", "Every 5 minutes"),
                            ("10", "Every 10 minutes"),
                            ("15", "Every 15 minutes"),
                            ("30", "Every 30 minutes"),
                            ("60", "Every 60 minutes"),
                        ],
                        |settings| settings.behaviour.auto_fetch_minutes.to_string(),
                        |settings, value| {
                            change::auto_fetch_minutes(
                                settings,
                                value.parse().unwrap_or_default(),
                            );
                        },
                    ),
                )
                .description("Fetch every remote on an interval while a repository is open. Skipped while another operation runs."),
                SettingItem::new(
                    "Prune on fetch",
                    toggle(
                        |settings| settings.behaviour.prune_on_fetch,
                        change::prune_on_fetch,
                    ),
                )
                .description("Delete remote-tracking branches that no longer exist on the remote."),
            ]),
        )
        .group(
            SettingGroup::new().title("Safety").items([
                SettingItem::new(
                    "Confirm destructive actions",
                    toggle(
                        |settings| settings.behaviour.confirm_destructive,
                        change::confirm_destructive,
                    ),
                )
                .description("Ask before discarding changes, deleting branches or force pushing."),
                SettingItem::new(
                    "Summary length hint",
                    number(
                        change::SUMMARY_LENGTH,
                        1.0,
                        |settings| f64::from(settings.behaviour.summary_length_hint),
                        change::summary_length_hint,
                    ),
                )
                .description("Warn once a commit summary is longer than this many characters."),
                SettingItem::new(
                    "Reopen the last repository",
                    toggle(
                        |settings| settings.behaviour.restore_last_repository,
                        change::restore_last_repository,
                    ),
                )
                .description("Open the repositories that were open at quit."),
            ]),
        )
}

fn tools() -> SettingPage {
    shell(Section::Tools)
        .description("Programs Strata runs on your behalf.")
        .group(
            SettingGroup::new().title("Git").item(
                SettingItem::new("Git executable", git_field()).description(
                    "Leave empty to use git from PATH. A changed path applies to repositories opened afterwards; a repository already open keeps the binary it started with.",
                ),
            ),
        )
        .group(
            SettingGroup::new().title("Editor and terminal").items([
                SettingItem::new(
                    "Editor command",
                    text_field(
                        "code --goto {path}:{line}",
                        &[],
                        |settings| settings.tools.editor.clone(),
                        change::editor,
                    ),
                )
                .description(
                    "{path} and {line} are replaced inside each argument, so a path with spaces stays one argument. {line} is empty when the line is unknown. Leave empty to open the file with the system application.",
                ),
                SettingItem::new(
                    "Terminal command",
                    text_field(
                        "alacritty --working-directory {path}",
                        &[],
                        |settings| settings.tools.terminal.clone(),
                        change::terminal,
                    ),
                )
                .description(
                    "{path} is replaced with the repository's working directory, inside one argument. Leave empty to use the system terminal.",
                ),
            ]),
        )
}

fn privacy() -> SettingPage {
    shell(Section::Privacy)
        .description("Only what leaves this machine.")
        .group(SettingGroup::new().title("Network").item(
            SettingItem::new(
                "Author pictures",
                toggle(
                    |settings| settings.privacy.external_avatars,
                    change::external_avatars,
                ),
            )
            .description("Load avatars from an external service. This sends a hash of each author's email over the network, so it is off by default."),
        ))
}

/// The page frame every section starts from: a title, an icon for the sidebar,
/// and a reset button that restores just that section.
fn shell(section: Section) -> SettingPage {
    SettingPage::new(section.title())
        .icon(icon(section))
        .resettable(false)
        // 24 px of padding and an 8 px gap between title, description and the
        // first card: the window's spacing grid.
        .header_style(&StyleRefinement::default().p(px(24.0)).gap(px(8.0)))
        .title_suffix(move |_, _cx| {
            crate::ui::button(
                SharedString::from(format!("reset-{}", section.title().to_lowercase())),
                "Reset section",
            )
            .size(crate::ui::ButtonSize::Tiny)
            .on_click(move |_, _, cx| {
                settings::update(cx, |settings| change::reset_section(settings, section));
                cx.refresh_windows();
            })
        })
}

fn theme_field() -> SettingField<SharedString> {
    SettingField::dropdown(
        pairs(&[
            ("system", "Follow the system"),
            ("light", "Light"),
            ("dark", "Dark"),
        ]),
        |cx| {
            match Settings::get(cx).appearance.theme {
                ThemeMode::System => "system",
                ThemeMode::Light => "light",
                ThemeMode::Dark => "dark",
            }
            .into()
        },
        |value, cx| {
            let theme = match value.as_ref() {
                "light" => ThemeMode::Light,
                "dark" => ThemeMode::Dark,
                _ => ThemeMode::System,
            };
            settings::update(cx, |settings| change::theme(settings, theme));
            // The theme is a gpui-component global, so it is applied rather
            // than only stored; that also refreshes every window.
            settings::apply_theme(None, cx);
        },
    )
    .w(px(CONTROL_WIDTH))
}

/// Value and label pairs for a dropdown.
fn pairs(list: &[(&'static str, &'static str)]) -> Vec<(SharedString, SharedString)> {
    list.iter()
        .map(|(value, label)| (SharedString::from(*value), SharedString::from(*label)))
        .collect()
}

fn toggle(get: fn(&Settings) -> bool, set: fn(&mut Settings, bool)) -> SettingField<bool> {
    SettingField::switch(
        move |cx| get(Settings::get(cx)),
        move |value, cx| {
            settings::update(cx, |settings| set(settings, value));
            cx.refresh_windows();
        },
    )
}

fn choice(
    list: &[(&'static str, &'static str)],
    get: fn(&Settings) -> String,
    set: fn(&mut Settings, &str),
) -> SettingField<SharedString> {
    SettingField::dropdown(
        pairs(list),
        move |cx| SharedString::from(get(Settings::get(cx))),
        move |value, cx| {
            settings::update(cx, |settings| set(settings, &value));
            cx.refresh_windows();
        },
    )
    .w(px(CONTROL_WIDTH))
}

/// A number field whose bounds are the ones [`change`] clamps to, so the two
/// cannot drift apart.
fn number(
    range: std::ops::RangeInclusive<f64>,
    step: f64,
    get: fn(&Settings) -> f64,
    set: fn(&mut Settings, f64),
) -> SettingField<f64> {
    SettingField::number_input(
        NumberFieldOptions {
            min: *range.start(),
            max: *range.end(),
            step,
        },
        move |cx| get(Settings::get(cx)),
        move |value, cx| {
            settings::update(cx, |settings| set(settings, value));
            cx.refresh_windows();
        },
    )
    .w(px(CONTROL_WIDTH))
}

/// The state of a text field: the input plus the subscription that writes each
/// keystroke into the settings.
struct TextField {
    input: Entity<InputState>,
    _subscription: Subscription,
}

fn text_field(
    placeholder: &'static str,
    suggestions: &'static [&'static str],
    get: fn(&Settings) -> Option<String>,
    set: fn(&mut Settings, &str),
) -> SettingField<SharedString> {
    SettingField::render(move |options, window, cx| {
        let key = field_key(options, "text");
        let state = text_state(key.clone(), placeholder, get, set, window, cx);
        let input = state.read(cx).input.clone();

        let mut field = v_flex().gap_1().w(px(CONTROL_WIDTH)).child(
            Input::new(&input)
                .with_size(options.size())
                .disabled(options.is_disabled()),
        );
        if !suggestions.is_empty() {
            field = field.child(h_flex().gap_1().flex_wrap().children(
                suggestions.iter().enumerate().map(|(index, suggestion)| {
                    let input = input.clone();
                    crate::ui::button(
                        SharedString::from(format!("{key}-suggestion-{index}")),
                        *suggestion,
                    )
                    .size(crate::ui::ButtonSize::Tiny)
                    .ghost()
                    .disabled(options.is_disabled())
                    .on_click(move |_, window, cx| {
                        settings::update(cx, |settings| set(settings, suggestion));
                        input.update(cx, |input, cx| {
                            input.set_value(*suggestion, window, cx);
                        });
                        cx.refresh_windows();
                    })
                }),
            ));
        }
        field.into_any_element()
    })
}

/// The git executable: a text field with the platform's file picker beside it.
///
/// An unusable path is kept and explained inline rather than corrected or
/// cleared on the spot: the user may be halfway through typing it, and the
/// error says what to finish.
fn git_field() -> SettingField<SharedString> {
    SettingField::render(|options, window, cx| {
        let get = |settings: &Settings| {
            settings
                .behaviour
                .git_executable
                .as_ref()
                .map(|path| path.display().to_string())
        };
        let key = field_key(options, "git");
        let state = text_state(key, "/usr/bin/git", get, change::git_executable, window, cx);
        let input = state.read(cx).input.clone();
        let error = git_executable_error(Settings::get(cx).behaviour.git_executable.as_deref());

        let mut field = v_flex().gap_1().w(px(CONTROL_WIDTH)).child(
            h_flex()
                .gap_1()
                .w_full()
                .child(
                    Input::new(&input)
                        .flex_1()
                        .min_w_0()
                        .with_size(options.size())
                        .disabled(options.is_disabled()),
                )
                .child(
                    crate::ui::button("git-executable-browse", "Browse…")
                        .size(crate::ui::ButtonSize::Small)
                        .disabled(options.is_disabled())
                        .on_click(move |_, _, cx| {
                            let paths = cx.prompt_for_paths(PathPromptOptions {
                                files: true,
                                directories: false,
                                multiple: false,
                                prompt: Some("Choose the git executable".into()),
                            });
                            cx.spawn(async move |cx| {
                                if let Ok(Ok(Some(mut chosen))) = paths.await
                                    && let Some(path) = chosen.pop()
                                {
                                    cx.update(|cx| {
                                        settings::update(cx, |settings| {
                                            change::git_executable_path(settings, path.clone());
                                        });
                                        cx.refresh_windows();
                                    });
                                }
                            })
                            .detach();
                        }),
                ),
        );
        if let Some(error) = error {
            let danger = crate::style::Design::new(cx.theme(), Settings::get(cx)).remove_text;
            field = field.child(div().text_size(px(11.0)).text_color(danger).child(error));
        }
        field.into_any_element()
    })
}

/// Why the configured git executable cannot be run, if it cannot. An empty
/// setting is valid: it means git from `PATH`.
fn git_executable_error(path: Option<&Path>) -> Option<String> {
    let path = path?;
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => None,
        Ok(_) => Some(format!("{} is not a file", path.display())),
        Err(_) => Some(format!("{} does not exist", path.display())),
    }
}

fn field_key(options: &gpui_component::setting::RenderOptions, prefix: &str) -> SharedString {
    SharedString::from(format!(
        "settings-{prefix}-{}-{}-{}",
        options.page_ix(),
        options.group_ix(),
        options.item_ix()
    ))
}

fn text_state(
    key: SharedString,
    placeholder: &'static str,
    get: fn(&Settings) -> Option<String>,
    set: fn(&mut Settings, &str),
    window: &mut Window,
    cx: &mut App,
) -> Entity<TextField> {
    let state = window.use_keyed_state(key, cx, move |window, cx| {
        let text = get(Settings::get(cx)).unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(text)
        });
        let subscription = cx.subscribe(
            &input,
            move |_: &mut TextField, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    settings::update(cx, |settings| set(settings, &value));
                    cx.refresh_windows();
                }
            },
        );
        TextField {
            input,
            _subscription: subscription,
        }
    });

    // A reset or a suggestion changed the setting from outside the input; show
    // the stored value. The stored value is trimmed, so compare trimmed text:
    // otherwise the space typed between "JetBrains" and "Mono" is stored as
    // "JetBrains" and this sync immediately deletes it from the input.
    let value = get(Settings::get(cx)).unwrap_or_default();
    state.update(cx, |state, cx| {
        if state.input.read(cx).value().trim() != value.trim() {
            state
                .input
                .update(cx, |input, cx| input.set_value(value.clone(), window, cx));
        }
    });
    state
}

#[cfg(test)]
mod tests {
    use super::git_executable_error;

    #[test]
    fn the_git_executable_must_name_an_existing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("git");
        std::fs::write(&file, b"").expect("write");

        assert_eq!(git_executable_error(None), None, "empty means git on PATH");
        assert_eq!(git_executable_error(Some(&file)), None);
        assert!(
            git_executable_error(Some(dir.path()))
                .expect("a directory is not runnable")
                .contains("is not a file")
        );
        assert!(
            git_executable_error(Some(&dir.path().join("absent")))
                .expect("a missing path is an error")
                .contains("does not exist")
        );
    }
}
