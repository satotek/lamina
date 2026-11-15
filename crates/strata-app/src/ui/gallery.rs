//! `strata --gallery`: every shared part, in every kind, size and state.
//!
//! It exists so a change to `crate::ui` can be judged on one screen, in both
//! themes, without a repository to open. It reads nothing but the theme and the
//! settings, so it runs anywhere the app runs.

use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement, Render, StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::{ActiveTheme as _, Theme, ThemeMode};
use strata_core::{ChangeKind, RefKind};

use super::{
    BadgeTone, BannerKind, ButtonKind, ButtonSize, Confirm, RowState, banner, button, change_badge,
    content_button, count_pill, empty_state, icon_button, icon_toggle, kbd, list_row, message,
    rail, rail_item, ref_badge, section_header, status_badge, swatch_label, swatch_strip, tab,
    tab_strip,
};
use crate::settings::Settings;
use crate::style::{self, Design};

pub struct Gallery {
    focus: FocusHandle,
}

impl Gallery {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self { focus }
    }
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let dark = cx.theme().is_dark();

        let mut column = div()
            .id("gallery")
            .flex()
            .flex_col()
            .gap_6()
            .p_6()
            .size_full()
            .overflow_y_scroll()
            .bg(design.background)
            .text_color(design.text)
            .text_size(px(design.ui))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(20.0))
                                    .font_weight(FontWeight::BOLD)
                                    .child("Strata UI parts"),
                            )
                            .child(
                                div()
                                    .text_size(px(design.small()))
                                    .text_color(design.muted)
                                    .child(if dark { "Dark theme" } else { "Light theme" }),
                            ),
                    )
                    .child(
                        button("gallery-toggle-theme", "Toggle theme")
                            .size(ButtonSize::Medium)
                            .on_click(cx.listener(|_, _, window, cx| {
                                let mode = if cx.theme().is_dark() {
                                    ThemeMode::Light
                                } else {
                                    ThemeMode::Dark
                                };
                                Theme::change(mode, Some(window), cx);
                                crate::style::sync_component_buttons(cx);
                            })),
                    ),
            );

        column = column.child(section(
            "Buttons",
            &design,
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(kinds("Primary", &design, ButtonKind::Primary))
                .child(kinds("Danger", &design, ButtonKind::Danger))
                .child(kinds("Normal", &design, ButtonKind::Normal))
                .child(kinds("Ghost", &design, ButtonKind::Ghost))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .pt_2()
                        .child(
                            icon_button("gallery-icon", gpui_kit_assets::IconName::RefreshCw)
                                .tooltip("Icon only, with a tooltip"),
                        )
                        .child(
                            icon_button("gallery-icon-danger", gpui_kit_assets::IconName::Delete)
                                .danger()
                                .tooltip("Icon only, danger"),
                        )
                        .child(
                            icon_toggle(
                                "gallery-icon-toggle-off",
                                gpui_kit_assets::IconName::FileSearch,
                                false,
                            )
                            .size(ButtonSize::Tiny)
                            .tooltip("Icon toggle (inactive)"),
                        )
                        .child(
                            icon_toggle(
                                "gallery-icon-toggle-on",
                                gpui_kit_assets::IconName::Regex,
                                true,
                            )
                            .size(ButtonSize::Tiny)
                            .tooltip("Icon toggle (active)"),
                        )
                        .child(
                            content_button("gallery-content").ghost().child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .gap(px(2.0))
                                    .text_size(px(10.0))
                                    .child("Icon over label"),
                            ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .pt_2()
                        .child(
                            button("gallery-commit", "Commit file")
                                .size(ButtonSize::Medium)
                                .child(kbd("Ctrl ↵", &design)),
                        )
                        .child(button("gallery-tiny", "Stage hunk").size(ButtonSize::Tiny))
                        .child(
                            button("gallery-disabled", "Disabled")
                                .disabled(true)
                                .on_click(|_, _, _| unreachable!("disabled buttons do not click")),
                        ),
                ),
        ));

        column = column.child(section(
            "Badges",
            &design,
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div().flex().flex_wrap().items_center().gap_2().children(
                        [
                            ChangeKind::Added,
                            ChangeKind::Modified,
                            ChangeKind::Deleted,
                            ChangeKind::Renamed,
                            ChangeKind::Copied,
                            ChangeKind::TypeChanged,
                            ChangeKind::Conflicted,
                            ChangeKind::Untracked,
                        ]
                        .into_iter()
                        .map(|kind| change_badge(kind, &design)),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(ref_badge(
                            RefKind::LocalBranch,
                            true,
                            "main (HEAD)",
                            &design,
                        ))
                        .child(ref_badge(
                            RefKind::LocalBranch,
                            false,
                            "feat/ui-parts",
                            &design,
                        ))
                        .child(ref_badge(
                            RefKind::RemoteBranch,
                            false,
                            "origin/main",
                            &design,
                        ))
                        .child(ref_badge(RefKind::Tag, false, "v1.2.0", &design))
                        .child(count_pill(3, &design))
                        .child(kbd("Ctrl F", &design)),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(status_badge("main", BadgeTone::Accent, &design))
                        .child(status_badge("locked", BadgeTone::Warning, &design))
                        .child(status_badge("missing", BadgeTone::Danger, &design))
                        .child(status_badge("not initialised", BadgeTone::Neutral, &design)),
                ),
        ));

        column = column.child(section(
            "Banners",
            &design,
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(banner(
                    BannerKind::Info,
                    "All remotes are up to date.",
                    &design,
                ))
                .child(banner(
                    BannerKind::Warning,
                    "You are not on any branch; commits are detached.",
                    &design,
                ))
                .child(
                    banner(
                        BannerKind::Danger,
                        "This file still contains <<<<<<< conflict markers.",
                        &design,
                    )
                    .action(
                        button("gallery-banner-cancel", "Cancel")
                            .kind(ButtonKind::Normal)
                            .on_click(|_, _, _| {}),
                    )
                    .action(
                        button("gallery-banner-ok", "Mark resolved anyway")
                            .danger()
                            .on_click(|_, _, _| {}),
                    ),
                ),
        ));

        let rows = [
            ("Normal", RowState::NORMAL),
            ("Selected", RowState::NORMAL.selected(true)),
            ("Revealed", RowState::NORMAL.revealed(true)),
            ("Head", RowState::NORMAL.head(true)),
            (
                "Selected beats head",
                RowState::NORMAL.selected(true).head(true),
            ),
        ];
        let mut row_list = div().flex().flex_col().gap_1().w(px(420.0));
        for (label, state) in rows {
            row_list = row_list.child(
                list_row(format!("gallery-row-{label}"), state, &design)
                    .child(change_badge(ChangeKind::Modified, &design))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child("src/ui/row.rs"),
                    ),
            );
        }
        column = column.child(section("List rows", &design, row_list));

        column = column.child(section(
            "Section headers",
            &design,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .w(px(420.0))
                .child(
                    section_header("gallery-header-plain", "UNSTAGED", &design)
                        .count(2)
                        .trailing(
                            button("gallery-header-action", "Stage all")
                                .kind(ButtonKind::Normal)
                                .on_click(|_, _, _| {}),
                        ),
                )
                .child(
                    section_header("gallery-header-collapsible", "BRANCHES", &design)
                        .count(7)
                        .disclosure(true)
                        .icon(style::glyph_icon(style::glyph::BRANCH, 14.0, design.muted))
                        .on_click(|_, _| {}),
                )
                .child(
                    section_header("gallery-header-danger", "CONFLICTS", &design)
                        .count(1)
                        .danger(),
                ),
        ));

        column = column.child(section(
            "Messages",
            &design,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .h(px(80.0))
                        .flex()
                        .child(empty_state("No commits yet", &design)),
                )
                .child(div().h(px(80.0)).flex().child(message(
                    "computing diff…",
                    design.muted,
                    &design,
                )))
                .child(div().h(px(80.0)).flex().child(message(
                    "git failed: fatal: bad object",
                    design.remove_text,
                    &design,
                ))),
        ));

        column = column.child(section(
            "Tabs",
            &design,
            tab_strip("gallery-tab-strip", &design)
                .border_1()
                .rounded(px(6.0))
                .child(
                    tab("gallery-tab-0", 0, "strata", true, &design)
                        .tooltip("/work/strata")
                        .on_select(|_, _, _| {})
                        .on_close(|_, _| {}),
                )
                .child(
                    tab("gallery-tab-1", 1, "gpui-component", false, &design)
                        .busy(true)
                        .tooltip("/work/gpui-component")
                        .on_select(|_, _, _| {})
                        .on_close(|_, _| {}),
                )
                .child(
                    tab("gallery-tab-2", 2, "docs-site", false, &design)
                        .tooltip("/work/docs-site")
                        .on_select(|_, _, _| {})
                        .on_close(|_, _| {}),
                ),
        ));

        let mut rail_demo = rail(&design)
            .child(
                icon_button(
                    "gallery-rail-expand",
                    gpui_kit_assets::IconName::PanelLeftOpen,
                )
                .size(ButtonSize::Small)
                .ghost()
                .tooltip("Expand the pane"),
            )
            .child(
                rail_item(
                    "gallery-rail-working-tree",
                    style::glyph_icon(style::glyph::COMMIT, 15.0, design.text),
                    "Working tree",
                    &design,
                )
                .count(3)
                .on_click(|_, _, _| {}),
            )
            .child(
                rail_item(
                    "gallery-rail-branches",
                    style::glyph_icon(style::glyph::BRANCH, 15.0, design.accent_text),
                    "Branches",
                    &design,
                )
                .count(12)
                .selected(true)
                .on_click(|_, _, _| {}),
            )
            .child(
                rail_item(
                    "gallery-rail-tags",
                    style::glyph_icon(style::glyph::TAG, 15.0, design.text),
                    "Tags",
                    &design,
                )
                .count(0)
                .on_click(|_, _, _| {}),
            )
            .child(
                rail_item(
                    "gallery-rail-history",
                    style::glyph_icon(style::glyph::COMMIT, 15.0, design.text),
                    "History",
                    &design,
                )
                .on_click(|_, _, _| {}),
            );
        rail_demo = rail_demo.h(px(240.0));
        column = column.child(section(
            "Collapsed rail",
            &design,
            div()
                .flex()
                .items_start()
                .gap_4()
                .child(rail_demo)
                .child(
                    div()
                        .max_w(px(360.0))
                        .text_size(px(design.small()))
                        .text_color(design.muted)
                        .child(
                            "A pane folded to icons: one entry per section, its count underneath, the name in the tooltip. The entry the pane is showing keeps the selection tint.",
                        ),
                ),
        ));
        column =
            column.child(section(
                "Theme swatches",
                &design,
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(swatch_strip(design.lane, &design))
                    .child(div().flex().flex_wrap().items_center().gap_4().children(
                        style::theme::all().iter().map(|theme| {
                            swatch_label(theme.name.clone(), theme.swatches(), &design)
                        }),
                    )),
            ));

        column = column.child(section(
            "Confirmation",
            &design,
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    button("gallery-confirm", "Discard changes…")
                        .danger()
                        .on_click(cx.listener(|_, _, window, cx| {
                            super::confirm(
                                window,
                                cx,
                                Confirm::new(
                                    "Discard changes to src/ui/row.rs?",
                                    "The working-tree changes are lost; this cannot be undone.",
                                )
                                .ok_label("Discard"),
                                |_, _| {},
                            );
                        })),
                )
                .child(
                    button("gallery-confirm-cancel-label", "Abort merge…")
                        .danger()
                        .on_click(cx.listener(|_, _, window, cx| {
                            super::confirm(
                                window,
                                cx,
                                Confirm::new(
                                    "Abort the merge?",
                                    "Conflicts resolved so far are lost.",
                                )
                                .ok_label("Abort merge")
                                .cancel_label("Keep resolving"),
                                |_, _| {},
                            );
                        })),
                )
                .child(
                    div()
                        .text_size(px(design.small()))
                        .text_color(design.muted)
                        .child(
                            "The same dialog the worktree uses. It opens on Cancel: Enter cancels, Tab then Enter discards, Esc cancels too.",
                        ),
                ),
        ));

        div()
            .size_full()
            .track_focus(&self.focus)
            .child(column)
            .into_any_element()
    }
}

/// One titled block of the gallery.
fn section(title: &str, design: &Design, body: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(px(design.tiny()))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(design.muted)
                .child(title.to_owned()),
        )
        .child(body)
        .into_any_element()
}

/// The three sizes of one button kind, plus the disabled state.
fn kinds(title: &str, design: &Design, kind: ButtonKind) -> AnyElement {
    let mut row = div().flex().items_center().gap_2().child(
        div()
            .w(px(70.0))
            .flex_none()
            .text_size(px(design.tiny()))
            .text_color(design.muted)
            .child(title.to_owned()),
    );
    for (label, size) in [
        ("Tiny", ButtonSize::Tiny),
        ("Small", ButtonSize::Small),
        ("Medium", ButtonSize::Medium),
    ] {
        row = row.child(
            button(format!("gallery-{title}-{label}"), label.to_owned())
                .kind(kind)
                .size(size)
                .on_click(|_, _, _| {}),
        );
    }
    row.child(
        button(format!("gallery-{title}-disabled"), "Disabled")
            .kind(kind)
            .size(ButtonSize::Medium)
            .disabled(true),
    )
    .into_any_element()
}
