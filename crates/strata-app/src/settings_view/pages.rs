//! The six settings pages, built from [`SettingField`]s that read and write the
//! [`Settings`] global directly.
//!
//! Every control writes through [`settings::update`], so there is no Save
//! button: the value is stored, saved in the background and the open windows
//! re-render on the same click. The rules for each value (clamping, blank
//! meaning "default", the offered fetch intervals) live in
//! [`super::change`].

use std::path::Path;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Entity, FontWeight, IntoElement, ParentElement, PathPromptOptions,
    SharedString, StyleRefinement, Styled, Subscription, Window, div, px, relative,
};
use gpui_component::group_box::GroupBoxVariant;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::label::Label;
use gpui_component::setting::{
    NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage,
};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit_assets::IconName;

use super::SIDEBAR_MAX;
use super::change::{self, Section};
use crate::settings::{self, DateFormat, DiffLayout, Settings, ThemeMode};
use crate::style::Design;

/// Font families offered beside the free-text font fields.
const UI_FONTS: &[&str] = &["Inter", "SF Pro Text", "Segoe UI"];
const CODE_FONTS: &[&str] = &["JetBrains Mono", "Cascadia Code", "Menlo", "Consolas"];

/// The gutter the page list gives its cards, and the padding a card gives its
/// rows. Both come from gpui-component; they are named here because the width a
/// control may take is what is left after them.
const PAGE_GUTTER: f32 = 16.0;
const CARD_PADDING: f32 = 16.0;

/// The gap between a row's label column and its control (`SettingItem`'s).
const ROW_GAP: f32 = 12.0;

/// The gap between a card's label and the card's surface, the space a group
/// title used to sit over its group.
const CARD_LABEL_GAP: f32 = 8.0;

/// The share of a row the label column may take. `SettingItem` caps it there
/// and stops shrinking it at that cap, so a control wider than the rest of the
/// row is pushed past the card's right padding instead of fitting beside it.
const LABEL_FRACTION: f32 = 0.6;

/// Widest a control gets: wide enough for a font name, not a slab of the page.
const CONTROL_MAX: f32 = 260.0;

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
///
/// `viewport_width` is the settings window's width. The right-hand controls are
/// sized from it because the page list and its cards are only as wide as the
/// window leaves them.
pub(super) fn pages(viewport_width: f32, design: &Design) -> Vec<SettingPage> {
    let control = control_width(viewport_width);
    Section::ALL
        .into_iter()
        .map(|section| page(section, control, design))
        .collect()
}

/// Width every control on the right of a settings row shares, so the right edge
/// lines up down a page and across pages.
///
/// The sidebar keeps the width it was dragged to inside the settings component,
/// so the widest it can become is what the calculation may assume; beside a
/// narrower sidebar the control just ends further inside the card.
fn control_width(viewport_width: f32) -> f32 {
    let row = viewport_width - SIDEBAR_MAX - 2.0 * PAGE_GUTTER - 2.0 * CARD_PADDING;
    let room = row * (1.0 - LABEL_FRACTION) - ROW_GAP;
    room.clamp(0.0, CONTROL_MAX)
}

fn page(section: Section, control: f32, design: &Design) -> SettingPage {
    match section {
        Section::Appearance => appearance(control, design),
        Section::Diff => diff(control, design),
        Section::History => history(control, design),
        Section::Behaviour => behaviour(control, design),
        Section::Tools => tools(control, design),
        Section::Privacy => privacy(control, design),
    }
}

fn appearance(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::Appearance,
        "Preferences for Strata itself. Nothing here writes git config.",
        design,
    )
    .groups(card("Theme").item(
        SettingItem::new("Theme", theme_field(control)).description(
            "Follow the system, or pin light or dark. Every window follows immediately.",
        ),
    ))
    .groups(
        card("Fonts").items([
            SettingItem::new(
                "Interface font",
                text_field(
                    "System default",
                    UI_FONTS,
                    control,
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
                    control,
                    |settings| f64::from(settings.appearance.ui_font_size),
                    change::ui_font_size,
                ),
            ),
            SettingItem::new(
                "Code font",
                text_field(
                    "Bundled monospace",
                    CODE_FONTS,
                    control,
                    |settings| settings.appearance.code_font.clone(),
                    change::code_font,
                ),
            )
            .description(
                "Diffs, object ids and git commands. Leave empty for the bundled monospace face.",
            ),
            SettingItem::new(
                "Code size",
                number(
                    change::FONT_SIZE,
                    0.5,
                    control,
                    |settings| f64::from(settings.appearance.code_font_size),
                    change::code_font_size,
                ),
            ),
            SettingItem::new(
                "Tab width",
                number(
                    change::TAB_WIDTH,
                    1.0,
                    control,
                    |settings| f64::from(settings.appearance.tab_width),
                    change::tab_width,
                ),
            )
            .description("Columns a tab expands to in a diff."),
        ]),
    )
}

fn diff(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::Diff,
        "How a diff is drawn, in every pane that shows one.",
        design,
    )
        .groups(
            card("Layout").items([
                SettingItem::new(
                    "Layout",
                    choice(
                        &[("unified", "Unified"), ("split", "Side by side")],
                        control,
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
        .groups(
            card("Text").items([
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
        .groups(
            card("Comparison")
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
                            control,
                            |settings| f64::from(settings.diff.context_lines),
                            change::context_lines,
                        ),
                    )
                    .description("Unchanged lines shown around each change (git's default is 3)."),
                ]),
        )
}

fn history(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::History,
        "What the commit list and the sidebar show.",
        design,
    )
    .groups(
        card("Refs").items([
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
    .groups(
        card("Dates").item(
            SettingItem::new(
                "Date format",
                choice(
                    &[("relative", "Relative"), ("absolute", "Absolute")],
                    control,
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

fn behaviour(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::Behaviour,
        "Background work and the questions Strata asks before it acts.",
        design,
    )
        .groups(
            card("Fetching").items([
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
                        control,
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
        .groups(
            card("Safety").items([
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
                        control,
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

fn tools(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::Tools,
        "Programs Strata runs on your behalf.",
        design,
    )
        .groups(
            card("Git").item(
                SettingItem::new("Git executable", git_field(control)).description(
                    "Leave empty to use git from PATH. A changed path applies to repositories opened afterwards; a repository already open keeps the binary it started with.",
                ),
            ),
        )
        .groups(
            card("Editor and terminal").items([
                SettingItem::new(
                    "Editor command",
                    text_field(
                        "code --goto {path}:{line}",
                        &[],
                        control,
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
                        control,
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

fn privacy(_control: f32, design: &Design) -> SettingPage {
    shell(Section::Privacy, "Only what leaves this machine.", design).groups(
        card("Network").item(
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

/// A card under construction: a label and the outlined group of rows below it.
///
/// The label is a group of its own rather than the outlined group's title. The
/// settings page list turns every titled group into an expandable sub-item, so
/// a titled card would give the sidebar a chevron with the card's name inside;
/// untitled groups leave it a flat list of page names. The two groups are
/// yielded together so a page lists its cards with `page.groups(card(..))`.
struct Card {
    title: &'static str,
    description: Option<&'static str>,
    group: SettingGroup,
}

impl Card {
    fn item(mut self, item: SettingItem) -> Self {
        self.group = self.group.item(item);
        self
    }

    fn items(mut self, items: impl IntoIterator<Item = SettingItem>) -> Self {
        self.group = self.group.items(items);
        self
    }

    /// Supporting text under the card's label, above its surface.
    fn description(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }
}

impl IntoIterator for Card {
    type Item = SettingGroup;
    type IntoIter = std::array::IntoIter<SettingGroup, 2>;

    fn into_iter(self) -> Self::IntoIter {
        [card_label(self.title, self.description), self.group].into_iter()
    }
}

/// A card: a section label above an outlined group, on the 8 px grid.
///
/// The page list gives every group [`PAGE_GUTTER`] above and below it, so the
/// label takes back the space below itself and the card takes back half the
/// space above it. The label then sits the same [`CARD_LABEL_GAP`] over its
/// surface, and two cards keep the same 32 px between them, as when the label
/// was the group's title.
fn card(title: &'static str) -> Card {
    Card {
        title,
        description: None,
        group: SettingGroup::new().mt(px(-CARD_LABEL_GAP)),
    }
}

/// The label above a card, as its own untitled group.
///
/// It draws the same element a group title does: muted text on a 1.25 line
/// height, with the description as small muted text under it.
fn card_label(title: &'static str, description: Option<&'static str>) -> SettingGroup {
    SettingGroup::new()
        .variant(GroupBoxVariant::Normal)
        .mb(px(-PAGE_GUTTER))
        .item(SettingItem::render(move |_, _, cx| {
            div()
                .text_color(cx.theme().muted_foreground)
                .line_height(relative(1.25))
                .child(
                    v_flex()
                        .gap_1()
                        .child(title)
                        .when_some(description, |this, description| {
                            this.child(
                                Label::new(description)
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground),
                            )
                        }),
                )
        }))
}

/// The page heading: the page title, its description and the reset action.
///
/// It is page content rather than the settings component's own header because
/// that header lays its title and suffix out in one left-aligned cluster, which
/// leaves no way to put an action at the row's right end. Everything here sits
/// on the same 32 px inset as the rows of the cards below it.
fn heading(section: Section, description: &'static str, design: &Design) -> SettingGroup {
    let design = design.clone();
    SettingGroup::new()
        .variant(GroupBoxVariant::Normal)
        .item(SettingItem::render(move |_, _, _cx| {
            v_flex()
                .gap(px(8.0))
                .pt(px(8.0))
                .px(px(CARD_PADDING))
                .child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(design.ui + 5.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(design.text)
                                .child(section.title()),
                        )
                        .child(
                            crate::ui::button(
                                SharedString::from(format!(
                                    "reset-{}",
                                    section.title().to_lowercase()
                                )),
                                "Reset section",
                            )
                            .size(crate::ui::ButtonSize::Small)
                            .ghost()
                            .on_click(move |_, _, cx| {
                                settings::update(cx, |settings| {
                                    change::reset_section(settings, section);
                                });
                                cx.refresh_windows();
                            }),
                        ),
                )
                .child(
                    div()
                        .text_size(px(design.ui))
                        .text_color(design.muted)
                        .child(description),
                )
        }))
}

/// The page frame every section starts from: a heading, an icon for the
/// sidebar, and the cards below the heading.
fn shell(section: Section, description: &'static str, design: &Design) -> SettingPage {
    SettingPage::new(section.title())
        .icon(icon(section))
        .resettable(false)
        // The built-in header is replaced by [`heading`], which controls the
        // title's weight and the reset button's edge.
        .header_style(&StyleRefinement::default().hidden())
        .group(heading(section, description, design))
}

fn theme_field(control: f32) -> SettingField<SharedString> {
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
    .w(px(control))
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
    control: f32,
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
    .w(px(control))
}

/// A number field whose bounds are the ones [`change`] clamps to, so the two
/// cannot drift apart.
fn number(
    range: std::ops::RangeInclusive<f64>,
    step: f64,
    control: f32,
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
    .w(px(control))
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
    control: f32,
    get: fn(&Settings) -> Option<String>,
    set: fn(&mut Settings, &str),
) -> SettingField<SharedString> {
    SettingField::render(move |options, window, cx| {
        let key = field_key(options, "text");
        let state = text_state(key.clone(), placeholder, get, set, window, cx);
        let input = state.read(cx).input.clone();

        let mut field = v_flex().gap_1().w(px(control)).child(
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
fn git_field(control: f32) -> SettingField<SharedString> {
    SettingField::render(move |options, window, cx| {
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

        let mut field = v_flex().gap_1().w(px(control)).child(
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
    use super::{
        CARD_PADDING, CONTROL_MAX, LABEL_FRACTION, PAGE_GUTTER, ROW_GAP, SIDEBAR_MAX,
        control_width, git_executable_error,
    };

    #[test]
    fn a_control_always_fits_beside_its_label() {
        // The row is what the window leaves after the sidebar at its widest and
        // the gutters on both sides of the card, and the label may take 60% of
        // it: anything wider than the rest, less the row's gap, spills past the
        // card's right padding.
        for viewport in [640.0, 900.0, 1200.0, 2400.0] {
            let row = viewport - SIDEBAR_MAX - 2.0 * PAGE_GUTTER - 2.0 * CARD_PADDING;
            let control = control_width(viewport);
            assert!(control > 0.0, "{viewport}");
            assert!(
                control + ROW_GAP <= row * (1.0 - LABEL_FRACTION) + 0.01,
                "{viewport}: {control} does not fit in {row}"
            );
            assert!(control <= CONTROL_MAX, "{viewport}");
        }
        assert_eq!(control_width(2400.0), CONTROL_MAX);
    }

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
