//! The About page: what the binary is, how it was built, and what it bundles.
//!
//! The build revision and date are written into the binary by `build.rs`, so
//! the page never asks git or the filesystem for them at render time. The
//! git version and the third-party notices are probed on the background
//! executor; until they arrive the page draws the rest, because the About
//! page must open even when git is missing or slow.
//!
//! The page lives in the settings dialog; [`AboutStrata`] opens that dialog
//! on this page. The Overview borrows the settings pages' look — a hero, a
//! row of links, then groups of flat rows — so it reads as one more page of
//! the same dialog rather than a second window.
//!
//! The licence texts are embedded, so they are always readable. The
//! third-party notices are generated per release and copied next to the
//! executable (or into `Contents/Resources` in a bundle); a development build
//! has no such file and shows a link to the repository instead.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Div, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, actions, div, px,
};
use gpui_component::switch::Switch;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Icon, WindowExt as _, h_flex, v_flex};
use gpui_kit_assets::IconName;
use strata_core::git_cli::Git;

use crate::menus::AboutStrata;
use crate::settings::{self, Settings};
use crate::style::{self, Design};
use crate::ui::{ButtonKind, ButtonSize, button, spinner};
use crate::updater::{Outcome, Status};

/// The project page, the issue form and the release tag are derived from the
/// manifest's repository so a fork only has to change one file.
const REPOSITORY: &str = "https://github.com/satotek/strata";
const ISSUES: &str = "https://github.com/satotek/strata/issues/new";
const NOTICES_FILE: &str = "THIRD-PARTY-NOTICES.md";

const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = env!("STRATA_BUILD_COMMIT");
const BUILD_DATE: &str = env!("STRATA_BUILD_DATE");

pub(crate) const ABOUT_VIEW: &str = "AboutView";
/// How long the copy button says that it copied.
const COPIED_FOR: Duration = Duration::from_secs(2);

/// Keyed state under the auto-check row's pinned description.
const AUTO_CHECK_INFO: &str = "about-auto-check-info";

const MIT_LICENSE: &str = include_str!("../../../LICENSE-MIT");
const APACHE_LICENSE: &str = include_str!("../../../LICENSE-APACHE");

actions!(
    strata_about,
    [
        /// Closes the dialog showing the About page.
        CloseAbout,
    ]
);

/// Registers the action that opens the page. Call once at startup.
pub fn bind(cx: &mut App) {
    cx.on_action(|_: &AboutStrata, cx: &mut App| {
        crate::settings_view::show(Some(crate::settings_view::Destination::About), cx);
    });
    crate::keymap::bind_group(cx, crate::keymap::Group::Application);
}

pub(crate) struct AboutView {
    page: Page,
    notices: Notices,
    git_version: Option<String>,
    copied: bool,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Overview,
    License(License),
    Notices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum License {
    Mit,
    Apache,
}

impl License {
    fn title(self) -> String {
        match self {
            License::Mit => t!("about.license_mit").to_string(),
            License::Apache => t!("about.license_apache").to_string(),
        }
    }

    fn text(self) -> &'static str {
        match self {
            License::Mit => MIT_LICENSE,
            License::Apache => APACHE_LICENSE,
        }
    }
}

enum Notices {
    Loading,
    Found(String),
    Missing,
}

impl Notices {
    #[cfg(test)]
    fn text(&self) -> Option<&str> {
        match self {
            Notices::Found(text) => Some(text),
            _ => None,
        }
    }
}

impl AboutView {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        // The switch reads the settings and the status row follows a check
        // the menu, the startup schedule or a notification started, so both
        // globals have to repaint the page when they change under it.
        let subscriptions = vec![
            cx.observe_global::<Settings>(|_, cx| cx.notify()),
            cx.observe_global::<crate::updater::UpdaterState>(|_, cx| cx.notify()),
        ];
        let executable = std::env::current_exe().ok();
        let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let task = cx.background_executor().spawn(async move {
            // The same handle the rest of the app writes with, and the same
            // `--version` probe, so the About page and the session agree on
            // which git is in use.
            let git_version = Git::new(workdir)
                .version()
                .ok()
                .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"));
            let notices = executable
                .as_deref()
                .and_then(|executable| locate_notices(current_platform(), executable))
                .map_or(Notices::Missing, Notices::Found);
            (git_version, notices)
        });
        cx.spawn(async move |this, cx| {
            let (git_version, notices) = task.await;
            let _ = this.update(cx, |this, cx| {
                if git_version.is_some() {
                    this.git_version = git_version;
                }
                this.notices = notices;
                cx.notify();
            });
        })
        .detach();

        Self {
            page: Page::Overview,
            notices: Notices::Loading,
            git_version: None,
            copied: false,
            _subscriptions: subscriptions,
        }
    }

    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let text = copy_block(self.git_version.as_deref());
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// What a document page would show, which is also what the licence and
    /// notices assertions read.
    #[cfg(test)]
    fn document_text(&self) -> Option<String> {
        match self.page {
            Page::Overview => None,
            Page::License(license) => Some(license.text().to_owned()),
            Page::Notices => self.notices.text().map(str::to_owned),
        }
    }

    /// The app mark beside the name, the one-line description and the build
    /// lines. Every line wraps rather than overflows, so the hero survives the
    /// dialog's narrow widths.
    fn hero(&self, design: &Design) -> Div {
        h_flex()
            .flex_wrap()
            .items_center()
            .gap(design.px(14.0))
            .child(style::app_mark(design.scale(52.0)))
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .gap(design.px(3.0))
                    .child(
                        div()
                            .text_size(px(design.ui + design.scale(6.0)))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("about.app_and_version", version = VERSION).to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(design.small()))
                            .text_color(design.muted)
                            .child(t!("about.description").to_string()),
                    )
                    .child(
                        div()
                            .font_family(design.mono.clone())
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(
                                t!("about.build_line", commit = COMMIT, date = BUILD_DATE)
                                    .to_string(),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(environment_line(self.git_version.as_deref())),
                    ),
            )
    }

    /// The links out of the page: the release the user is on, where it is
    /// developed, where to report what does not work, and the copy button
    /// that carries all of it into a bug report.
    fn links(&self, design: &Design, cx: &mut Context<Self>) -> Div {
        h_flex()
            .flex_wrap()
            .items_center()
            .gap(design.px(8.0))
            .child(
                button("about-release-notes", t!("about.release_notes").to_string())
                    .kind(ButtonKind::Normal)
                    .size(ButtonSize::Small)
                    .on_click(|_, _, cx| {
                        cx.open_with_system(Path::new(&release_notes_url()));
                    }),
            )
            .child(
                button("about-github", t!("about.github").to_string())
                    .kind(ButtonKind::Normal)
                    .size(ButtonSize::Small)
                    .on_click(|_, _, cx| cx.open_with_system(Path::new(REPOSITORY))),
            )
            .child(
                button("about-report-issue", t!("about.report_issue").to_string())
                    .kind(ButtonKind::Normal)
                    .size(ButtonSize::Small)
                    .on_click(|_, _, cx| cx.open_with_system(Path::new(ISSUES))),
            )
            .child(
                button(
                    "about-copy",
                    if self.copied {
                        t!("about.copied").to_string()
                    } else {
                        t!("about.copy_info").to_string()
                    },
                )
                .kind(ButtonKind::Normal)
                .size(ButtonSize::Small)
                .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
            )
    }

    /// The check row and the auto-check switch. While a check or install
    /// runs the row names the phase; once idle it keeps the last outcome, so
    /// a dismissed notification is not the only place the result lives.
    fn updates_group(
        &self,
        design: &Design,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = crate::updater::status(cx);
        let outcome = crate::updater::outcome(cx);
        let checking = matches!(status, Some(Status::Checking));
        let line = |text: String| {
            div()
                .min_w_0()
                .truncate()
                .text_size(px(design.small()))
                .child(text)
        };
        let status_line = match (&status, &outcome) {
            (Some(status), _) => h_flex()
                .min_w_0()
                .items_center()
                .gap(design.px(8.0))
                .when(checking, |this| {
                    this.child(spinner("about-updates-spinner", design, 12.0))
                })
                .child(line(status.label()))
                .into_any_element(),
            (None, Some(outcome)) => {
                let (text, color) = match outcome {
                    Outcome::UpToDate => (t!("updater.up_to_date").to_string(), design.text),
                    Outcome::Available(update) => (
                        t!("updater.available_title", version = update.version.clone()).to_string(),
                        design.accent,
                    ),
                    Outcome::Failed(message) => (message.clone(), cx.theme().danger),
                };
                v_flex()
                    .min_w_0()
                    .gap(design.px(2.0))
                    .child(line(text.clone()).text_color(color))
                    .children(last_checked_label(cx).map(|when| {
                        line(when)
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                    }))
                    .id("about-update-outcome")
                    .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
                    .into_any_element()
            }
            (None, None) => {
                line(last_checked_label(cx).unwrap_or_else(|| t!("about.not_checked").to_string()))
                    .text_color(design.muted)
                    .into_any_element()
            }
        };
        // A found update turns the check button into the way to take it, so
        // the page offers what the notification offered.
        let action = match (&status, outcome) {
            (None, Some(Outcome::Available(update))) if update.can_install() => button(
                "about-install-update",
                t!("updater.install_and_restart").to_string(),
            )
            .kind(ButtonKind::Primary)
            .size(ButtonSize::Small)
            .on_click(move |_, _, cx| {
                let update = update.clone();
                cx.defer(move |cx| crate::updater::install(update, cx));
            }),
            (None, Some(Outcome::Available(update))) => {
                button("about-download-update", t!("updater.download").to_string())
                    .kind(ButtonKind::Primary)
                    .size(ButtonSize::Small)
                    .on_click(move |_, _, cx| {
                        cx.open_with_system(Path::new(&update.notes_url));
                    })
            }
            _ => button("about-check-updates", t!("about.check_updates").to_string())
                .kind(ButtonKind::Primary)
                .size(ButtonSize::Small)
                // While a check or install runs, the button becomes the
                // spinner and refuses the click, which is what the action's own
                // re-entry guard would show as a notification anyway.
                .loading(status.is_some())
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(crate::updater::CheckForUpdates), cx);
                }),
        };
        let check = row(status_line, action.into_any_element(), false, design);
        group(
            t!("about.updates").to_string(),
            vec![check.into_any_element(), auto_check_row(design, window, cx)],
            design,
        )
        .into_any_element()
    }

    /// The licence texts the app ships under, and the notices for everything
    /// it bundles.
    fn licenses_group(&self, design: &Design, cx: &mut Context<Self>) -> AnyElement {
        let strata = row(
            t!("about.app_name").to_string().into_any_element(),
            h_flex()
                .flex_none()
                .items_center()
                .gap(design.px(8.0))
                .child(
                    div()
                        .id("about-license-mit")
                        .debug_selector(|| "about-license-mit".to_string())
                        .flex_none()
                        .child(
                            button("about-mit", t!("about.license_mit_short").to_string())
                                .kind(ButtonKind::Normal)
                                .size(ButtonSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show(Page::License(License::Mit), cx)
                                })),
                        ),
                )
                .child(
                    div().id("about-license-apache").flex_none().child(
                        button("about-apache", t!("about.license_apache_short").to_string())
                            .kind(ButtonKind::Normal)
                            .size(ButtonSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show(Page::License(License::Apache), cx)
                            })),
                    ),
                )
                .into_any_element(),
            false,
            design,
        );
        let notices = row(
            t!("about.third_party").to_string().into_any_element(),
            div()
                .id("about-third-party-show")
                .debug_selector(|| "about-third-party-show".to_string())
                .flex_none()
                .child(
                    button("about-third-party", t!("common.show").to_string())
                        .kind(ButtonKind::Normal)
                        .size(ButtonSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.show(Page::Notices, cx))),
                )
                .into_any_element(),
            true,
            design,
        );
        group(
            t!("about.licenses").to_string(),
            vec![strata.into_any_element(), notices.into_any_element()],
            design,
        )
        .into_any_element()
    }

    fn overview(
        &self,
        design: &Design,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // The dialog already pads and scrolls this column, so the page is a
        // plain column and adds nothing window-like of its own.
        v_flex()
            .w_full()
            .gap(design.px(24.0))
            .child(self.hero(design))
            .child(self.links(design, cx))
            .child(self.updates_group(design, window, cx))
            .child(self.licenses_group(design, cx))
    }

    fn document_page(
        &self,
        design: &Design,
        cx: &mut Context<Self>,
        title: String,
        body: gpui::AnyElement,
    ) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(design.px(8.0))
                    .pb(design.px(10.0))
                    .border_b_1()
                    .border_color(design.border)
                    .child(
                        div()
                            .id("about-back")
                            .debug_selector(|| "about-back".to_string())
                            .flex_none()
                            .child(
                                button("about-back-button", t!("common.back").to_string())
                                    .icon(IconName::ArrowLeft)
                                    .kind(ButtonKind::Ghost)
                                    .size(ButtonSize::Small)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.show(Page::Overview, cx)),
                                    ),
                            ),
                    )
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title)),
            )
            .child(
                div()
                    .id("about-document")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pt(design.px(12.0))
                    .text_size(px(design.tiny()))
                    .child(body),
            )
    }

    fn notices_page(&self, design: &Design, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.notices {
            Notices::Loading => {
                crate::ui::loading_state(t!("about.loading_notices").to_string(), design)
                    .into_any_element()
            }
            Notices::Found(text) => div()
                .font_family(design.mono.clone())
                .child(text.clone())
                .into_any_element(),
            Notices::Missing => v_flex()
                .gap(design.px(12.0))
                .text_color(design.text)
                .child(t!("about.third_party_missing").to_string())
                .child(
                    button(
                        "about-view-repository",
                        t!("about.view_repository").to_string(),
                    )
                    .kind(ButtonKind::Normal)
                    .size(ButtonSize::Small)
                    .on_click(|_, _, cx| cx.open_with_system(Path::new(REPOSITORY))),
                )
                .into_any_element(),
        };
        self.document_page(design, cx, t!("about.third_party").to_string(), body)
    }
}

impl Render for AboutView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let page = match self.page {
            Page::Overview => self.overview(&design, window, cx).into_any_element(),
            Page::License(license) => self
                .document_page(
                    &design,
                    cx,
                    license.title(),
                    div()
                        .font_family(design.mono.clone())
                        .child(license.text())
                        .into_any_element(),
                )
                .into_any_element(),
            Page::Notices => self.notices_page(&design, cx).into_any_element(),
        };

        v_flex()
            .size_full()
            .key_context(ABOUT_VIEW)
            .on_action(cx.listener(|_, _: &CloseAbout, window, cx| window.close_dialog(cx)))
            .text_color(design.text)
            .text_size(px(design.ui))
            .child(page)
    }
}

/// The auto-check switch, with its explanation behind an info icon the way
/// the settings pages hide theirs. The switch replaces the row the Behaviour
/// page used to carry, so the setting lives where it is used.
fn auto_check_row(design: &Design, window: &mut Window, cx: &mut App) -> AnyElement {
    let title = t!("about.auto_check").to_string();
    let description = t!("about.auto_check_description").to_string();
    let pinned = *window
        .use_keyed_state(SharedString::from(AUTO_CHECK_INFO), cx, |_, _| false)
        .read(cx);
    v_flex()
        .w_full()
        .py(design.px(9.0))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap(design.px(16.0))
                .child(
                    h_flex()
                        .min_w_0()
                        .flex_1()
                        .items_center()
                        .gap(design.px(6.0))
                        .child(title.clone())
                        .child(info_icon(
                            AUTO_CHECK_INFO,
                            description.clone().into(),
                            design,
                            window,
                            cx,
                        )),
                )
                .child(
                    div().flex_none().child(
                        Switch::new("about-auto-check")
                            .accessibility_label(title)
                            .checked(Settings::get(cx).updates.auto_check)
                            .on_click(|checked: &bool, _, cx| {
                                settings::update(cx, |settings| {
                                    crate::settings_view::auto_check_updates(settings, *checked)
                                });
                                cx.refresh_windows();
                            }),
                    ),
                ),
        )
        .when(pinned, |this| {
            this.child(
                div()
                    .pt(design.px(6.0))
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child(description),
            )
        })
        .into_any_element()
}

/// A settings row: the name on the left, the control on the right, separated
/// from the next row by the same 1px line the settings pages draw.
fn row(left: AnyElement, right: AnyElement, last: bool, design: &Design) -> Div {
    div()
        .w_full()
        .py(design.px(9.0))
        .when(!last, |this| this.border_b_1().border_color(design.border))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap(design.px(16.0))
                .child(div().flex_1().min_w_0().child(left))
                .child(div().flex_none().child(right)),
        )
}

/// The short muted semibold heading every settings page puts over its rows.
fn group_heading(text: String, design: &Design) -> Div {
    div()
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(design.muted)
        .child(text)
}

fn group(title: String, rows: Vec<AnyElement>, design: &Design) -> Div {
    v_flex()
        .gap(design.px(2.0))
        .child(group_heading(title, design))
        .children(rows)
}

/// The explanation behind the auto-check row's info icon: hover to read it,
/// click to pin it under the row for anyone not using a mouse.
fn info_icon(
    key: &'static str,
    text: SharedString,
    design: &Design,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let pinned = window.use_keyed_state(SharedString::from(key), cx, |_, _| false);
    div()
        .id(SharedString::from(format!("{key}-icon")))
        .flex_none()
        .cursor_pointer()
        .text_color(design.muted)
        .hover(|this| this.text_color(design.text))
        .child(Icon::new(IconName::Info).size(design.px(14.0)))
        .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
        .on_click(move |_, _, cx| {
            pinned.update(cx, |pinned, cx| {
                *pinned = !*pinned;
                cx.notify();
            });
        })
        .into_any_element()
}

/// When no check is in flight, the status row names when a check last ran.
fn last_checked_label(cx: &App) -> Option<String> {
    let checked = Settings::get(cx).updates.last_checked_at? as i64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(checked);
    Some(
        t!(
            "about.last_checked",
            when = crate::style::relative_time(checked, now)
        )
        .to_string(),
    )
}

fn release_notes_url() -> String {
    format!("{REPOSITORY}/releases/tag/v{VERSION}")
}

/// The line a bug report should carry: OS, architecture, and the git Strata
/// runs. The git part is left out until the probe answers, rather than showing
/// a placeholder that looks like a missing install.
fn environment_line(git: Option<&str>) -> String {
    let os = platform_name();
    let arch = std::env::consts::ARCH;
    match git {
        Some(git) => t!("about.environment_line", os = os, arch = arch, git = git).to_string(),
        None => t!("about.environment_line_no_git", os = os, arch = arch).to_string(),
    }
}

/// The plain-text block the copy button writes.
fn copy_block(git: Option<&str>) -> String {
    let git = git
        .map(str::to_owned)
        .unwrap_or_else(|| t!("about.unknown").to_string());
    t!(
        "about.copy_info_block",
        version = VERSION,
        commit = COMMIT,
        date = BUILD_DATE,
        os = platform_name(),
        arch = std::env::consts::ARCH,
        git = git
    )
    .to_string()
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(windows) {
        "Windows"
    } else {
        "Linux"
    }
}

/// How the packaged app lays out the third-party notices. The runtime picks
/// the row that matches the executable's platform; tests pass it explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Macos,
    Windows,
    Linux,
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Linux
    }
}

/// The places a package may have put `THIRD-PARTY-NOTICES.md`, in the order
/// they are tried.
///
/// A macOS bundle keeps it in `Contents/Resources`, one directory above the
/// executable; the Linux archive and the Windows zip put it beside the binary,
/// and `install.sh` moves it under `share/doc/strata`.
fn notices_candidates(platform: Platform, executable: &Path) -> Vec<PathBuf> {
    let directory = executable.parent().unwrap_or_else(|| Path::new("."));
    let beside = directory.join(NOTICES_FILE);
    match platform {
        Platform::Macos => {
            let resources = directory
                .parent()
                .map(|contents| contents.join("Resources").join(NOTICES_FILE));
            resources
                .into_iter()
                .chain(std::iter::once(beside))
                .collect()
        }
        Platform::Windows => vec![beside],
        Platform::Linux => {
            let installed = directory.parent().map(|prefix| {
                prefix
                    .join("share")
                    .join("doc")
                    .join("strata")
                    .join(NOTICES_FILE)
            });
            std::iter::once(beside).chain(installed).collect()
        }
    }
}

fn locate_notices(platform: Platform, executable: &Path) -> Option<String> {
    notices_candidates(platform, executable)
        .into_iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext, point};

    use super::*;

    fn open_view(cx: &mut TestAppContext) -> (gpui::Entity<AboutView>, VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Settings::default());
        });
        let window = cx.add_window(|_, cx| AboutView::new(cx));
        let view = window.root(cx).expect("the About view");
        let mut about = VisualTestContext::from_window(window.into(), cx);
        about.update(|window, cx| window.draw(cx).clear(cx));
        (view, about)
    }

    fn click_center(about: &mut VisualTestContext, selector: &'static str) {
        // The click needs the page laid out as it stands now, not as it stood
        // before the previous click changed the page.
        about.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = about
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was laid out"));
        about.simulate_click(
            point(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            Default::default(),
        );
        about.run_until_parked();
    }

    #[test]
    fn build_info_is_known() {
        assert!(!VERSION.is_empty(), "the crate version is empty");
        assert!(!COMMIT.is_empty(), "the embedded commit is empty");
        assert_eq!(BUILD_DATE.len(), 10, "the build date is not YYYY-MM-DD");
        assert_eq!(&BUILD_DATE[4..5], "-");
        assert_eq!(&BUILD_DATE[7..8], "-");
    }

    #[test]
    fn the_copied_block_names_the_version_and_commit() {
        let text = copy_block(Some("2.39.3"));
        assert!(text.contains(VERSION), "{text}");
        assert!(text.contains(COMMIT), "{text}");
        assert!(text.contains(BUILD_DATE), "{text}");
        assert!(text.contains("2.39.3"), "{text}");
        assert!(copy_block(None).contains(COMMIT));
    }

    #[test]
    fn the_embedded_licenses_are_the_repository_files() {
        assert_eq!(License::Mit.text(), MIT_LICENSE);
        assert_eq!(License::Apache.text(), APACHE_LICENSE);
        assert!(!MIT_LICENSE.is_empty());
        assert!(!APACHE_LICENSE.is_empty());
    }

    #[test]
    fn notices_are_looked_up_in_each_platforms_packaging_layout() {
        let macos = Path::new("/Applications/Strata.app/Contents/MacOS/strata");
        assert_eq!(
            notices_candidates(Platform::Macos, macos),
            vec![
                PathBuf::from("/Applications/Strata.app/Contents/Resources/THIRD-PARTY-NOTICES.md"),
                PathBuf::from("/Applications/Strata.app/Contents/MacOS/THIRD-PARTY-NOTICES.md"),
            ]
        );

        let windows = Path::new("/opt/strata/strata.exe");
        assert_eq!(
            notices_candidates(Platform::Windows, windows),
            vec![PathBuf::from("/opt/strata/THIRD-PARTY-NOTICES.md")]
        );

        let linux = Path::new("/usr/local/bin/strata");
        assert_eq!(
            notices_candidates(Platform::Linux, linux),
            vec![
                PathBuf::from("/usr/local/bin/THIRD-PARTY-NOTICES.md"),
                PathBuf::from("/usr/local/share/doc/strata/THIRD-PARTY-NOTICES.md"),
            ]
        );
    }

    #[gpui::test]
    fn the_status_row_names_the_last_check_once_one_ran(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(Settings::default());
        });
        cx.update(|cx| assert_eq!(last_checked_label(cx), None));

        cx.update(|cx| {
            let mut settings = Settings::default();
            settings.updates.last_checked_at = Some(1);
            cx.set_global(settings);
        });
        let label = cx.update(|cx| last_checked_label(cx));
        assert!(label.is_some(), "a stored check time draws a line");
    }

    #[gpui::test]
    fn the_mit_button_shows_its_license(cx: &mut TestAppContext) {
        let (view, mut about) = open_view(cx);
        assert_eq!(view.read_with(&about, |view, _| view.page), Page::Overview);

        click_center(&mut about, "about-license-mit");

        let (page, text) = view.read_with(&about, |view, _| (view.page, view.document_text()));
        assert_eq!(page, Page::License(License::Mit));
        assert_eq!(text.as_deref(), Some(MIT_LICENSE));
    }

    #[gpui::test]
    fn the_show_button_opens_the_notices_and_back_returns(cx: &mut TestAppContext) {
        let (view, mut about) = open_view(cx);

        click_center(&mut about, "about-third-party-show");
        assert_eq!(view.read_with(&about, |view, _| view.page), Page::Notices);

        click_center(&mut about, "about-back");
        assert_eq!(view.read_with(&about, |view, _| view.page), Page::Overview);
    }
}
