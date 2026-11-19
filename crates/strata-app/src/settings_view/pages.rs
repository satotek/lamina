//! The six settings pages, built from [`SettingField`]s that read and write the
//! [`Settings`] global directly.
//!
//! Every control writes through [`settings::update`], so there is no Save
//! button: the value is stored, saved in the background and the open windows
//! re-render on the same click. The rules for each value (clamping, blank
//! meaning "default", the offered fetch intervals) live in
//! [`super::change`].

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::kit::{
    self, CardOption, Field, Group, Item, ItemCx, NumberOptions, Page, PreviewCard, preview_cards,
    preview_cards_width,
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, BorrowAppContext as _, Entity, IntoElement,
    ParentElement, PathPromptOptions, SharedString, Styled, Subscription, Task, Window, div, px,
};
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::searchable_list::SearchableListItem;
use gpui_component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IndexPath, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit_assets::IconName;
use strata_core::proxy::ProxyScheme;

use super::change::{self, Section};
use crate::actions::OpenGitConfig;
use crate::fonts::{self, FontChoice};
use crate::settings::{self, DateFormat, DiffLayout, ProxyMode, Settings, ThemeMode};
use crate::style::Design;
use crate::style::theme::{self, ThemeAppearance, ThemeErrorKind, ThemeSource};
use crate::tools::Tooling;

/// Incremented whenever the Network section is reset so [`PasswordField`]
/// clears its masked input and any pending save.
static NETWORK_RESET_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The gap between a row's label column and its control.
const ROW_GAP: f32 = 16.0;

/// The share of a row the label column keeps, so a long setting name is not
/// squeezed into a sliver beside a wide control.
const LABEL_FRACTION: f32 = 0.6;

/// Widest a control gets: wide enough for a font name, not a slab of the page.
const CONTROL_MAX: f32 = 260.0;

/// The protocol and port parts of the proxy server row: both hold short
/// values, so they keep a fixed width and the host field between them flexes.
const SERVER_PROTOCOL_WIDTH: f32 = 92.0;
const SERVER_PORT_WIDTH: f32 = 72.0;
const SERVER_PART_GAP: f32 = 6.0;

/// The icon the sidebar shows beside a section, matching the main sidebar's
/// use of one per list. Every name here is in the asset bundle the app embeds,
/// so a section cannot end up with a blank space where its icon belongs.
pub(super) fn icon(section: Section) -> IconName {
    match section {
        Section::Appearance => IconName::Palette,
        Section::Diff => IconName::Replace,
        Section::History => IconName::FileText,
        Section::Behaviour => IconName::Settings2,
        Section::Tools => IconName::SquareTerminal,
        Section::Network => IconName::Globe,
        Section::Privacy => IconName::EyeOff,
        Section::Keyboard => IconName::Keyboard,
        Section::Commands => IconName::ListChecks,
    }
}

/// Every page, in the order the sidebar lists them.
///
/// `content_width` is the width the dialog leaves the page body; the controls
/// on the right of the rows are sized from it, and a control wider than a
/// row offers a compact fallback instead of squeezing the label.
pub(super) fn pages(content_width: f32, design: &Design, cx: &App) -> Vec<Page> {
    let control = control_width(content_width, design);
    let manual_proxy = Settings::get(cx).proxy.mode == ProxyMode::Manual;
    Section::ALL
        .into_iter()
        .map(|section| page(section, control, content_width, manual_proxy, design))
        .collect()
}

/// Width every control on the right of a settings row shares, so the right edge
/// lines up down a page and across pages.
fn control_width(row: f32, design: &Design) -> f32 {
    let room = row * (1.0 - LABEL_FRACTION) - design.scale(ROW_GAP);
    room.clamp(0.0, design.scale(CONTROL_MAX))
}

fn page(
    section: Section,
    control: f32,
    content_width: f32,
    manual_proxy: bool,
    design: &Design,
) -> Page {
    match section {
        Section::Appearance => appearance(control, design),
        Section::Diff => diff(control, content_width, design),
        Section::History => history(design),
        Section::Behaviour => behaviour(control, design),
        Section::Tools => tools(control, design),
        Section::Network => network(control, manual_proxy, design),
        Section::Privacy => privacy(control, design),
        Section::Keyboard => keyboard(design),
        Section::Commands => commands_page(design),
    }
}

fn appearance(control: f32, design: &Design) -> Page {
    shell(
        Section::Appearance,
        t!("settings.appearance.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.theme.card").to_string(), design).items([
            Item::new(t!("settings.theme.mode").to_string(), theme_field())
                .description(t!("settings.theme.description").to_string()),
            theme_cards(ThemeAppearance::Dark),
            theme_cards(ThemeAppearance::Light),
            Item::render(user_themes_row).keywords([
                t!("settings.theme.open_folder").to_string(),
                t!("settings.theme.export_current").to_string(),
                t!("settings.theme.reload").to_string(),
            ]),
        ]),
    )
    .groups(
        card(t!("settings.appearance.language").to_string(), design).item(
            Item::new(
                t!("settings.appearance.language").to_string(),
                language_field(),
            )
            .description(t!("settings.appearance.language_description").to_string()),
        ),
    )
    .groups(
        card(t!("settings.fonts.card").to_string(), design).items([
            Item::new(
                t!("settings.fonts.interface").to_string(),
                font_field(
                    t!("settings.fonts.system_default").to_string(),
                    control,
                    |settings| settings.appearance.ui_font.clone(),
                    change::ui_font,
                    false,
                ),
            )
            .description(t!("settings.fonts.interface_description").to_string()),
            Item::new(
                t!("settings.fonts.interface_size").to_string(),
                number(
                    change::FONT_SIZE,
                    0.5,
                    control,
                    |settings| f64::from(settings.appearance.ui_font_size),
                    change::ui_font_size,
                ),
            ),
            Item::new(
                t!("settings.fonts.code").to_string(),
                font_field(
                    t!("settings.fonts.bundled_monospace").to_string(),
                    control,
                    |settings| settings.appearance.code_font.clone(),
                    change::code_font,
                    true,
                ),
            )
            .description(t!("settings.fonts.code_description").to_string()),
            Item::new(
                t!("settings.fonts.code_size").to_string(),
                number(
                    change::FONT_SIZE,
                    0.5,
                    control,
                    |settings| f64::from(settings.appearance.code_font_size),
                    change::code_font_size,
                ),
            ),
            Item::new(
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
    .groups(
        card(t!("settings.zoom.card").to_string(), design).item(
            Item::new(
                t!("settings.appearance.zoom").to_string(),
                zoom_field(control),
            )
            .description(t!("settings.appearance.zoom_description").to_string()),
        ),
    )
}

fn diff(control: f32, content_width: f32, design: &Design) -> Page {
    shell(
        Section::Diff,
        t!("settings.diff.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.diff.layout.card").to_string(), design).item(
            Item::new(
                t!("settings.diff.layout.card").to_string(),
                diff_layout_field(content_width, design),
            )
            .description(t!("settings.diff.layout.description").to_string()),
        ),
    )
    .groups(
        card(t!("settings.diff.display.card").to_string(), design).items([
            Item::new(
                t!("settings.diff.wrap").to_string(),
                toggle(|settings| settings.diff.wrap_lines, change::wrap_lines),
            )
            .description(t!("settings.diff.wrap_description").to_string()),
            Item::new(
                t!("settings.diff.syntax").to_string(),
                toggle(
                    |settings| settings.diff.syntax_highlighting,
                    change::syntax_highlighting,
                ),
            )
            .description(t!("settings.diff.syntax_description").to_string()),
            Item::new(
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
        card(t!("settings.diff.comparison.card").to_string(), design)
            .description(t!("settings.diff.comparison_description").to_string())
            .note(t!("settings.diff.comparison_note").to_string())
            .items([
                Item::new(
                    t!("settings.diff.ignore_whitespace").to_string(),
                    toggle(
                        |settings| settings.diff.ignore_whitespace,
                        change::ignore_whitespace,
                    ),
                )
                .description(t!("settings.diff.ignore_whitespace_description").to_string()),
                Item::new(
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

/// The diff layout, offered as two cards that each show the layout as a small
/// picture of itself. A dialog too narrow to hold the cards offers the same
/// two choices as joined buttons instead.
fn diff_layout_field(content_width: f32, design: &Design) -> Field {
    let labels = [
        ("unified", t!("settings.diff.layout.unified").to_string()),
        ("split", t!("settings.diff.layout.split").to_string()),
    ];
    if preview_cards_width(labels.len(), design) > content_width {
        return segments(
            &labels,
            |settings| match settings.diff.layout {
                DiffLayout::Unified => "unified".to_owned(),
                DiffLayout::Split => "split".to_owned(),
            },
            |settings, value| change::diff_layout(settings, diff_layout_value(value)),
        );
    }
    Field::render(move |item, _window, cx| {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let current = match Settings::get(cx).diff.layout {
            DiffLayout::Unified => "unified",
            DiffLayout::Split => "split",
        };
        let cards = vec![
            PreviewCard::new("unified", labels[0].1.clone(), unified_preview),
            PreviewCard::new("split", labels[1].1.clone(), split_preview),
        ];
        preview_cards(
            item,
            cards,
            SharedString::from(current),
            |value, cx| {
                settings::update(cx, |settings| {
                    change::diff_layout(settings, diff_layout_value(&value));
                });
                cx.refresh_windows();
            },
            &design,
        )
    })
}

/// The layout a stored choice names. Anything but "split" keeps the default,
/// the way a hand-edited file is read.
fn diff_layout_value(value: &str) -> DiffLayout {
    match value {
        "split" => DiffLayout::Split,
        _ => DiffLayout::Unified,
    }
}

/// The unified card's picture: each line a full-width bar, the removed one
/// above the added one, over the context lines around them.
fn unified_preview(design: &Design) -> AnyElement {
    v_flex()
        .size_full()
        .justify_center()
        .gap(design.px(4.0))
        .child(preview_bar(design.diff_removed_word, design))
        .child(preview_bar(design.diff_added_word, design))
        .child(preview_bar(design.border, design))
        .child(preview_bar(design.border, design))
        .into_any_element()
}

/// The split card's picture: the old file on the left of a dividing line, the
/// new one on the right, the removed and the added bar on the first row.
fn split_preview(design: &Design) -> AnyElement {
    let column = |first: gpui::Hsla| {
        v_flex()
            .flex_1()
            .min_w_0()
            .justify_center()
            .gap(design.px(4.0))
            .child(preview_bar(first, design))
            .child(preview_bar(design.border, design))
            .child(preview_bar(design.border, design))
    };
    h_flex()
        .size_full()
        .child(column(design.diff_removed_word))
        .child(div().flex_none().w(px(1.0)).h_full().bg(design.border))
        .child(column(design.diff_added_word))
        .into_any_element()
}

/// One line of a layout card's picture: a flat bar in a diff colour.
fn preview_bar(color: gpui::Hsla, design: &Design) -> gpui::Div {
    div().w_full().h(design.px(7.0)).bg(color)
}

fn history(design: &Design) -> Page {
    shell(
        Section::History,
        t!("settings.history.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.history.refs").to_string(), design).items([
            Item::new(
                t!("settings.history.remote_branches").to_string(),
                toggle(
                    |settings| settings.history.show_remote_branches,
                    change::show_remote_branches,
                ),
            )
            .description(t!("settings.history.remote_branches_description").to_string()),
            Item::new(
                t!("settings.history.tags").to_string(),
                toggle(|settings| settings.history.show_tags, change::show_tags),
            )
            .description(t!("settings.history.tags_description").to_string()),
            Item::new(
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
        card(t!("settings.history.dates").to_string(), design).item(
            Item::new(
                t!("settings.history.date_format").to_string(),
                date_format_field(),
            )
            .keywords([
                t!("settings.history.relative").to_string(),
                t!("settings.history.absolute").to_string(),
            ]),
        ),
    )
    .groups(
        card(t!("settings.history.signatures").to_string(), design).item(
            Item::new(
                t!("settings.history.commit_signatures").to_string(),
                toggle(
                    |settings| settings.history.show_commit_signatures,
                    change::show_commit_signatures,
                ),
            )
            .description(t!("settings.history.commit_signatures_description").to_string()),
        ),
    )
    .groups(
        card(t!("settings.history.rows").to_string(), design).items([
            Item::new(
                t!("settings.history.uncommitted_changes").to_string(),
                toggle(
                    |settings| settings.history.show_uncommitted_changes,
                    change::show_uncommitted_changes,
                ),
            )
            .description(t!("settings.history.uncommitted_changes_description").to_string()),
            Item::new(
                t!("settings.history.changed_file_count").to_string(),
                toggle(
                    |settings| settings.history.show_changed_file_count,
                    change::show_changed_file_count,
                ),
            )
            .description(t!("settings.history.changed_file_count_description").to_string()),
        ]),
    )
}

/// The date format, offered as the two renderings of one moment two hours
/// ago. The buttons are their own live example, so no description is needed;
/// the words the old labels used stay behind as search keywords.
fn date_format_field() -> Field {
    let now = crate::style::now_epoch();
    let example = now - 2 * 3600;
    segments(
        &[
            ("relative", crate::style::relative_time(example, now)),
            (
                "absolute",
                crate::style::date_label(example, now, DateFormat::Absolute),
            ),
        ],
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
    )
}

fn behaviour(control: f32, design: &Design) -> Page {
    let every_minutes = |minutes: usize| {
        t!(
            &crate::i18n::count_key("settings.behaviour.every_minutes", minutes),
            count = minutes
        )
        .to_string()
    };
    shell(
        Section::Behaviour,
        t!("settings.behaviour.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.behaviour.fetching").to_string(), design).items([
            Item::new(
                t!("settings.behaviour.auto_fetch").to_string(),
                choice(
                    &[
                        ("0", t!("settings.behaviour.off").to_string()),
                        ("5", every_minutes(5)),
                        ("10", every_minutes(10)),
                        ("15", every_minutes(15)),
                        ("30", every_minutes(30)),
                        ("60", every_minutes(60)),
                    ],
                    control,
                    |settings| settings.behaviour.auto_fetch_minutes.to_string(),
                    |settings, value| {
                        change::auto_fetch_minutes(settings, value.parse().unwrap_or_default());
                    },
                ),
            )
            .description(t!("settings.behaviour.auto_fetch_description").to_string()),
            Item::new(
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
        card(t!("settings.behaviour.safety").to_string(), design).items([
            Item::new(
                t!("settings.behaviour.confirm_destructive").to_string(),
                toggle(
                    |settings| settings.behaviour.confirm_destructive,
                    change::confirm_destructive,
                ),
            )
            .description(t!("settings.behaviour.confirm_destructive_description").to_string()),
            Item::new(
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
            Item::new(
                t!("settings.behaviour.restore_last_repository").to_string(),
                toggle(
                    |settings| settings.behaviour.restore_last_repository,
                    change::restore_last_repository,
                ),
            )
            .description(t!("settings.behaviour.restore_last_repository_description").to_string()),
        ]),
    )
    .group(card(t!("settings.file").to_string(), design).item(settings_file_row()))
}

/// Where the settings are kept, with a way to open that folder. Editing the
/// file by hand is the path for anything the pages do not offer.
fn settings_file_row() -> Item {
    let file = settings::settings_file();
    let label = file.as_ref().map_or_else(
        || t!("settings.file_unknown").to_string(),
        |file| file.display().to_string(),
    );
    Item::new(
        t!("settings.file_location").to_string(),
        Field::render(move |_, _, _| {
            crate::ui::button(
                "open-settings-folder",
                t!("settings.open_folder").to_string(),
            )
            .size(crate::ui::ButtonSize::Small)
            .disabled(settings::settings_file().is_none())
            .on_click(|_, _, cx| {
                if let Some(folder) = settings::settings_file().as_deref().and_then(Path::parent) {
                    cx.open_with_system(folder);
                }
            })
        }),
    )
    .note(label)
}

fn tools(control: f32, design: &Design) -> Page {
    shell(
        Section::Tools,
        t!("settings.tools.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.tools.git").to_string(), design).item(
            Item::new(
                t!("settings.tools.git_executable").to_string(),
                git_field(control),
            )
            .description(t!("settings.tools.git_executable_description").to_string())
            .note(t!("settings.tools.git_executable_note").to_string()),
        ),
    )
    .groups(
        card("ghq", design).items([
            Item::new(
                t!("settings.tools.use_ghq").to_string(),
                tool_toggle(|settings| settings.tools.ghq.enabled, change::ghq_enabled),
            )
            .description(t!("settings.tools.use_ghq_description").to_string()),
            Item::new(
                t!("settings.tools.status").to_string(),
                Field::render(tool_status_row("redetect-ghq", ghq_status)),
            ),
            Item::new(
                t!("settings.tools.root_override").to_string(),
                text_field(
                    "/path/to/ghq/root",
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
        card(t!("settings.tools.github_cli").to_string(), design).items([
            Item::new(
                t!("settings.tools.use_gh").to_string(),
                tool_toggle(|settings| settings.tools.gh.enabled, change::gh_enabled),
            )
            .description(t!("settings.tools.use_gh_description").to_string()),
            Item::new(
                t!("settings.tools.status").to_string(),
                Field::render(tool_status_row("redetect-gh", gh_status)),
            ),
        ]),
    )
    .groups(
        card(t!("settings.tools.repository").to_string(), design).item(Item::render(|_, _, cx| {
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
            Item::new(
                t!("settings.tools.editor_command").to_string(),
                text_field(
                    "code --goto {path}:{line}",
                    control,
                    |settings| settings.tools.editor.clone(),
                    change::editor,
                ),
            )
            .description(t!("settings.tools.editor_description").to_string()),
            Item::new(
                t!("settings.tools.terminal_command").to_string(),
                text_field(
                    "alacritty --working-directory {path}",
                    control,
                    |settings| settings.tools.terminal.clone(),
                    change::terminal,
                ),
            )
            .description(t!("settings.tools.terminal_description").to_string()),
        ];
        #[cfg(feature = "terminal")]
        items.push(
            Item::new(
                t!("settings.tools.embedded_shell").to_string(),
                text_field(
                    "/bin/zsh -l",
                    control,
                    |settings| settings.tools.embedded_shell.clone(),
                    change::embedded_shell,
                ),
            )
            .description(t!("settings.tools.embedded_shell_description").to_string()),
        );
        card(t!("settings.tools.editor_terminal").to_string(), design).items(items)
    })
}

fn network(control: f32, manual: bool, design: &Design) -> Page {
    shell(
        Section::Network,
        t!("settings.network.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.network.proxy_card").to_string(), design).item(
            Item::new(
                t!("settings.network.mode").to_string(),
                segments(
                    &[
                        ("system", t!("settings.network.mode_system").to_string()),
                        ("none", t!("settings.network.mode_none").to_string()),
                        ("manual", t!("settings.network.mode_manual").to_string()),
                    ],
                    |settings| match settings.proxy.mode {
                        ProxyMode::System => "system".to_owned(),
                        ProxyMode::None => "none".to_owned(),
                        ProxyMode::Manual => "manual".to_owned(),
                    },
                    |settings, value| {
                        let mode = match value {
                            "none" => ProxyMode::None,
                            "manual" => ProxyMode::Manual,
                            _ => ProxyMode::System,
                        };
                        change::proxy_mode(settings, mode);
                    },
                ),
            )
            .description(t!("settings.network.mode_description").to_string())
            .note(t!("settings.network.mode_note").to_string()),
        ),
    )
    .groups(
        card(t!("settings.network.manual_card").to_string(), design).items([
            Item::new(
                t!("settings.network.server").to_string(),
                server_field(control),
            )
            .description(t!("settings.network.server_description").to_string())
            .disabled(!manual),
            Item::new(
                t!("settings.network.username").to_string(),
                text_field(
                    t!("settings.network.optional").to_string(),
                    control,
                    |settings| {
                        (!settings.proxy.username.is_empty())
                            .then(|| settings.proxy.username.clone())
                    },
                    change::proxy_username,
                ),
            )
            .disabled(!manual),
            Item::new(
                t!("settings.network.password").to_string(),
                password_field(control),
            )
            .note(t!("settings.network.password_description").to_string())
            .disabled(!manual),
            Item::new(
                t!("settings.network.no_proxy").to_string(),
                text_field(
                    "localhost, 127.0.0.1, .corp",
                    control,
                    |settings| {
                        (!settings.proxy.no_proxy.is_empty())
                            .then(|| settings.proxy.no_proxy.clone())
                    },
                    change::proxy_no_proxy,
                ),
            )
            .description(t!("settings.network.no_proxy_description").to_string())
            .disabled(!manual),
        ]),
    )
    .groups(
        card(t!("settings.network.connection_card").to_string(), design).item(
            Item::render(test_connection_row).keywords([
                t!("settings.network.test_connection").to_string(),
                t!("settings.network.test_connection_description").to_string(),
            ]),
        ),
    )
}

fn privacy(_control: f32, design: &Design) -> Page {
    shell(
        Section::Privacy,
        t!("settings.privacy.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.privacy.network").to_string(), design).item(
            Item::new(
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

fn keyboard(design: &Design) -> Page {
    shell_no_reset(
        Section::Keyboard,
        t!("settings.keyboard.description").to_string(),
        design,
    )
    .groups(
        card(t!("settings.keyboard.bindings").to_string(), design)
            .item(Item::render(keyboard_table)),
    )
}

fn commands_page(design: &Design) -> Page {
    let file = t!("settings.commands.file").to_string();
    shell_no_reset(
        Section::Commands,
        t!("settings.commands.description").to_string(),
        design,
    )
    .groups(card(file, design).item(Item::render(|_, _, cx| {
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
        card(t!("settings.commands.loaded").to_string(), design).item(Item::render(|_, _, cx| {
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
                            .w(design.px(180.0))
                            .truncate()
                            .child(row.title.to_owned()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(design.px(74.0))
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
fn keyboard_table(options: &ItemCx, window: &mut Window, cx: &mut App) -> AnyElement {
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
                .w(design.px(130.0))
                .child(t!("settings.keyboard.column_keys").to_string()),
        )
        .child(
            div()
                .flex_none()
                .w(design.px(100.0))
                .child(t!("settings.keyboard.column_context").to_string()),
        )
        .child(
            div()
                .flex_none()
                .w(design.px(84.0))
                .child(t!("settings.keyboard.column_source").to_string()),
        )
        .child(div().flex_none().w(design.px(84.0)).child(""))
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
    .when(conflict.is_some(), |row| row.h(design.px(46.0)))
    .child(
        v_flex()
            .flex_1()
            .min_w_0()
            .gap(design.px(1.0))
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
            .w(design.px(130.0))
            .gap(design.px(2.0))
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
            .w(design.px(100.0))
            .truncate()
            .text_size(px(design.tiny()))
            .text_color(design.muted)
            .child(row.context.clone()),
    )
    .child(
        v_flex()
            .flex_none()
            .w(design.px(84.0))
            .gap(design.px(2.0))
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
            .w(design.px(84.0))
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

/// A group of rows under a short heading.
fn card(title: impl Into<String>, _design: &Design) -> Group {
    Group::new(title.into())
}

/// Puts a section back to its defaults, from the page title's reset button.
pub(super) fn reset(section: Section, cx: &mut App) {
    settings::update(cx, |settings| {
        change::reset_section(settings, section);
    });
    if section == Section::Appearance {
        settings::apply_theme(None, cx);
        crate::i18n::apply(cx);
    }
    if section == Section::Network {
        NETWORK_RESET_EPOCH.fetch_add(1, Ordering::Relaxed);
        cx.spawn(async move |cx| {
            let _ = cx
                .background_executor()
                .spawn(async move { crate::proxy::store_password("") })
                .await;
            cx.update(|cx| {
                cx.update_global::<Settings, _>(|_, _| {});
                cx.refresh_windows();
            });
        })
        .detach();
    }
    // The zoom is written onto the component theme, not only stored, so the
    // reset has to apply it.
    crate::zoom::apply(cx);
    cx.refresh_windows();
}

/// The page every section starts from: its explanation and a reset action.
fn shell(section: Section, description: impl Into<String>, _design: &Design) -> Page {
    Page::new(section).description(description.into())
}

/// A page whose content is a file the page itself edits, so a reset button
/// has nothing to restore.
fn shell_no_reset(section: Section, description: impl Into<String>, _design: &Design) -> Page {
    Page::new(section)
        .description(description.into())
        .resettable(false)
}

fn theme_field() -> Field {
    Field::segmented(
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
}

/// One appearance's themes as a wrapping row of preview cards under a short
/// heading, in place of the dropdown a theme used to hide behind. A theme is
/// chosen by look, so every palette of that appearance sits on the page,
/// drawn from its own colours; the names double as search keywords because a
/// card row has no title a query can match.
fn theme_cards(appearance: ThemeAppearance) -> Item {
    let is_dark = appearance == ThemeAppearance::Dark;
    let entries = theme::for_appearance(appearance);
    let (heading, info) = if is_dark {
        (
            t!("settings.theme.dark_heading").to_string(),
            t!("settings.theme.dark_theme_description").to_string(),
        )
    } else {
        (
            t!("settings.theme.light_heading").to_string(),
            t!("settings.theme.light_theme_description").to_string(),
        )
    };
    let keywords: Vec<String> = entries
        .iter()
        .map(|entry| entry.palette.name.clone())
        // The heading and the group's own name keep a query that used to
        // match the removed "Light theme"/"Dark theme" rows finding the
        // cards, and the info text keeps an explanation queryable.
        .chain([
            heading.clone(),
            info.clone(),
            t!("settings.theme.card").to_string(),
        ])
        .collect();
    let set = move |value: SharedString, cx: &mut App| {
        settings::update(cx, |settings| {
            if is_dark {
                change::dark_theme(settings, &value);
            } else {
                change::light_theme(settings, &value);
            }
        });
        // The theme is a gpui-component global, so it is applied rather than
        // only stored; that also refreshes every window.
        settings::apply_theme(None, cx);
    };
    Item::render(move |options, window, cx| {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let stored = if is_dark {
            Settings::get(cx).appearance.dark_theme.clone()
        } else {
            Settings::get(cx).appearance.light_theme.clone()
        };
        // A stored id that no longer resolves selects the bundled palette
        // actually drawn, the same one the dropdown fell back to.
        let current = theme::resolve_entry(&stored, is_dark).palette.id.clone();
        let user_tag = t!("settings.theme.user_tag").to_string();
        let cards: Vec<CardOption> = entries
            .iter()
            .map(|entry| {
                let palette = entry.palette.as_ref();
                CardOption {
                    value: palette.id.clone().into(),
                    label: palette.name.clone().into(),
                    fill: palette.background,
                    bars: vec![
                        (palette.border, 0.55),
                        (palette.diff_removed_word, 0.85),
                        (palette.diff_added_word, 0.75),
                        (palette.accent, 0.4),
                    ],
                    tag: (entry.source == ThemeSource::User).then(|| user_tag.clone().into()),
                }
            })
            .collect();
        v_flex()
            .w_full()
            .gap(design.px(4.0))
            .child(kit::sub_heading(
                options,
                heading.clone().into(),
                Some(info.clone().into()),
                &design,
                window,
                cx,
            ))
            .child(kit::card_picker(
                options,
                cards,
                SharedString::from(current),
                set,
                &design,
            ))
            .into_any_element()
    })
    .keywords(keywords)
}

fn user_themes_row(_options: &ItemCx, _window: &mut Window, cx: &mut App) -> AnyElement {
    let design = Design::new(cx.theme(), Settings::get(cx));
    let dir_label = theme::user_themes_dir().map_or_else(
        || t!("settings.theme.no_config_directory").to_string(),
        |dir| dir.display().to_string(),
    );
    let errors = theme::load_errors();

    v_flex()
        .gap_2()
        .w_full()
        .child(
            h_flex()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(
                    crate::ui::button(
                        "open-themes-folder",
                        t!("settings.theme.open_folder").to_string(),
                    )
                    .size(crate::ui::ButtonSize::Small)
                    .on_click(|_, window, cx| match theme::user_themes_dir() {
                        Some(dir) => {
                            if let Err(error) = std::fs::create_dir_all(&dir) {
                                window.push_notification(
                                    Notification::error(
                                        t!("settings.theme.open_folder_error", error = error)
                                            .to_string(),
                                    ),
                                    cx,
                                );
                            } else {
                                cx.open_with_system(&dir);
                            }
                        }
                        None => {
                            window.push_notification(
                                Notification::error(
                                    t!("settings.theme.no_config_directory").to_string(),
                                ),
                                cx,
                            );
                        }
                    }),
                )
                .child(
                    crate::ui::button(
                        "export-current-theme",
                        t!("settings.theme.export_current").to_string(),
                    )
                    .size(crate::ui::ButtonSize::Small)
                    .on_click(|_, window, cx| {
                        let is_dark = cx.theme().is_dark();
                        let settings = Settings::get(cx);
                        let active_id = if is_dark {
                            &settings.appearance.dark_theme
                        } else {
                            &settings.appearance.light_theme
                        };
                        let active = theme::resolve(active_id, is_dark);
                        match theme::export_to_user_dir(&active) {
                            Ok(path) => {
                                theme::reload(cx);
                                let file = path
                                    .file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| path.display().to_string());
                                window.push_notification(
                                    Notification::success(
                                        t!("settings.theme.exported", file = file).to_string(),
                                    ),
                                    cx,
                                );
                            }
                            Err(error) => {
                                window.push_notification(
                                    Notification::error(
                                        t!("settings.theme.export_error", error = error)
                                            .to_string(),
                                    ),
                                    cx,
                                );
                            }
                        }
                    }),
                )
                .child(
                    crate::ui::button("reload-themes", t!("settings.theme.reload").to_string())
                        .size(crate::ui::ButtonSize::Small)
                        .on_click(|_, _, cx| theme::reload(cx)),
                ),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(dir_label),
        )
        .when(!errors.is_empty(), |this| {
            this.child(
                v_flex().gap_1().w_full().children(
                    errors
                        .iter()
                        .map(|error| theme_error_banner(error, &design)),
                ),
            )
        })
        .into_any_element()
}

fn theme_error_banner(error: &theme::ThemeLoadError, design: &Design) -> AnyElement {
    let (lead, detail) = match &error.kind {
        ThemeErrorKind::ReadDirFailed(detail) => (
            t!("settings.theme.error_read_dir", path = &error.file).to_string(),
            Some(detail.as_str()),
        ),
        ThemeErrorKind::ReadFailed(detail) => (
            t!("settings.theme.error_read", file = &error.file).to_string(),
            Some(detail.as_str()),
        ),
        ThemeErrorKind::ParseFailed(detail) => (
            t!("settings.theme.error_invalid", file = &error.file).to_string(),
            Some(detail.as_str()),
        ),
        ThemeErrorKind::CollidesWithBundled(id) => (
            t!(
                "settings.theme.error_bundled_collision",
                file = &error.file,
                id = id
            )
            .to_string(),
            None,
        ),
        ThemeErrorKind::DuplicateUserId(id) => (
            t!(
                "settings.theme.error_duplicate_id",
                file = &error.file,
                id = id
            )
            .to_string(),
            None,
        ),
    };
    let mut banner = crate::ui::banner(crate::ui::BannerKind::Warning, lead, design)
        .wrap()
        .rounded(design.px(6.0));
    if let Some(detail) = detail {
        banner = banner.detail(detail.to_owned());
    }
    banner.into_any_element()
}

/// The language segmented control. A change is applied to every open window
/// at once, rather than waiting for the next launch; three short choices are
/// all visible without opening anything.
fn language_field() -> Field {
    let labels: Vec<(SharedString, SharedString)> = crate::i18n::Language::ALL
        .iter()
        .map(|language| {
            (
                SharedString::from(language.id()),
                SharedString::from(language.label()),
            )
        })
        .collect();
    Field::segmented(
        labels,
        |cx| SharedString::from(Settings::get(cx).appearance.language.id()),
        |value, cx| {
            settings::update(cx, |settings| {
                change::language(settings, crate::i18n::Language::from_id(&value));
            });
            crate::i18n::apply(cx);
        },
    )
}

/// The zoom dropdown. A step applies to every window at once, the way the
/// keyboard and the menu apply it. Eight steps are too many to line up as
/// joined buttons, so the list stays behind the caret.
fn zoom_field(control: f32) -> Field {
    let labels: Vec<(SharedString, SharedString)> = crate::settings::ZOOM_STEPS
        .iter()
        .map(|percent| {
            (
                SharedString::from(percent.to_string()),
                SharedString::from(format!("{percent}%")),
            )
        })
        .collect();
    Field::dropdown(
        labels,
        |cx| SharedString::from(Settings::get(cx).appearance.zoom_percent.to_string()),
        |value, cx| {
            let percent = value
                .parse::<u16>()
                .unwrap_or(crate::settings::DEFAULT_ZOOM);
            crate::zoom::set(cx, percent);
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

fn toggle(get: fn(&Settings) -> bool, set: fn(&mut Settings, bool)) -> Field {
    Field::switch(
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
) -> Field {
    Field::dropdown(
        pairs(list),
        move |cx| SharedString::from(get(Settings::get(cx))),
        move |value, cx| {
            settings::update(cx, |settings| set(settings, &value));
            cx.refresh_windows();
        },
    )
    .w(px(control))
}

/// Joined buttons for two to four short choices, all visible at once.
fn segments<L: AsRef<str>>(
    list: &[(&'static str, L)],
    get: fn(&Settings) -> String,
    set: fn(&mut Settings, &str),
) -> Field {
    Field::segmented(
        pairs(list),
        move |cx| SharedString::from(get(Settings::get(cx))),
        move |value, cx| {
            settings::update(cx, |settings| set(settings, &value));
            cx.refresh_windows();
        },
    )
}

/// A number field whose bounds are the ones [`change`] clamps to, so the two
/// cannot drift apart.
fn number(
    range: std::ops::RangeInclusive<f64>,
    step: f64,
    control: f32,
    get: fn(&Settings) -> f64,
    set: fn(&mut Settings, f64),
) -> Field {
    Field::number_input(
        NumberOptions {
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

/// The select rows a font picker carries.
///
/// The value is the stored setting itself: `None` is the default face row, so
/// choosing it clears the setting instead of storing an empty family name.
impl SearchableListItem for FontChoice {
    type Value = Option<String>;

    fn title(&self) -> SharedString {
        SharedString::from(self.label.clone())
    }

    fn value(&self) -> &Self::Value {
        &self.family
    }
}

/// A font field: a searchable list of the installed families.
///
/// The list is built once per field, when the window first draws it, because
/// enumerating fonts is slow; the select itself keeps the rows for its
/// lifetime. A family stored in the settings is marked when the machine no
/// longer has it, and the app draws the default instead.
struct FontField {
    select: Entity<SelectState<SearchableVec<FontChoice>>>,
    _subscription: Subscription,
}

fn font_field(
    default_label: String,
    control: f32,
    get: fn(&Settings) -> Option<String>,
    set: fn(&mut Settings, &str),
    monospace_first: bool,
) -> Field {
    Field::render(move |options, window, cx| {
        let key = field_key(options, "font");
        let state = font_state(
            key,
            default_label.clone(),
            get,
            set,
            monospace_first,
            window,
            cx,
        );
        let select = state.read(cx).select.clone();
        sync_font_selection(&state, get, window, cx);

        Select::new(&select)
            .placeholder(default_label.clone())
            .search_placeholder(t!("settings.fonts.search").to_string())
            .with_size(options.size())
            .disabled(options.is_disabled())
            .w(px(control))
            .into_any_element()
    })
}

fn font_state(
    key: SharedString,
    default_label: String,
    get: fn(&Settings) -> Option<String>,
    set: fn(&mut Settings, &str),
    monospace_first: bool,
    window: &mut Window,
    cx: &mut App,
) -> Entity<FontField> {
    window.use_keyed_state(key, cx, move |window, cx| {
        let current = get(Settings::get(cx));
        let choices = fonts::picker_choices(
            &fonts::families(cx),
            current.as_deref(),
            &default_label,
            monospace_first,
        );
        // The installed spelling keeps a family typed with different case
        // pointing at the row it now matches.
        let selected = font_value(cx, current.as_deref());
        let selected = choices
            .iter()
            .position(|choice| choice.family == selected)
            .map(IndexPath::new);
        let delegate = SearchableVec::new(choices);
        let select = cx.new(|cx| SelectState::new(delegate, selected, window, cx).searchable(true));
        let subscription = cx.subscribe(
            &select,
            move |_: &mut FontField, _, event: &SelectEvent<SearchableVec<FontChoice>>, cx| {
                if let SelectEvent::Confirm(Some(value)) = event {
                    settings::update(cx, |settings| {
                        set(settings, value.as_deref().unwrap_or_default());
                    });
                    // The families live on the component theme, so they are
                    // applied on the same click that stores them.
                    crate::zoom::apply(cx);
                    cx.refresh_windows();
                }
            },
        );
        FontField {
            select,
            _subscription: subscription,
        }
    })
}

/// The select value of a stored family: the installed spelling when there is
/// one, the stored text otherwise.
fn font_value(cx: &App, current: Option<&str>) -> Option<String> {
    current.map(|family| {
        fonts::installed_family(cx, family)
            .map_or_else(|| family.to_owned(), |installed| installed.to_string())
    })
}

/// Mirrors a setting changed outside the picker — a section reset, or another
/// window — back into the trigger's selection.
fn sync_font_selection(
    state: &Entity<FontField>,
    get: fn(&Settings) -> Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let value = font_value(cx, get(Settings::get(cx)).as_deref());
    let selected = state
        .read(cx)
        .select
        .read(cx)
        .selected_value()
        .cloned()
        .flatten();
    if selected != value {
        state.update(cx, |state, cx| {
            state.select.update(cx, |select, cx| {
                select.set_selected_value(&value, window, cx);
            });
        });
    }
}

/// The state of a text field: the input plus the subscription that writes each
/// keystroke into the settings.
struct TextField {
    input: Entity<InputState>,
    _subscription: Subscription,
}

fn text_field(
    placeholder: impl Into<SharedString>,
    control: f32,
    get: fn(&Settings) -> Option<String>,
    set: fn(&mut Settings, &str),
) -> Field {
    let placeholder = placeholder.into();
    Field::render(move |options, window, cx| {
        let key = field_key(options, "text");
        let state = text_state(key, placeholder.clone(), get, set, window, cx);
        let input = state.read(cx).input.clone();
        v_flex()
            .gap_1()
            .w(px(control))
            .child(
                Input::new(&input)
                    .with_size(options.size())
                    .disabled(options.is_disabled()),
            )
            .into_any_element()
    })
}

/// The git executable: a text field with the platform's file picker beside it.
///
/// An unusable path is kept and explained inline rather than corrected or
/// cleared on the spot: the user may be halfway through typing it, and the
/// error says what to finish.
fn git_field(control: f32) -> Field {
    Field::render(move |options, window, cx| {
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
            let design = crate::style::Design::new(cx.theme(), Settings::get(cx));
            field = field.child(
                div()
                    .text_size(design.px(11.0))
                    .text_color(design.remove_text)
                    .child(error),
            );
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
fn tool_toggle(get: fn(&Settings) -> bool, set: fn(&mut Settings, bool)) -> Field {
    Field::switch(
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
    status: fn(&ItemCx, &mut Window, &mut App) -> AnyElement,
) -> impl Fn(&ItemCx, &mut Window, &mut App) -> AnyElement {
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
fn ghq_status(_options: &ItemCx, _window: &mut Window, cx: &mut App) -> AnyElement {
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
fn gh_status(_options: &ItemCx, _window: &mut Window, cx: &mut App) -> AnyElement {
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
        .max_w(design.px(420.0))
        .text_size(px(design.tiny()))
        .text_color(if ok { design.muted } else { design.remove_text })
        .child(text)
        .into_any_element()
}

fn field_key(options: &ItemCx, prefix: &str) -> SharedString {
    options.key(prefix)
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

struct PortField {
    input: Entity<InputState>,
    last_port: u16,
    invalid: bool,
    _subscription: Subscription,
}

/// The protocols a manual proxy can speak, with the label the dropdown shows.
const PROXY_SCHEMES: [(ProxyScheme, &str); 5] = [
    (ProxyScheme::Http, "HTTP"),
    (ProxyScheme::Https, "HTTPS"),
    (ProxyScheme::Socks4, "SOCKS4"),
    (ProxyScheme::Socks5, "SOCKS5"),
    (ProxyScheme::Socks5h, "SOCKS5h"),
];

/// The proxy server's endpoint as one control: a narrow protocol dropdown, a
/// flexible host field and a narrow port field share the row's control width,
/// so what is really one address does not take three rows.
fn server_field(control: f32) -> Field {
    Field::render(move |options, window, cx| {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let host = text_state(
            field_key(options, "host"),
            "proxy.example.com".into(),
            |settings| (!settings.proxy.host.is_empty()).then(|| settings.proxy.host.clone()),
            change::proxy_host,
            window,
            cx,
        );
        let port = port_state(field_key(options, "port"), window, cx);
        let port_input = port.read(cx).input.clone();
        let invalid = port.read(cx).invalid;

        let current = Settings::get(cx).proxy.scheme;
        let label = PROXY_SCHEMES
            .iter()
            .find(|(scheme, _)| *scheme == current)
            .map_or_else(
                || current.as_str().to_uppercase(),
                |(_, label)| (*label).to_owned(),
            );
        let protocol = Button::new(field_key(options, "scheme"))
            .w(design.px(SERVER_PROTOCOL_WIDTH))
            .accessibility_label(format!("{}: {label}", t!("settings.network.server")))
            .label(label)
            .dropdown_caret(true)
            .outline()
            .disabled(options.is_disabled())
            .with_size(options.size())
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                PROXY_SCHEMES
                    .into_iter()
                    .fold(menu, |menu, (scheme, label)| {
                        menu.item(
                            PopupMenuItem::new(label)
                                .checked(scheme == current)
                                .on_click(move |_, _, cx| {
                                    settings::update(cx, |settings| {
                                        change::proxy_scheme(settings, scheme);
                                    });
                                    cx.refresh_windows();
                                }),
                        )
                    })
            });

        let mut field = v_flex().gap_1().w(px(control)).child(
            h_flex()
                .gap(design.px(SERVER_PART_GAP))
                .items_center()
                .child(protocol)
                .child(
                    Input::new(&host.read(cx).input)
                        .flex_1()
                        .min_w_0()
                        .with_size(options.size())
                        .disabled(options.is_disabled()),
                )
                .child(
                    Input::new(&port_input)
                        .w(design.px(SERVER_PORT_WIDTH))
                        .with_size(options.size())
                        .disabled(options.is_disabled()),
                ),
        );
        if invalid && !options.is_disabled() {
            field = field.child(
                h_flex().justify_end().child(
                    div()
                        .text_size(design.px(11.0))
                        .text_color(design.remove_text)
                        .child(t!("settings.network.port_invalid").to_string()),
                ),
            );
        }
        field.into_any_element()
    })
}

fn port_state(key: SharedString, window: &mut Window, cx: &mut App) -> Entity<PortField> {
    let state = window.use_keyed_state(key, cx, move |window, cx| {
        let current = Settings::get(cx).proxy.port;
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("8080")
                .default_value(current.to_string())
        });
        let window_handle = window.window_handle();
        let subscription = cx.subscribe(
            &input,
            move |state: &mut PortField, input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let raw = input.read(cx).value().to_string();
                    let trimmed = raw.trim();
                    if let Ok(port) = trimmed.parse::<u16>()
                        && port > 0
                    {
                        state.invalid = false;
                        if state.last_port != port {
                            state.last_port = port;
                            settings::update(cx, |settings| change::proxy_port(settings, port));
                            cx.refresh_windows();
                        }
                    } else {
                        state.invalid = true;
                        cx.refresh_windows();
                    }
                }
                InputEvent::Blur if state.invalid => {
                    let fallback = state.last_port.to_string();
                    state.invalid = false;
                    let entity = input.clone();
                    let _ = window_handle.update(cx, |_, window, cx| {
                        entity.update(cx, |input, cx| {
                            input.set_value(fallback, window, cx);
                        });
                    });
                    cx.refresh_windows();
                }
                _ => {}
            },
        );
        PortField {
            input,
            last_port: current,
            invalid: false,
            _subscription: subscription,
        }
    });

    let stored = Settings::get(cx).proxy.port;
    state.update(cx, |state, cx| {
        if state.last_port != stored {
            state.last_port = stored;
            state.invalid = false;
            state.input.update(cx, |input, cx| {
                input.set_value(stored.to_string(), window, cx);
            });
        }
    });
    state
}

struct PasswordField {
    input: Entity<InputState>,
    generation: u64,
    reset_epoch: u64,
    syncing: bool,
    pending_save: Option<String>,
    save_task: Option<Task<()>>,
    error: Option<String>,
    _subscription: Subscription,
}

fn password_field(control: f32) -> Field {
    Field::render(move |options, window, cx| {
        let state = password_state(window, cx);
        let input = state.read(cx).input.clone();
        let error = state.read(cx).error.clone();

        let mut field = v_flex().gap_1().w(px(control)).child(
            Input::new(&input)
                .mask_toggle()
                .with_size(options.size())
                .disabled(options.is_disabled()),
        );
        if let Some(error) = error {
            let design = Design::new(cx.theme(), Settings::get(cx));
            field = field.child(
                div()
                    .text_size(design.px(11.0))
                    .text_color(design.remove_text)
                    .child(error),
            );
        }
        field.into_any_element()
    })
}

fn password_state(window: &mut Window, cx: &mut App) -> Entity<PasswordField> {
    let key = SharedString::from("settings-network-password");
    let state = window.use_keyed_state(key, cx, move |window, cx| {
        let epoch = NETWORK_RESET_EPOCH.load(Ordering::Relaxed);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("settings.network.optional").to_string())
                .masked(true)
        });
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { crate::proxy::load_password() })
                .await;
            let Some(password) = loaded else {
                return;
            };
            let should_apply = this
                .read_with(cx, |state: &PasswordField, _| {
                    state.generation == 0 && NETWORK_RESET_EPOCH.load(Ordering::Relaxed) == epoch
                })
                .unwrap_or(false);
            if !should_apply {
                return;
            }
            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |state: &mut PasswordField, cx| {
                    if state.generation == 0 && NETWORK_RESET_EPOCH.load(Ordering::Relaxed) == epoch
                    {
                        state.syncing = true;
                        state.input.update(cx, |input, cx| {
                            input.set_value(password, window, cx);
                        });
                        state.syncing = false;
                    }
                });
            });
        })
        .detach();

        let subscription = cx.subscribe(
            &input,
            move |state: &mut PasswordField, input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    if state.syncing {
                        return;
                    }
                    state.generation = state.generation.wrapping_add(1);
                    let generation = state.generation;
                    let value = input.read(cx).value().to_string();
                    state.pending_save = Some(value.clone());
                    state.save_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(150))
                            .await;
                        let should_write = this
                            .read_with(cx, |state, _| state.generation == generation)
                            .unwrap_or(false);
                        if !should_write {
                            return;
                        }
                        let result = cx
                            .background_executor()
                            .spawn(async move { crate::proxy::store_password(&value) })
                            .await;
                        let _ = this.update(cx, |state, cx| {
                            if state.generation != generation {
                                return;
                            }
                            state.pending_save = None;
                            match result {
                                Ok(()) => {
                                    state.error = None;
                                    cx.update_global::<Settings, _>(|_, _| {});
                                }
                                Err(error) => {
                                    state.error = Some(error);
                                }
                            }
                            cx.refresh_windows();
                        });
                    }));
                }
                InputEvent::Blur => {
                    if let Some(value) = state.pending_save.take() {
                        state.save_task.take();
                        state.generation = state.generation.wrapping_add(1);
                        let generation = state.generation;
                        state.save_task = Some(cx.spawn(async move |this, cx| {
                            let result = cx
                                .background_executor()
                                .spawn(async move { crate::proxy::store_password(&value) })
                                .await;
                            let _ = this.update(cx, |state, cx| {
                                if state.generation != generation {
                                    return;
                                }
                                match result {
                                    Ok(()) => {
                                        state.error = None;
                                        cx.update_global::<Settings, _>(|_, _| {});
                                    }
                                    Err(error) => {
                                        state.error = Some(error);
                                    }
                                }
                                cx.refresh_windows();
                            });
                        }));
                    }
                }
                _ => {}
            },
        );
        PasswordField {
            input,
            generation: 0,
            reset_epoch: epoch,
            syncing: false,
            pending_save: None,
            save_task: None,
            error: None,
            _subscription: subscription,
        }
    });

    let current_epoch = NETWORK_RESET_EPOCH.load(Ordering::Relaxed);
    state.update(cx, |state, cx| {
        if state.reset_epoch != current_epoch {
            state.reset_epoch = current_epoch;
            state.generation = state.generation.wrapping_add(1);
            state.pending_save = None;
            state.save_task = None;
            state.error = None;
            state.syncing = true;
            state
                .input
                .update(cx, |input, cx| input.set_value(String::new(), window, cx));
            state.syncing = false;
        }
    });
    state
}

#[derive(Default)]
struct TestConnectionState {
    testing: bool,
    generation: u64,
    /// `Ok` carries how long the request took, so a success can be shown with
    /// its timing.
    result: Option<Result<Duration, String>>,
}

/// The diagnostics row: the latest outcome on the left (a dot plus the text)
/// and the button on the right, so one row carries both the action and its
/// result.
fn test_connection_row(_options: &ItemCx, window: &mut Window, cx: &mut App) -> AnyElement {
    let key = SharedString::from("settings-network-test");
    let state = window.use_keyed_state(key, cx, |_, _| TestConnectionState::default());
    let password_entity = password_state(window, cx);
    let design = Design::new(cx.theme(), Settings::get(cx));
    let snapshot = state.read(cx);
    let testing = snapshot.testing;
    let result = snapshot.result.clone();
    let state_entity = state.clone();

    let mut outcome_row = h_flex().gap_2().items_center().min_w_0();
    if let Some(outcome) = result {
        let (text, color) = match outcome {
            Ok(elapsed) => (
                t!("settings.network.test_success", ms = elapsed.as_millis()).to_string(),
                design.add_text,
            ),
            Err(error) => (
                t!("settings.network.test_failed", error = error).to_string(),
                design.remove_text,
            ),
        };
        outcome_row = outcome_row
            .child(
                div()
                    .flex_none()
                    .w(design.px(7.0))
                    .h(design.px(7.0))
                    .rounded_full()
                    .bg(color),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(design.small()))
                    .text_color(color)
                    .child(text),
            );
    }

    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap(design.px(16.0))
        .child(outcome_row)
        .child(
            crate::ui::button(
                "test-proxy-connection",
                if testing {
                    t!("settings.network.testing").to_string()
                } else {
                    t!("settings.network.test_button").to_string()
                },
            )
            .size(crate::ui::ButtonSize::Small)
            .disabled(testing)
            .on_click(move |_, _, cx| {
                let proxy = Settings::get(cx).proxy.clone();
                let pending_password = password_entity.update(cx, |pw, _| {
                    pw.save_task.take();
                    pw.generation = pw.generation.wrapping_add(1);
                    pw.pending_save.take()
                });
                let generation = state_entity.update(cx, |state, _| {
                    state.testing = true;
                    state.result = None;
                    state.generation = state.generation.wrapping_add(1);
                    state.generation
                });
                cx.refresh_windows();

                let target = state_entity.clone();
                let pw_entity = password_entity.clone();
                cx.spawn(async move |cx| {
                    let (pw_saved, pw_error, outcome) = cx
                        .background_executor()
                        .spawn(async move {
                            let mut pw_saved = false;
                            let mut pw_error = None;
                            if let Some(password) = pending_password {
                                match crate::proxy::store_password(&password) {
                                    Ok(()) => pw_saved = true,
                                    Err(error) => pw_error = Some(error),
                                }
                            }
                            // The timing is measured around the request only, so
                            // storing a pending password does not count as
                            // connect time.
                            let started = Instant::now();
                            let outcome =
                                crate::proxy::test_connection("https://github.com", &proxy);
                            (pw_saved, pw_error, outcome.map(|()| started.elapsed()))
                        })
                        .await;
                    cx.update(|cx| {
                        if let Some(error) = pw_error {
                            pw_entity.update(cx, |pw, _| pw.error = Some(error));
                        } else if pw_saved {
                            pw_entity.update(cx, |pw, _| pw.error = None);
                            cx.update_global::<Settings, _>(|_, _| {});
                        }
                        target.update(cx, |state, _| {
                            if state.generation == generation {
                                state.testing = false;
                                state.result = Some(outcome);
                            }
                        });
                        cx.refresh_windows();
                    });
                })
                .detach();
            }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{CONTROL_MAX, LABEL_FRACTION, ROW_GAP, control_width, git_executable_error};
    use crate::settings::Settings;
    use crate::style::Design;
    use gpui_component::Theme;

    #[test]
    fn a_control_always_fits_beside_its_label() {
        let base = Design::default();
        let mut settings = Settings::default();
        settings.appearance.zoom_percent = 150;
        let zoomed = Design::new(&Theme::default(), &settings);

        // The label keeps 60% of the row: a control wider than the rest, less
        // the row's gap, would push the label into a sliver.
        for design in [&base, &zoomed] {
            for row in [300.0, 480.0, 640.0, 2400.0] {
                let viewport = row;
                let control = control_width(row, design);
                assert!(control > 0.0, "{viewport}");
                assert!(
                    control + design.scale(ROW_GAP) <= row * (1.0 - LABEL_FRACTION) + 0.01,
                    "{viewport}: {control} does not fit in {row}"
                );
                assert!(control <= design.scale(CONTROL_MAX), "{viewport}");
            }
            assert_eq!(control_width(2400.0, design), design.scale(CONTROL_MAX));
        }
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
