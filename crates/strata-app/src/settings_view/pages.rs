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
    AnyElement, App, AppContext as _, Entity, FontWeight, IntoElement, ParentElement,
    PathPromptOptions, SharedString, StyleRefinement, Styled, Subscription, Window, div, px,
    relative,
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
use crate::actions::OpenGitConfig;
use crate::settings::{self, DateFormat, DiffLayout, Settings, ThemeMode};
use crate::style::Design;
use crate::tools::Tooling;

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
        Section::Keyboard => IconName::Keyboard,
        Section::Commands => IconName::ListChecks,
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
        Section::Keyboard => keyboard(design),
        Section::Commands => commands_page(design),
    }
}

fn appearance(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::Appearance,
        t!("settings.appearance.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.theme.card").to_string()).item(
            SettingItem::new(t!("settings.theme.card").to_string(), theme_field(control))
                .description(t!("settings.theme.description").to_string()),
        ),
    )
    .groups(
        card(t!("settings.appearance.language").to_string()).item(
            SettingItem::new(
                t!("settings.appearance.language").to_string(),
                language_field(control),
            )
            .description(t!("settings.appearance.language_description").to_string()),
        ),
    )
    .groups(
        card(t!("settings.fonts.card").to_string()).items([
            SettingItem::new(
                t!("settings.fonts.interface").to_string(),
                text_field(
                    t!("settings.fonts.system_default").to_string(),
                    UI_FONTS,
                    control,
                    |settings| settings.appearance.ui_font.clone(),
                    change::ui_font,
                ),
            )
            .description(t!("settings.fonts.interface_description").to_string()),
            SettingItem::new(
                t!("settings.fonts.interface_size").to_string(),
                number(
                    change::FONT_SIZE,
                    0.5,
                    control,
                    |settings| f64::from(settings.appearance.ui_font_size),
                    change::ui_font_size,
                ),
            ),
            SettingItem::new(
                t!("settings.fonts.code").to_string(),
                text_field(
                    t!("settings.fonts.bundled_monospace").to_string(),
                    CODE_FONTS,
                    control,
                    |settings| settings.appearance.code_font.clone(),
                    change::code_font,
                ),
            )
            .description(t!("settings.fonts.code_description").to_string()),
            SettingItem::new(
                t!("settings.fonts.code_size").to_string(),
                number(
                    change::FONT_SIZE,
                    0.5,
                    control,
                    |settings| f64::from(settings.appearance.code_font_size),
                    change::code_font_size,
                ),
            ),
            SettingItem::new(
                t!("settings.fonts.tab_width").to_string(),
                number(
                    change::TAB_WIDTH,
                    1.0,
                    control,
                    |settings| f64::from(settings.appearance.tab_width),
                    change::tab_width,
                ),
            )
            .description(t!("settings.fonts.tab_width_description").to_string()),
        ]),
    )
}

fn diff(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::Diff,
        t!("settings.diff.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.diff.layout.card").to_string()).items([
            SettingItem::new(
                t!("settings.diff.layout.card").to_string(),
                choice(
                    &[
                        ("unified", t!("settings.diff.layout.unified").to_string()),
                        ("split", t!("settings.diff.layout.split").to_string()),
                    ],
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
            .description(t!("settings.diff.layout.description").to_string()),
            SettingItem::new(
                t!("settings.diff.wrap").to_string(),
                toggle(|settings| settings.diff.wrap_lines, change::wrap_lines),
            )
            .description(t!("settings.diff.wrap_description").to_string()),
        ]),
    )
    .groups(
        card(t!("settings.diff.text.card").to_string()).items([
            SettingItem::new(
                t!("settings.diff.syntax").to_string(),
                toggle(
                    |settings| settings.diff.syntax_highlighting,
                    change::syntax_highlighting,
                ),
            )
            .description(t!("settings.diff.syntax_description").to_string()),
            SettingItem::new(
                t!("settings.diff.whitespace").to_string(),
                toggle(
                    |settings| settings.diff.show_whitespace,
                    change::show_whitespace,
                ),
            )
            .description(t!("settings.diff.whitespace_description").to_string()),
        ]),
    )
    .groups(
        card(t!("settings.diff.comparison.card").to_string())
            .description(t!("settings.diff.comparison_description").to_string())
            .items([
                SettingItem::new(
                    t!("settings.diff.ignore_whitespace").to_string(),
                    toggle(
                        |settings| settings.diff.ignore_whitespace,
                        change::ignore_whitespace,
                    ),
                )
                .description(t!("settings.diff.ignore_whitespace_description").to_string()),
                SettingItem::new(
                    t!("settings.diff.context_lines").to_string(),
                    number(
                        change::CONTEXT_LINES,
                        1.0,
                        control,
                        |settings| f64::from(settings.diff.context_lines),
                        change::context_lines,
                    ),
                )
                .description(t!("settings.diff.context_lines_description").to_string()),
            ]),
    )
}

fn history(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::History,
        t!("settings.history.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.history.refs").to_string()).items([
            SettingItem::new(
                t!("settings.history.remote_branches").to_string(),
                toggle(
                    |settings| settings.history.show_remote_branches,
                    change::show_remote_branches,
                ),
            )
            .description(t!("settings.history.remote_branches_description").to_string()),
            SettingItem::new(
                t!("settings.history.tags").to_string(),
                toggle(|settings| settings.history.show_tags, change::show_tags),
            )
            .description(t!("settings.history.tags_description").to_string()),
            SettingItem::new(
                t!("settings.history.stashes").to_string(),
                toggle(
                    |settings| settings.history.show_stashes,
                    change::show_stashes,
                ),
            )
            .description(t!("settings.history.stashes_description").to_string()),
        ]),
    )
    .groups(
        card(t!("settings.history.dates").to_string()).item(
            SettingItem::new(
                t!("settings.history.date_format").to_string(),
                choice(
                    &[
                        ("relative", t!("settings.history.relative").to_string()),
                        ("absolute", t!("settings.history.absolute").to_string()),
                    ],
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
            .description(t!("settings.history.date_format_description").to_string()),
        ),
    )
    .groups(
        card(t!("settings.history.signatures").to_string()).item(
            SettingItem::new(
                t!("settings.history.commit_signatures").to_string(),
                toggle(
                    |settings| settings.history.show_commit_signatures,
                    change::show_commit_signatures,
                ),
            )
            .description(t!("settings.history.commit_signatures_description").to_string()),
        ),
    )
}

fn behaviour(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::Behaviour,
        t!("settings.behaviour.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.behaviour.fetching").to_string()).items([
            SettingItem::new(
                t!("settings.behaviour.auto_fetch").to_string(),
                choice(
                    &[
                        ("0", t!("settings.behaviour.off").to_string()),
                        (
                            "5",
                            t!("settings.behaviour.every_minutes", count = 5).to_string(),
                        ),
                        (
                            "10",
                            t!("settings.behaviour.every_minutes", count = 10).to_string(),
                        ),
                        (
                            "15",
                            t!("settings.behaviour.every_minutes", count = 15).to_string(),
                        ),
                        (
                            "30",
                            t!("settings.behaviour.every_minutes", count = 30).to_string(),
                        ),
                        (
                            "60",
                            t!("settings.behaviour.every_minutes", count = 60).to_string(),
                        ),
                    ],
                    control,
                    |settings| settings.behaviour.auto_fetch_minutes.to_string(),
                    |settings, value| {
                        change::auto_fetch_minutes(settings, value.parse().unwrap_or_default());
                    },
                ),
            )
            .description(t!("settings.behaviour.auto_fetch_description").to_string()),
            SettingItem::new(
                t!("settings.behaviour.prune_on_fetch").to_string(),
                toggle(
                    |settings| settings.behaviour.prune_on_fetch,
                    change::prune_on_fetch,
                ),
            )
            .description(t!("settings.behaviour.prune_on_fetch_description").to_string()),
        ]),
    )
    .groups(
        card(t!("settings.behaviour.safety").to_string()).items([
            SettingItem::new(
                t!("settings.behaviour.confirm_destructive").to_string(),
                toggle(
                    |settings| settings.behaviour.confirm_destructive,
                    change::confirm_destructive,
                ),
            )
            .description(t!("settings.behaviour.confirm_destructive_description").to_string()),
            SettingItem::new(
                t!("settings.behaviour.summary_length_hint").to_string(),
                number(
                    change::SUMMARY_LENGTH,
                    1.0,
                    control,
                    |settings| f64::from(settings.behaviour.summary_length_hint),
                    change::summary_length_hint,
                ),
            )
            .description(t!("settings.behaviour.summary_length_hint_description").to_string()),
            SettingItem::new(
                t!("settings.behaviour.restore_last_repository").to_string(),
                toggle(
                    |settings| settings.behaviour.restore_last_repository,
                    change::restore_last_repository,
                ),
            )
            .description(t!("settings.behaviour.restore_last_repository_description").to_string()),
        ]),
    )
}

fn tools(control: f32, design: &Design) -> SettingPage {
    shell(
        Section::Tools,
        t!("settings.tools.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.tools.git").to_string()).item(
            SettingItem::new(
                t!("settings.tools.git_executable").to_string(),
                git_field(control),
            )
            .description(t!("settings.tools.git_executable_description").to_string()),
        ),
    )
    .groups(
        card("ghq").items([
            SettingItem::new(
                t!("settings.tools.use_ghq").to_string(),
                tool_toggle(|settings| settings.tools.ghq.enabled, change::ghq_enabled),
            )
            .description(t!("settings.tools.use_ghq_description").to_string()),
            SettingItem::new(
                t!("settings.tools.status").to_string(),
                SettingField::render(tool_status_row("redetect-ghq", ghq_status)),
            ),
            SettingItem::new(
                t!("settings.tools.root_override").to_string(),
                text_field(
                    "/path/to/ghq/root",
                    &[],
                    control,
                    |settings| {
                        settings
                            .tools
                            .ghq
                            .root
                            .as_ref()
                            .map(|root| root.display().to_string())
                    },
                    change::ghq_root,
                ),
            )
            .description(t!("settings.tools.root_override_description").to_string()),
        ]),
    )
    .groups(
        card(t!("settings.tools.github_cli").to_string()).items([
            SettingItem::new(
                t!("settings.tools.use_gh").to_string(),
                tool_toggle(|settings| settings.tools.gh.enabled, change::gh_enabled),
            )
            .description(t!("settings.tools.use_gh_description").to_string()),
            SettingItem::new(
                t!("settings.tools.status").to_string(),
                SettingField::render(tool_status_row("redetect-gh", gh_status)),
            ),
        ]),
    )
    .groups(
        card(t!("settings.tools.repository").to_string()).item(SettingItem::render(|_, _, cx| {
            let design = Design::new(cx.theme(), Settings::get(cx));
            let available = crate::git_config_view::has_repository(cx);
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            crate::ui::button(
                                "open-git-config",
                                t!("settings.tools.show_git_config").to_string(),
                            )
                            .size(crate::ui::ButtonSize::Small)
                            .disabled(!available)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenGitConfig), cx);
                            }),
                        )
                        .child(
                            div()
                                .text_size(px(design.tiny()))
                                .text_color(design.muted)
                                .child(if available {
                                    t!("settings.tools.show_git_config_available").to_string()
                                } else {
                                    t!("settings.tools.show_git_config_unavailable").to_string()
                                }),
                        ),
                )
                .into_any_element()
        })),
    )
    .groups({
        #[cfg_attr(not(feature = "terminal"), allow(unused_mut))]
        let mut items = vec![
            SettingItem::new(
                t!("settings.tools.editor_command").to_string(),
                text_field(
                    "code --goto {path}:{line}",
                    &[],
                    control,
                    |settings| settings.tools.editor.clone(),
                    change::editor,
                ),
            )
            .description(t!("settings.tools.editor_description").to_string()),
            SettingItem::new(
                t!("settings.tools.terminal_command").to_string(),
                text_field(
                    "alacritty --working-directory {path}",
                    &[],
                    control,
                    |settings| settings.tools.terminal.clone(),
                    change::terminal,
                ),
            )
            .description(t!("settings.tools.terminal_description").to_string()),
        ];
        #[cfg(feature = "terminal")]
        items.push(
            SettingItem::new(
                t!("settings.tools.embedded_shell").to_string(),
                text_field(
                    "/bin/zsh -l",
                    &[],
                    control,
                    |settings| settings.tools.embedded_shell.clone(),
                    change::embedded_shell,
                ),
            )
            .description(t!("settings.tools.embedded_shell_description").to_string()),
        );
        card(t!("settings.tools.editor_terminal").to_string()).items(items)
    })
}

fn privacy(_control: f32, design: &Design) -> SettingPage {
    shell(
        Section::Privacy,
        t!("settings.privacy.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.privacy.network").to_string()).item(
            SettingItem::new(
                t!("settings.privacy.avatars").to_string(),
                toggle(
                    |settings| settings.privacy.external_avatars,
                    change::external_avatars,
                ),
            )
            .description(t!("settings.privacy.avatars_description").to_string()),
        ),
    )
}

fn keyboard(design: &Design) -> SettingPage {
    shell_no_reset(
        Section::Keyboard,
        t!("settings.keyboard.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.keyboard.bindings").to_string())
            .item(SettingItem::render(keyboard_table)),
    )
}

fn commands_page(design: &Design) -> SettingPage {
    let file = t!("settings.commands.file").to_string();
    shell_no_reset(
        Section::Commands,
        t!("settings.commands.description").to_string(),
        design,
    )
    .groups(card(file).item(SettingItem::render(|_, _, cx| {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let state = crate::commands::CommandState::get(cx);
        let path = state.path.as_ref().map_or_else(
            || {
                t!("settings.commands.no_config_directory")
                    .to_string()
                    .to_owned()
            },
            |path| path.display().to_string(),
        );
        v_flex()
            .gap_2()
            .w_full()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        crate::ui::button(
                            "open-commands-json",
                            t!("settings.commands.open").to_string(),
                        )
                        .size(crate::ui::ButtonSize::Small)
                        .on_click(|_, _, cx| {
                            match crate::commands::ensure_file() {
                                Ok(path) => cx.open_with_system(&path),
                                Err(error) => log::warn!("{error}"),
                            }
                        }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(path),
                    ),
            )
            .when(
                !state.errors.is_empty() || !state.warnings.is_empty(),
                |this| {
                    this.child(crate::ui::banner(
                        crate::ui::BannerKind::Danger,
                        state
                            .errors
                            .iter()
                            .chain(state.warnings.iter())
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" · "),
                        &design,
                    ))
                },
            )
            .into_any_element()
    })))
    .groups(
        card(t!("settings.commands.loaded").to_string()).item(SettingItem::render(|_, _, cx| {
            let design = Design::new(cx.theme(), Settings::get(cx));
            let state = crate::commands::CommandState::get(cx);
            let rows = crate::commands::rows(state);
            if rows.is_empty() {
                return crate::ui::empty_state(t!("settings.commands.empty").to_string(), &design)
                    .into_any_element();
            }
            v_flex()
                .w_full()
                .children(rows.into_iter().enumerate().map(|(index, row)| {
                    crate::ui::list_row(
                        ("custom-command", index),
                        crate::ui::RowState::NORMAL,
                        &design,
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(180.0))
                            .truncate()
                            .child(row.title.to_owned()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(74.0))
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(row.context.to_owned()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(design.mono.clone())
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(row.line),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(if row.confirm {
                                t!("settings.commands.asks").to_string()
                            } else {
                                String::new()
                            }),
                    )
                    .when_some(row.key.map(str::to_owned), |this, key| {
                        this.child(crate::ui::kbd(key, &design))
                    })
                    .into_any_element()
                }))
                .into_any_element()
        })),
    )
}

/// The keyboard page is a table over the action registry; the search field
/// and the Edit buttons live in one element so the pending capture can be
/// shown above the rows.
fn keyboard_table(
    options: &gpui_component::setting::RenderOptions,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let design = Design::new(cx.theme(), Settings::get(cx));
    let key = field_key(options, "keyboard-filter");
    let filter = keyboard_filter(key, window, cx);
    let query = filter.read(cx).input.read(cx).value().to_string();
    let os = cx.platform_info().os;
    let state = crate::keymap::KeymapState::get(cx);
    let rows = crate::keymap::rows(os, state);
    let pending = state.pending_edit;
    let problems = state.problems();

    let mut body = v_flex().w_full().gap_2();
    if let Some(spec) = pending {
        body = body.child(crate::ui::banner(
            crate::ui::BannerKind::Info,
            t!(
                "settings.keyboard.press_new_key",
                title = crate::keymap::display_title(spec)
            )
            .to_string(),
            &design,
        ));
    }
    if !problems.is_empty() {
        body = body.child(crate::ui::banner(
            crate::ui::BannerKind::Warning,
            problems.join(" · "),
            &design,
        ));
    }
    body = body
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Input::new(&filter.read(cx).input)
                        .with_size(options.size())
                        .flex_1()
                        .min_w_0(),
                )
                .child(
                    crate::ui::button(
                        "open-keymap-json",
                        t!("settings.keyboard.open_keymap").to_string(),
                    )
                    .size(crate::ui::ButtonSize::Small)
                    .on_click(|_, _, cx| match crate::keymap::ensure_file() {
                        Ok(path) => cx.open_with_system(&path),
                        Err(error) => log::warn!("{error}"),
                    }),
                ),
        )
        .child(table_header(&design));

    let mut visible = 0usize;
    for row in rows {
        let title = crate::keymap::display_title(row.spec);
        if !query.trim().is_empty()
            && crate::palette::score(&query, &title).is_none()
            && crate::palette::score(&query, row.title).is_none()
            && crate::palette::score(&query, row.name).is_none()
        {
            continue;
        }
        visible += 1;
        body = body.child(action_row(row, &design));
    }
    if visible == 0 {
        body = body.child(crate::ui::empty_state(
            t!("settings.keyboard.no_action_matches").to_string(),
            &design,
        ));
    }
    body.into_any_element()
}

struct KeyboardFilter {
    input: Entity<InputState>,
    _subscription: Subscription,
}

fn keyboard_filter(key: SharedString, window: &mut Window, cx: &mut App) -> Entity<KeyboardFilter> {
    window.use_keyed_state(key, cx, |window, cx| {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("settings.keyboard.filter").to_string())
        });
        let subscription = cx.subscribe(&input, |_: &mut KeyboardFilter, _, event, cx| {
            if matches!(event, gpui_component::input::InputEvent::Change) {
                cx.refresh_windows();
            }
        });
        KeyboardFilter {
            input,
            _subscription: subscription,
        }
    })
}

fn table_header(design: &Design) -> AnyElement {
    h_flex()
        .gap_2()
        .px_2()
        .pb_1()
        .border_b_1()
        .border_color(design.border)
        .text_size(px(design.tiny()))
        .text_color(design.muted)
        .w_full()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(t!("settings.keyboard.column_action").to_string()),
        )
        .child(
            div()
                .flex_none()
                .w(px(130.0))
                .child(t!("settings.keyboard.column_keys").to_string()),
        )
        .child(
            div()
                .flex_none()
                .w(px(100.0))
                .child(t!("settings.keyboard.column_context").to_string()),
        )
        .child(
            div()
                .flex_none()
                .w(px(84.0))
                .child(t!("settings.keyboard.column_source").to_string()),
        )
        .child(div().flex_none().w(px(84.0)).child(""))
        .into_any_element()
}

fn action_row(row: crate::keymap::ActionRow, design: &Design) -> AnyElement {
    let spec = row.spec;
    let title = crate::keymap::display_title(spec);
    let changed = row.changed;
    let conflict = row.conflict;
    let element = crate::ui::list_row(
        SharedString::from(format!("binding-{}", row.name)),
        crate::ui::RowState::NORMAL,
        design,
    )
    .gap_2()
    // Three lines: the title, the action name and the conflict warning.
    .when(conflict.is_some(), |row| row.h(px(46.0)))
    .child(
        v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(1.0))
            .child(div().truncate().child(title))
            .child(
                div()
                    .truncate()
                    .font_family(design.mono.clone())
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(row.name.to_owned()),
            )
            .when_some(row.conflict, |this, other| {
                this.child(
                    div()
                        .truncate()
                        .text_size(px(design.tiny()))
                        .text_color(design.remove_text)
                        .child(t!("settings.keyboard.conflict", other = other).to_string()),
                )
            }),
    )
    .child(
        v_flex()
            .flex_none()
            .w(px(130.0))
            .gap(px(2.0))
            .child(crate::ui::kbd(row.effective, design))
            .when(changed && !row.unbound, |this| {
                this.child(
                    div()
                        .truncate()
                        .text_size(px(design.tiny()))
                        .text_color(design.muted)
                        .child(
                            t!("settings.keyboard.default_keys", keys = row.defaults).to_string(),
                        ),
                )
            }),
    )
    .child(
        div()
            .flex_none()
            .w(px(100.0))
            .truncate()
            .text_size(px(design.tiny()))
            .text_color(design.muted)
            .child(row.context.clone()),
    )
    .child(
        v_flex()
            .flex_none()
            .w(px(84.0))
            .gap(px(2.0))
            .text_size(px(design.tiny()))
            .text_color(if changed {
                design.accent_text
            } else {
                design.muted
            })
            .child(if row.unbound {
                t!("settings.keyboard.source_unbound").to_string()
            } else if changed {
                t!("settings.keyboard.source_user").to_string()
            } else {
                t!("settings.keyboard.source_default").to_string()
            })
            .when(changed, |this| {
                this.child(
                    div()
                        .truncate()
                        .text_size(px(design.tiny()))
                        .text_color(design.muted)
                        .child(row.user.clone()),
                )
            }),
    )
    .child(
        h_flex()
            .flex_none()
            .w(px(84.0))
            .gap_1()
            .child(
                crate::ui::button(
                    SharedString::from(format!("edit-{}", row.name)),
                    t!("common.edit").to_string(),
                )
                .size(crate::ui::ButtonSize::Tiny)
                .ghost()
                .on_click(move |_, _, cx| crate::keymap::begin_capture(cx, spec)),
            )
            .child(
                crate::ui::button(
                    SharedString::from(format!("reset-{}", row.name)),
                    t!("common.reset").to_string(),
                )
                .size(crate::ui::ButtonSize::Tiny)
                .ghost()
                .disabled(!changed)
                .on_click(move |_, _, cx| {
                    if let Err(error) = crate::keymap::reset_binding(cx, spec) {
                        crate::keymap::KeymapState::get_mut(cx).errors.push(error);
                        cx.refresh_windows();
                    }
                }),
            ),
    );
    element.into_any_element()
}

/// A card under construction: a label and the outlined group of rows below it.
///
/// The label is a group of its own rather than the outlined group's title. The
/// settings page list turns every titled group into an expandable sub-item, so
/// a titled card would give the sidebar a chevron with the card's name inside;
/// untitled groups leave it a flat list of page names. The two groups are
/// yielded together so a page lists its cards with `page.groups(card(..))`.
struct Card {
    title: String,
    description: Option<String>,
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
    fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
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
fn card(title: impl Into<String>) -> Card {
    Card {
        title: title.into(),
        description: None,
        group: SettingGroup::new().mt(px(-CARD_LABEL_GAP)),
    }
}

/// The label above a card, as its own untitled group.
///
/// It draws the same element a group title does: muted text on a 1.25 line
/// height, with the description as small muted text under it.
fn card_label(title: String, description: Option<String>) -> SettingGroup {
    SettingGroup::new()
        .variant(GroupBoxVariant::Normal)
        .mb(px(-PAGE_GUTTER))
        .item(SettingItem::render(move |_, _, cx| {
            div()
                .text_color(cx.theme().muted_foreground)
                .line_height(relative(1.25))
                .child(v_flex().gap_1().child(title.clone()).when_some(
                    description.clone(),
                    |this, description| {
                        this.child(
                            Label::new(description)
                                .text_sm()
                                .text_color(cx.theme().muted_foreground),
                        )
                    },
                ))
        }))
}

/// The page heading: the page title, its description and the reset action.
///
/// It is page content rather than the settings component's own header because
/// that header lays its title and suffix out in one left-aligned cluster, which
/// leaves no way to put an action at the row's right end. Everything here sits
/// on the same 32 px inset as the rows of the cards below it.
fn heading(section: Section, description: impl Into<String>, design: &Design) -> SettingGroup {
    let description = description.into();
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
                                SharedString::from(format!("reset-{}", section.id())),
                                t!("settings.keyboard.reset_section").to_string(),
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
                        .child(description.clone()),
                )
        }))
}

/// The page frame every section starts from: a heading, an icon for the
/// sidebar, and the cards below the heading.
fn shell(section: Section, description: impl Into<String>, design: &Design) -> SettingPage {
    SettingPage::new(section.title())
        .icon(icon(section))
        .resettable(false)
        // The built-in header is replaced by [`heading`], which controls the
        // title's weight and the reset button's edge.
        .header_style(&StyleRefinement::default().hidden())
        .group(heading(section, description, design))
}

/// A page whose content is a file the page itself edits, so the settings
/// t!("settings.keyboard.reset_section").to_string() button has nothing to restore.
fn shell_no_reset(
    section: Section,
    description: impl Into<String>,
    design: &Design,
) -> SettingPage {
    SettingPage::new(section.title())
        .icon(icon(section))
        .resettable(false)
        .header_style(&StyleRefinement::default().hidden())
        .group(heading_no_reset(section, description, design))
}

fn heading_no_reset(
    section: Section,
    description: impl Into<String>,
    design: &Design,
) -> SettingGroup {
    let description = description.into();
    let design = design.clone();
    SettingGroup::new()
        .variant(GroupBoxVariant::Normal)
        .item(SettingItem::render(move |_, _, _cx| {
            v_flex()
                .gap(px(8.0))
                .pt(px(8.0))
                .px(px(CARD_PADDING))
                .child(
                    div()
                        .text_size(px(design.ui + 5.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(design.text)
                        .child(section.title()),
                )
                .child(
                    div()
                        .text_size(px(design.ui))
                        .text_color(design.muted)
                        .child(description.clone()),
                )
                .into_any_element()
        }))
}

fn theme_field(control: f32) -> SettingField<SharedString> {
    SettingField::dropdown(
        pairs(&[
            ("system", t!("settings.theme.follow_system").to_string()),
            ("light", t!("settings.theme.light").to_string()),
            ("dark", t!("settings.theme.dark").to_string()),
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

/// The language dropdown. A change is applied to every open window at once,
/// rather than waiting for the next launch.
fn language_field(control: f32) -> SettingField<SharedString> {
    let labels: Vec<(SharedString, SharedString)> = crate::i18n::Language::ALL
        .iter()
        .map(|language| {
            (
                SharedString::from(language.id()),
                SharedString::from(language.label()),
            )
        })
        .collect();
    SettingField::dropdown(
        labels,
        |cx| SharedString::from(Settings::get(cx).appearance.language.id()),
        |value, cx| {
            settings::update(cx, |settings| {
                change::language(settings, crate::i18n::Language::from_id(&value));
            });
            crate::i18n::apply(cx);
        },
    )
    .w(px(control))
}

/// Value and label pairs for a dropdown. Labels are translated by the caller.
fn pairs<L: AsRef<str>>(list: &[(&'static str, L)]) -> Vec<(SharedString, SharedString)> {
    list.iter()
        .map(|(value, label)| {
            (
                SharedString::from(*value),
                SharedString::from(label.as_ref()),
            )
        })
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

fn choice<L: AsRef<str>>(
    list: &[(&'static str, L)],
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
    placeholder: impl Into<SharedString>,
    suggestions: &'static [&'static str],
    control: f32,
    get: fn(&Settings) -> Option<String>,
    set: fn(&mut Settings, &str),
) -> SettingField<SharedString> {
    let placeholder = placeholder.into();
    SettingField::render(move |options, window, cx| {
        let key = field_key(options, "text");
        let state = text_state(key.clone(), placeholder.clone(), get, set, window, cx);
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
        let state = text_state(
            key,
            "/usr/bin/git".into(),
            get,
            change::git_executable,
            window,
            cx,
        );
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
                    crate::ui::button(
                        "git-executable-browse",
                        t!("settings.tools.browse").to_string(),
                    )
                    .size(crate::ui::ButtonSize::Small)
                    .disabled(options.is_disabled())
                    .on_click(move |_, _, cx| {
                        let paths = cx.prompt_for_paths(PathPromptOptions {
                            files: true,
                            directories: false,
                            multiple: false,
                            prompt: Some(
                                t!("settings.tools.choose_git_executable")
                                    .to_string()
                                    .into(),
                            ),
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
        Ok(_) => Some(t!("settings.tools.git_not_a_file", path = path.display()).to_string()),
        Err(_) => Some(t!("settings.tools.git_does_not_exist", path = path.display()).to_string()),
    }
}

/// A switch that also runs detection again: turning a tool off must make its
/// items disappear on the next frame, and turning it on must find it without a
/// restart.
fn tool_toggle(get: fn(&Settings) -> bool, set: fn(&mut Settings, bool)) -> SettingField<bool> {
    SettingField::switch(
        move |cx| get(Settings::get(cx)),
        move |value, cx| {
            settings::update(cx, |settings| set(settings, value));
            crate::tools::redetect(cx);
            cx.refresh_windows();
        },
    )
}

/// A tool's status row with the button that runs detection again, so a program
/// installed while Strata runs shows up without a restart.
fn tool_status_row(
    id: &'static str,
    status: fn(&gpui_component::setting::RenderOptions, &mut Window, &mut App) -> AnyElement,
) -> impl Fn(&gpui_component::setting::RenderOptions, &mut Window, &mut App) -> AnyElement {
    move |options, window, cx| {
        let detecting = Tooling::get(cx).is_some_and(Tooling::detecting);
        h_flex()
            .gap_2()
            .items_center()
            .child(status(options, window, cx))
            .child(
                crate::ui::button(id, t!("settings.tools.redetect").to_string())
                    .size(crate::ui::ButtonSize::Tiny)
                    .disabled(detecting)
                    .on_click(|_, _, cx| crate::tools::redetect(cx)),
            )
            .into_any_element()
    }
}

/// What ghq detection found: path, version and the effective root.
fn ghq_status(
    _options: &gpui_component::setting::RenderOptions,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let design = Design::new(cx.theme(), Settings::get(cx));
    let settings_root = Settings::get(cx).tools.ghq.root.clone();
    let Some(tooling) = Tooling::get(cx) else {
        return status_text(
            t!("settings.tools.ghq_missing").to_string().to_owned(),
            false,
            &design,
        );
    };
    match tooling.ghq() {
        Some(info) => {
            let version = info
                .version
                .clone()
                .unwrap_or_else(|| t!("settings.tools.version_unknown").to_string());
            let root = settings_root.as_deref().unwrap_or(info.root.as_path());
            status_text(
                format!(
                    "{} · {version}\n{}",
                    info.binary.display(),
                    t!("settings.tools.root_line", root = root.display())
                ),
                true,
                &design,
            )
        }
        None => status_text(tooling.ghq_reason(), false, &design),
    }
}

/// What `gh` detection found: path, version and whether it has an account.
fn gh_status(
    _options: &gpui_component::setting::RenderOptions,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let design = Design::new(cx.theme(), Settings::get(cx));
    let Some(tooling) = Tooling::get(cx) else {
        return status_text(
            t!("settings.tools.gh_missing").to_string().to_owned(),
            false,
            &design,
        );
    };
    match tooling.gh() {
        Some(info) if info.logged_in => status_text(
            format!(
                "{} · {} · {}",
                info.binary.display(),
                info.version,
                t!("settings.tools.gh_logged_in")
            ),
            true,
            &design,
        ),
        Some(info) => status_text(
            format!(
                "{} · {} · {}",
                info.binary.display(),
                info.version,
                tooling.gh_reason()
            ),
            false,
            &design,
        ),
        None => status_text(tooling.gh_reason(), false, &design),
    }
}

fn status_text(text: String, ok: bool, design: &Design) -> AnyElement {
    div()
        .max_w(px(420.0))
        .text_size(px(design.tiny()))
        .text_color(if ok { design.muted } else { design.remove_text })
        .child(text)
        .into_any_element()
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
    placeholder: SharedString,
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
