//! Strata — a lightweight git client.
//!
//! `strata [PATH]` opens the repository containing `PATH` (the current
//! directory by default). `strata --fake [N]` renders `N` synthetic commits
//! instead, which is the phase-0 render benchmark; both modes share the same
//! commit-list and diff renderers.

mod data;
mod diff_view;
mod fake;
mod graph_canvas;
mod metrics;
mod repo_view;

use std::path::Path;

use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

use repo_view::{Launch, RepoView};

fn main() {
    env_logger::init();
    let launch = parse_args();
    let title = launch.title();

    application().run(move |cx: &mut App| {
        gpui_component::init(cx);

        let bounds = Bounds::centered(None, size(px(1360.0), px(860.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                focus: true,
                ..Default::default()
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
