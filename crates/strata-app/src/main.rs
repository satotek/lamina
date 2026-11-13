//! Strata — a lightweight git client.
//!
//! `strata [PATH]` opens the repository containing `PATH`. `strata --fake [N]`
//! renders `N` synthetic commits instead, which is the phase-0 render
//! benchmark; both modes share the same commit-list and diff renderers.
//!
//! Started without a path, Strata reopens the last repository when the settings
//! ask it to and it is still there, and otherwise shows the welcome screen:
//! open a repository from the system chooser or pick one from the recent list.
//! A path that is not a repository keeps the error screen instead of exiting.

mod actions;
mod autofetch;
mod branches;
mod conflicts;
mod data;
mod diff_view;
mod fake;
mod file_history;
mod graph_canvas;
mod metrics;
mod picker;
mod recent;
mod remote;
mod repo_view;
mod scrollbar;
mod search;
mod settings;
mod settings_view;
mod stash;
mod style;
mod workspace;
mod worktree_panel;

use std::path::PathBuf;

use gpui::{
    App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions,
    px, size,
};
use gpui_component::{Root, TitleBar};
use gpui_platform::application;

use repo_view::Launch;
use workspace::Workspace;

fn main() {
    env_logger::init();
    let start = parse_args();

    application()
        .with_assets(gpui_kit_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            settings::init(cx);
            actions::bind(cx);
            settings_view::bind(cx);
            worktree_panel::bind(cx);
            branches::bind(cx);
            conflicts::bind(cx);
            remote::bind(cx);
            search::bind(cx);
            picker::bind(cx);

            open_window(start, cx);

            // macOS keeps the app alive after the last window closes; the dock
            // icon is how the user asks for a window again.
            #[cfg(target_os = "macos")]
            cx.on_reopen(|cx| {
                if cx.windows().is_empty() {
                    open_window(Start::Welcome, cx);
                } else {
                    cx.activate(true);
                }
            });

            cx.activate(true);
        });
}

/// What the command line asked the first window to show.
enum Start {
    /// `strata [PATH]`: a repository, or the error screen for a bad path.
    Path(PathBuf),
    /// `strata --fake [N]`: the synthetic render benchmark.
    Fake(usize),
    /// No argument: the last repository when restoring is on, else the welcome
    /// screen.
    Welcome,
}

fn open_window(start: Start, cx: &mut App) {
    let launch = match start {
        Start::Path(path) => Some(Launch::open(&path)),
        Start::Fake(count) => Some(Launch::fake(count)),
        Start::Welcome => workspace::restored_launch(cx),
    };
    let title = launch
        .as_ref()
        .map_or_else(|| "Strata".to_owned(), Launch::title);

    let bounds = Bounds::centered(None, size(px(1360.0), px(860.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // The app draws its own title bar (with the repository and branch in
            // it) so the window reads as one surface on every platform. The
            // title is still set for the taskbar and window switcher.
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..TitleBar::title_bar_options()
            }),
            window_decorations: Some(WindowDecorations::Client),
            window_min_size: Some(size(px(720.0), px(480.0))),
            focus: true,
            ..TitleBar::window_options()
        },
        move |window, cx| {
            settings::apply_theme(Some(window), cx);
            {
                // The workspace owns whichever view fills the window, so the
                // picker can replace a repository (and its session) in place.
                let content = cx.new(|cx| Workspace::new(launch, window, cx));
                // Root hosts gpui-component's dialog, notification and tooltip
                // layers and draws the client-side window border.
                cx.new(|cx| Root::new(content, window, cx))
            }
        },
    )
    .expect("failed to open the Strata window");
}

fn parse_args() -> Start {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        return Start::Welcome;
    };

    match first.as_str() {
        "--fake" => {
            let count = args
                .next()
                .and_then(|arg| arg.parse().ok())
                .unwrap_or(fake::COMMIT_COUNT);
            Start::Fake(count)
        }
        "-h" | "--help" => {
            println!(
                "usage: strata [PATH]\n       strata --fake [N]\n\n\
                 Without PATH, Strata reopens the last repository when restoring is enabled\n\
                 and shows the welcome screen otherwise.\n\
                 --fake renders N synthetic commits (default {}) for the render benchmark.",
                fake::COMMIT_COUNT
            );
            std::process::exit(0);
        }
        path => Start::Path(PathBuf::from(path)),
    }
}
