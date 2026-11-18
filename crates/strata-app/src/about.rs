//! The About window: what the binary is, how it was built, and what it bundles.
//!
//! The build revision and date are written into the binary by `build.rs`, so
//! the window never asks git or the filesystem for them at render time. The
//! git version and the third-party notices are probed on the background
//! executor; until they arrive the window draws the rest, because an About
//! window must open even when git is missing or slow.
//!
//! The licence texts are embedded, so they are always readable. The
//! third-party notices are generated per release and copied next to the
//! executable (or into `Contents/Resources` in a bundle); a development build
//! has no such file and shows a link to the repository instead.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, Context, Div, Entity,
    FocusHandle, FontWeight, Global, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
    WindowBounds, WindowDecorations, WindowOptions, actions, div, px,
};
use gpui_component::{ActiveTheme as _, TitleBar, h_flex, v_flex};
use strata_core::git_cli::Git;

use crate::menus::AboutStrata;
use crate::settings::{self, Settings};
use crate::style::{self, Design};
use crate::ui::{ButtonKind, ButtonSize, button};

/// The project page, the issue form and the release tag are derived from the
/// manifest's repository so a fork only has to change one file.
const REPOSITORY: &str = "https://github.com/satotek/strata";
const ISSUES: &str = "https://github.com/satotek/strata/issues/new";
const NOTICES_FILE: &str = "THIRD-PARTY-NOTICES.md";

const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = env!("STRATA_BUILD_COMMIT");
const BUILD_DATE: &str = env!("STRATA_BUILD_DATE");

pub(crate) const ABOUT_VIEW: &str = "AboutView";
const WIDTH: f32 = 620.0;
const HEIGHT: f32 = 640.0;
/// How long the copy button says that it copied.
const COPIED_FOR: Duration = Duration::from_secs(2);

const MIT_LICENSE: &str = include_str!("../../../LICENSE-MIT");
const APACHE_LICENSE: &str = include_str!("../../../LICENSE-APACHE");

actions!(
    strata_about,
    [
        /// Closes the About window.
        CloseAbout,
    ]
);

/// Registers the window and the action that opens it. Call once at startup.
pub fn bind(cx: &mut App) {
    cx.set_global(OpenAboutWindow::default());
    // The window is opened from a menu or palette dispatch, which already
    // borrows the window the action came from; wait until that dispatch ends.
    cx.on_action(|_: &AboutStrata, cx: &mut App| cx.defer(open));
    crate::keymap::bind_group(cx, crate::keymap::Group::Application);
}

/// The one About window, so a second command raises it instead of stacking
/// identical windows.
#[derive(Default)]
struct OpenAboutWindow {
    window: Option<AnyWindowHandle>,
    view: Option<WeakEntity<AboutView>>,
}

impl Global for OpenAboutWindow {}

fn open(cx: &mut App) {
    if let Some(view) = cx
        .try_global::<OpenAboutWindow>()
        .and_then(|window| window.view.clone())
        && let Some(view) = view.upgrade()
    {
        // Keep the page the user was reading when the window is asked for
        // again; only raise it.
        view.update(cx, |_, cx| cx.notify());
        if let Some(handle) = cx
            .try_global::<OpenAboutWindow>()
            .and_then(|window| window.window)
        {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        cx.activate(true);
        return;
    }

    let bounds = Bounds::centered(None, crate::zoom::window_size(WIDTH, HEIGHT, cx), cx);
    let slot: Rc<RefCell<Option<Entity<AboutView>>>> = Rc::new(RefCell::new(None));
    let window_slot = slot.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(style::window_titlebar(t!("about.title").to_string())),
            window_decorations: Some(WindowDecorations::Client),
            // The content is a fixed card: there is nothing to gain from a
            // resize, and the scrollable pages already handle a small screen.
            is_resizable: false,
            focus: true,
            ..TitleBar::window_options()
        },
        move |window, cx| {
            settings::apply_theme(Some(window), cx);
            let view = cx.new(|cx| AboutView::new(window, cx));
            *window_slot.borrow_mut() = Some(view.clone());
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        },
    );

    match opened {
        Ok(handle) => {
            crate::window_registry::track(handle.window_id(), cx);
            let view = slot.borrow_mut().take().map(|view| view.downgrade());
            if cx.has_global::<OpenAboutWindow>() {
                let global = cx.global_mut::<OpenAboutWindow>();
                global.window = Some(handle.into());
                global.view = view;
            }
            cx.activate(true);
        }
        Err(error) => log::warn!("cannot open the About window: {error}"),
    }
}

struct AboutView {
    focus: FocusHandle,
    page: Page,
    notices: Notices,
    git_version: Option<String>,
    copied: bool,
    /// The window follows the system appearance like every other one; this
    /// keeps the subscription alive.
    _appearance: Subscription,
    /// The appearance last applied, so unrelated setting changes do not reload
    /// the palette.
    appearance: crate::settings::Appearance,
    /// Repaints this window when another one changes the theme.
    _settings: Subscription,
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
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
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

        let executable = std::env::current_exe().ok();
        let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let task = cx.background_executor().spawn(async move {
            // The same handle the rest of the app writes with, and the same
            // `--version` probe, so the About window and the session agree on
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
            focus,
            page: Page::Overview,
            notices: Notices::Loading,
            git_version: None,
            copied: false,
            _appearance: appearance,
            appearance: current_appearance,
            _settings: settings,
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

    fn title_bar(&self, design: &Design) -> impl IntoElement {
        TitleBar::new()
            .h(design.px(style::TITLE_BAR_HEIGHT))
            .bg(design.chrome)
            .border_color(design.border)
            .child(
                h_flex()
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
                            .child(t!("about.title").to_string()),
                    ),
            )
    }

    fn header(&self, design: &Design) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap(design.px(18.0))
            .child(style::app_mark(design.scale(64.0)))
            .child(
                v_flex()
                    .gap(design.px(2.0))
                    .child(
                        div()
                            .text_size(px(design.scale(28.0)))
                            .font_weight(FontWeight::BOLD)
                            .child("Strata"),
                    )
                    .child(
                        div()
                            .text_size(px(design.small()))
                            .text_color(design.muted)
                            .child(t!("about.version", version = VERSION).to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(
                                t!("about.build", commit = COMMIT, date = BUILD_DATE).to_string(),
                            ),
                    ),
            )
    }

    fn overview(&self, design: &Design, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("about-overview")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(design.px(24.0))
            .gap(design.px(18.0))
            .child(self.header(design))
            .child(
                div()
                    .text_size(px(design.small()))
                    .child(t!("about.description").to_string()),
            )
            .child(
                v_flex()
                    .gap(design.px(5.0))
                    .child(section_heading(t!("about.environment").to_string(), design))
                    .child(
                        div()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(environment_line(self.git_version.as_deref())),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap(design.px(8.0))
                    .child(
                        button("about-github", t!("about.github").to_string())
                            .kind(ButtonKind::Normal)
                            .size(ButtonSize::Small)
                            .on_click(|_, _, cx| cx.open_with_system(Path::new(REPOSITORY))),
                    )
                    .child(
                        button("about-release-notes", t!("about.release_notes").to_string())
                            .kind(ButtonKind::Normal)
                            .size(ButtonSize::Small)
                            .on_click(|_, _, cx| {
                                cx.open_with_system(Path::new(&release_notes_url()))
                            }),
                    )
                    .child(
                        button("about-report-issue", t!("about.report_issue").to_string())
                            .kind(ButtonKind::Normal)
                            .size(ButtonSize::Small)
                            .on_click(|_, _, cx| cx.open_with_system(Path::new(ISSUES))),
                    )
                    .child(
                        button("about-check-updates", t!("about.check_updates").to_string())
                            .kind(ButtonKind::Normal)
                            .size(ButtonSize::Small)
                            .on_click(|_, window, cx| {
                                window
                                    .dispatch_action(Box::new(crate::updater::CheckForUpdates), cx);
                            }),
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
                    ),
            )
            .child(
                v_flex()
                    .gap(design.px(8.0))
                    .child(section_heading(t!("about.licenses").to_string(), design))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .items_center()
                            .gap(design.px(8.0))
                            .child(
                                div()
                                    .text_size(px(design.tiny()))
                                    .text_color(design.muted)
                                    .child(
                                        t!("about.license", license = "MIT OR Apache-2.0")
                                            .to_string(),
                                    ),
                            )
                            .child(
                                div()
                                    .id("about-license-mit")
                                    .debug_selector(|| "about-license-mit".to_string())
                                    .flex_none()
                                    .child(
                                        button("about-mit", t!("about.license_mit").to_string())
                                            .kind(ButtonKind::Normal)
                                            .size(ButtonSize::Small)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.show(Page::License(License::Mit), cx)
                                            })),
                                    ),
                            )
                            .child(
                                div().id("about-license-apache").flex_none().child(
                                    button("about-apache", t!("about.license_apache").to_string())
                                        .kind(ButtonKind::Normal)
                                        .size(ButtonSize::Small)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show(Page::License(License::Apache), cx)
                                        })),
                                ),
                            ),
                    ),
            )
            .child(
                div().flex_none().child(
                    button("about-third-party", t!("about.third_party").to_string())
                        .kind(ButtonKind::Normal)
                        .size(ButtonSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.show(Page::Notices, cx))),
                ),
            )
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
                    .gap(design.px(12.0))
                    .px(design.px(20.0))
                    .py(design.px(12.0))
                    .border_b_1()
                    .border_color(design.border)
                    .child(
                        button("about-back", t!("common.back").to_string())
                            .kind(ButtonKind::Ghost)
                            .size(ButtonSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.show(Page::Overview, cx))),
                    )
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title)),
            )
            .child(
                div()
                    .id("about-document")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(design.px(20.0))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let page = match self.page {
            Page::Overview => self.overview(&design, cx).into_any_element(),
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

        let mut root = v_flex()
            .size_full()
            .key_context(ABOUT_VIEW)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &CloseAbout, window, _| window.remove_window()))
            .bg(design.background)
            .text_color(design.text)
            .text_size(px(design.ui))
            .child(self.title_bar(&design))
            .child(page)
            .child(
                h_flex()
                    .flex_none()
                    .justify_end()
                    .items_center()
                    .h(design.px(48.0))
                    .px(design.px(20.0))
                    .border_t_1()
                    .border_color(design.border)
                    .bg(design.chrome)
                    .child(
                        button("about-close", t!("common.close").to_string())
                            .kind(ButtonKind::Normal)
                            .size(ButtonSize::Medium)
                            .on_click(|_, window, _| window.remove_window()),
                    ),
            );
        // This window is a root of its own, so the CJK chain has to be set
        // here as well as on the main window's.
        root.text_style().font_fallbacks = Some(style::font_fallbacks());
        root
    }
}

fn section_heading(text: String, design: &Design) -> Div {
    div()
        .text_size(px(design.tiny()))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(design.muted)
        .child(text)
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
    use gpui::{Render, TestAppContext, VisualTestContext, point};

    use super::*;

    struct Probe;

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
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
    fn dispatching_about_opens_the_view_and_the_mit_button_shows_its_license(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            bind(cx);
        });

        let window = cx.add_window(|_, _| Probe);
        window
            .update(cx, |_, window, _| window.activate_window())
            .expect("the probe window");
        let mut main = VisualTestContext::from_window(window.into(), cx);
        main.update(|window, cx| window.draw(cx).clear(cx));

        main.dispatch_action(AboutStrata);
        main.run_until_parked();

        let (about_window, view) = main.cx.update(|cx| {
            let open = cx
                .try_global::<OpenAboutWindow>()
                .expect("the About window is registered");
            (
                open.window.expect("the About window opened"),
                open.view
                    .clone()
                    .expect("the About view was recorded")
                    .upgrade()
                    .expect("the About view is alive"),
            )
        });
        assert_eq!(view.read_with(&main, |view, _| view.page), Page::Overview);

        let mut about = VisualTestContext::from_window(about_window, &main.cx);
        about.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = about
            .debug_bounds("about-license-mit")
            .expect("the MIT licence button was laid out");
        about.simulate_click(
            point(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            Default::default(),
        );
        about.run_until_parked();

        let (page, text) = view.read_with(&about, |view, _| (view.page, view.document_text()));
        assert_eq!(page, Page::License(License::Mit));
        assert_eq!(text.as_deref(), Some(MIT_LICENSE));
    }
}
