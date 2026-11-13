//! Strata — a lightweight git client.
//!
//! `strata [PATH]` opens the repository containing `PATH` (the current
//! directory by default). `strata --fake [N]` renders `N` synthetic commits
//! instead, which is the phase-0 render benchmark; both modes share the same
//! commit-list and diff renderers.

mod actions;
mod data;
mod diff_view;
mod fake;
mod graph_canvas;
mod metrics;
mod repo_view;
mod worktree_panel;

use std::path::Path;

use gpui::{
    App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions,
    px, size,
};
use gpui_component::TitleBar;
use gpui_platform::application;

use repo_view::{Launch, RepoView};

fn main() {
    env_logger::init();
    let launch = parse_args();
    let title = launch.title();

    application()
        .with_assets(gpui_kit_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            actions::bind(cx);
            worktree_panel::bind(cx);

            let bounds = Bounds::centered(None, size(px(1360.0), px(860.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // The app draws its own title bar (with the repository and
                    // branch in it) so the window reads as one surface on every
                    // platform. The title is still set for the taskbar and window
                    // switcher.
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
                    gpui_component::Theme::sync_system_appearance(Some(window), cx);
                    cx.new(|cx| RepoView::new(launch, window, cx))
                },
            )
            .expect("failed to open the Strata window");

            cx.activate(true);
        });
}

fn parse_args() -> Launch {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        return Launch::open(Path::new("."));
    };

    match first.as_str() {
        "--fake" => {
            let count = args
                .next()
                .and_then(|arg| arg.parse().ok())
                .unwrap_or(fake::COMMIT_COUNT);
            Launch::fake(count)
        }
        "-h" | "--help" => {
            println!(
                "usage: strata [PATH]\n       strata --fake [N]\n\n\
                 PATH defaults to the current directory.\n\
                 --fake renders N synthetic commits (default {}) for the render benchmark.",
                fake::COMMIT_COUNT
            );
            std::process::exit(0);
        }
        path => Launch::open(Path::new(path)),
    }
}
