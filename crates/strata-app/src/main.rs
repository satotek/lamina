//! Strata — phase-0 GPUI validation binary.
//!
//! One window, three panes, synthetic data: 200k commits in a virtualised list
//! with a canvas-drawn graph column, and a highlighted diff underneath.

mod app;
mod diff_view;
mod fake;
mod graph_canvas;
mod metrics;

use std::rc::Rc;
use std::time::Instant;

use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

fn main() {
    env_logger::init();
    let started = Instant::now();
    let repo = Rc::new(fake::FakeRepo::generate(fake::COMMIT_COUNT));
    eprintln!(
        "strata phase 0: generated {} commits and graph rows in {:?}",
        repo.commits.len(),
        started.elapsed()
    );

    application().run(move |cx: &mut App| {
        gpui_component::init(cx);

        let bounds = Bounds::centered(None, size(px(1360.0), px(860.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Strata".into()),
                    ..Default::default()
                }),
                focus: true,
                ..Default::default()
            },
            move |window, cx| {
                gpui_component::Theme::sync_system_appearance(Some(window), cx);
                let repo = repo.clone();
                cx.new(|cx| app::StrataApp::new(repo, window, cx))
            },
        )
        .expect("failed to open the Strata window");

        cx.activate(true);
    });
}
