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
mod debug_launch;
mod diff_view;
mod external_tools;
mod fake;
mod file_history;
mod graph_canvas;
mod metrics;
mod palette;
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
mod ui;
mod workspace;
mod worktree_panel;

use gpui::{
    App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions,
    px, size,
};
use gpui_component::{Root, TitleBar};
use gpui_platform::application;

use debug_launch::{DebugDialog, DebugLaunch, StartMode};
use repo_view::Launch;
use workspace::Workspace;

fn main() {
    env_logger::init();
    let parsed = debug_launch::parse_args();
    if parsed.help {
        println!("{}", debug_launch::help_text());
        std::process::exit(0);
    }
    for error in &parsed.errors {
        eprintln!("strata: {error}");
    }

    let start = parsed.start;
    let debug = parsed.debug;

    let app = application().with_assets(gpui_kit_assets::Assets);

    // macOS keeps the app alive after the last window closes; the dock icon is
    // how the user asks for a window again. The handler belongs to the
    // `Application`, so it is registered before `run`.
    #[cfg(target_os = "macos")]
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_window(StartMode::Welcome, DebugLaunch::default(), cx);
        } else {
            cx.activate(true);
        }
    });

    app.run(move |cx: &mut App| {
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
        palette::bind(cx);

        open_window(start, debug, cx);

        cx.activate(true);
    });
}

fn open_window(start: StartMode, debug: DebugLaunch, cx: &mut App) {
    let gallery = matches!(start, StartMode::Gallery);
    let launch = match start {
        StartMode::Path(path) => Some(Launch::open(&path)),
        StartMode::Fake(count) => Some(Launch::fake(count)),
        StartMode::Gallery => None,
        StartMode::Welcome => workspace::restored_launch(cx),
    };
    let title = if gallery {
        "Strata UI gallery".to_owned()
    } else {
        launch
            .as_ref()
            .map_or_else(|| "Strata".to_owned(), Launch::title)
    };

    let (window_size, min_size) = if let Some(requested) = debug.window_size {
        eprintln!(
            "debug-launch: applied --window-size {}x{}",
            requested.width, requested.height
        );
        let width = requested.width as f32;
        let height = requested.height as f32;
        (
            size(px(width), px(height)),
            size(px(width.min(720.0)), px(height.min(480.0))),
        )
    } else {
        (size(px(1360.0), px(860.0)), size(px(720.0), px(480.0)))
    };

    let bounds = Bounds::centered(None, window_size, cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // The app draws its own title bar (with the repository and branch in
        // it) so the window reads as one surface on every platform. The
        // title is still set for the taskbar and window switcher.
        titlebar: Some(TitlebarOptions {
            title: Some(title.into()),
            ..TitleBar::title_bar_options()
        }),
        window_decorations: Some(WindowDecorations::Client),
        window_min_size: Some(min_size),
        focus: true,
        // X11 and Wayland group windows by this id and match them to a desktop
        // file, so it has the name of packaging/linux/strata.desktop. macOS
        // takes its identity from the bundle's Info.plist instead.
        app_id: Some("strata".to_owned()),
        ..TitleBar::window_options()
    };
    // Only X11 needs the pixels: Wayland reads the icon from the desktop file.
    #[cfg(target_os = "linux")]
    let options = {
        let mut options = options;
        options.icon = linux_window_icon();
        options
    };

    cx.open_window(options, move |window, cx| {
        settings::apply_theme(Some(window), cx);
        if gallery {
            if debug.has_repo_actions() {
                eprintln!("debug-launch: error: repository options cannot be used with --gallery");
            }
            if let Some(dialog) = debug.dialog {
                match dialog {
                    DebugDialog::Settings | DebugDialog::Palette => {
                        let handle = window.window_handle();
                        window.defer(cx, move |_, cx| {
                            let _ = handle.update(cx, |_, window, cx| {
                                debug_launch::dispatch_window_dialog(dialog, window, cx);
                            });
                        });
                    }
                    _ => {
                        eprintln!(
                            "debug-launch: error: --dialog {} cannot be used with --gallery",
                            dialog.name()
                        );
                    }
                }
            }
            // The gallery is a window of its own: it reads the theme and the
            // settings and nothing else, so it opens without a repository.
            let content = cx.new(|cx| ui::gallery::Gallery::new(window, cx));
            cx.new(|cx| Root::new(content, window, cx))
        } else {
            // The workspace owns whichever view fills the window, so the
            // picker can replace a repository (and its session) in place.
            let content = cx.new(|cx| Workspace::new_with_debug(launch, debug, window, cx));
            // Root hosts gpui-component's dialog, notification and tooltip
            // layers and draws the client-side window border.
            cx.new(|cx| Root::new(content, window, cx))
        }
    })
    .expect("failed to open the Strata window");
}

/// The X11 window icon. Wayland draws its icon from the desktop file, and
/// macOS and Windows take theirs from the bundle or the embedded resource, so
/// this is the one platform where the app has to hand the pixels over.
#[cfg(target_os = "linux")]
fn linux_window_icon() -> Option<std::sync::Arc<image::RgbaImage>> {
    const PNG: &[u8] = include_bytes!("../resources/icon/hicolor/128.png");

    // A broken icon must not keep the window from opening.
    match image::load_from_memory_with_format(PNG, image::ImageFormat::Png) {
        Ok(icon) => Some(std::sync::Arc::new(icon.into_rgba8())),
        Err(error) => {
            log::warn!("could not decode the window icon: {error}");
            None
        }
    }
}
