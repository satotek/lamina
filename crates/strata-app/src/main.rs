//! Strata — a lightweight git client.
//!
//! `strata [PATH]` opens the repository containing `PATH`. `strata --fake [N]`
//! renders `N` synthetic commits instead, which is the phase-0 render
//! benchmark; both modes share the same commit-list and diff renderers.
//! `strata --askpass <prompt>` is git's credential helper mode and runs before
//! anything else, so it needs no display and no settings. git itself starts the
//! executable as `strata <prompt>` (with the channel in the environment); the
//! explicit form is for tests and for running it by hand.
//!
//! Started without a path, Strata reopens the last repository when the settings
//! ask it to and it is still there, and otherwise shows the welcome screen:
//! open a repository from the system chooser or pick one from the recent list.
//! A path that is not a repository keeps the error screen instead of exiting.

// A console-subsystem executable makes Windows open a console window next to
// the app on every launch. Debug builds keep it for their log output. git's
// askpass calls still work: the pipes git hands the child are inherited
// whichever subsystem the executable declares.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

// The interface catalogue. `t!` resolves the module-level `_rust_i18n_t` this
// macro generates, which is why both statements live at the crate root.
#[macro_use]
extern crate rust_i18n;

rust_i18n::i18n!("locales", fallback = "en");

mod about;
mod actions;
mod activity;
mod assets;
mod autofetch;
mod branch_switcher;
mod branches;
mod clone_dialog;
mod commands;
mod compare;
mod component_theme;
mod conflicts;
mod data;
mod debug_launch;
mod diff_view;
mod external_tools;
mod fake;
mod file_history;
mod fonts;
mod git_config_view;
mod github;
mod graph_canvas;
mod i18n;
mod image_diff;
mod keymap;
mod menus;
mod merge_editor;
mod metrics;
mod palette;
mod picker;
mod pull_requests;
mod recent;
mod remote;
mod repo_view;
mod scrollbar;
mod search;
mod settings;
mod settings_view;
mod shortcuts;
mod stash;
mod style;
mod tabs;
#[cfg(feature = "terminal")]
mod terminal_panel;
mod tools;
mod ui;
mod workspace;
mod worktree_panel;
mod zoom;

use gpui::{
    App, AppContext as _, Bounds, WindowBounds, WindowDecorations, WindowOptions, px, size,
};
use gpui_component::{Root, TitleBar};
use gpui_platform::application;

use debug_launch::{DebugLaunch, StartMode};
use repo_view::Launch;
use style::MIN_WINDOW_SIZE;
use workspace::Workspace;

fn main() {
    // git starts this executable as its askpass program while it holds a
    // credential prompt. That mode must not touch the display server, the
    // settings or a window, so it is checked before anything else.
    if let Some(code) = strata_core::askpass::helper_main(std::env::args_os().skip(1)) {
        std::process::exit(code);
    }
    env_logger::init();

    // A launch from Finder, the Dock or a desktop file inherits the minimal
    // PATH the session started with, so Homebrew, Nix and ~/.local/bin tools
    // are invisible and `git` resolves to the system copy. The login shell
    // knows the real list; ask it once, before any thread exists that could
    // have inherited the old value.
    #[cfg(unix)]
    inherit_login_shell_path();

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

    let app = application().with_assets(assets::AppAssets);

    // macOS keeps the app alive after the last window closes; the dock icon is
    // how the user asks for a window again. The handler belongs to the
    // `Application`, so it is registered before `run`.
    #[cfg(target_os = "macos")]
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_start(StartMode::Welcome, DebugLaunch::default(), cx);
        } else {
            cx.activate(true);
        }
    });

    app.run(move |cx: &mut App| {
        gpui_component::init(cx);
        style::theme::init(cx);
        settings::init(cx);
        i18n::apply(cx);
        tools::init(cx);
        keymap::init(cx);
        commands::init(cx);
        workspace::init(cx);
        actions::bind(cx);
        settings_view::bind(cx);
        git_config_view::bind(cx);
        worktree_panel::bind(cx);
        branches::bind(cx);
        branch_switcher::bind(cx);
        conflicts::bind(cx);
        remote::bind(cx);
        search::bind(cx);
        picker::bind(cx);
        palette::bind(cx);
        about::bind(cx);
        zoom::bind(cx);
        shortcuts::bind(cx);
        menus::bind(cx);
        #[cfg(feature = "terminal")]
        terminal_panel::bind(cx);

        // User bindings go on top of every default, so a keymap entry wins a
        // tied keystroke and a null entry can unbind a default.
        commands::bind_keys(cx);
        keymap::apply_user(cx);
        keymap::watch(cx);
        commands::watch(cx);
        style::theme::watch(cx);
        // The bar's key equivalents are read from the keymap, so it is built
        // after every default and the user's file have been applied.
        menus::install(cx);

        open_start(start, debug, cx);
        cx.activate(true);
        // A file that could not be parsed leaves the app running: the window
        // says which line it was, and the settings page keeps the message.
        keymap::report_to_window(cx);
        commands::report_to_window(cx);
    });
}

#[cfg(unix)]
fn inherit_login_shell_path() {
    let path = strata_core::env_path::login_shell_path();
    // SAFETY: the GPUI application, the background executor and the logger's
    // writer are all created after this. The only other thread that can exist
    // is the login-shell query's pipe reader, which never touches the
    // environment, so nothing reads it while it is replaced.
    unsafe { std::env::set_var("PATH", path) };
}

fn requested_window_size(debug: &DebugLaunch) -> (f32, f32) {
    if let Some(requested) = debug.window_size {
        eprintln!(
            "debug-launch: applied --window-size {}x{}",
            requested.width, requested.height
        );
        (requested.width as f32, requested.height as f32)
    } else {
        (1360.0, 860.0)
    }
}

fn open_start(start: StartMode, debug: DebugLaunch, cx: &mut App) {
    match start {
        StartMode::Gallery => open_window(None, true, debug, cx),
        StartMode::Path(path) => open_window(Some(Launch::open(&path)), false, debug, cx),
        StartMode::Compare(paths) => {
            let window_size = requested_window_size(&debug);
            match compare::validate_ordered_paths(&paths) {
                Ok(target) => {
                    workspace::open_compare_window(target, window_size, cx);
                }
                Err(error) => {
                    let message = error
                        .message()
                        .unwrap_or_else(|| "invalid paths for --compare".to_owned());
                    eprintln!("strata: --compare: {message}");
                    open_window(None, false, debug, cx);
                }
            }
        }
        StartMode::Fake(count) => open_window(Some(Launch::fake(count)), false, debug, cx),
        StartMode::Welcome => {
            let restored = workspace::restored_windows(cx);
            if restored.is_empty() {
                open_window(None, false, debug, cx);
                return;
            }
            let window_size = requested_window_size(&debug);
            let mut first_debug = Some(debug);
            for window in restored {
                let win_debug = first_debug.take().unwrap_or_default();
                workspace::open_tabs_window(window.tabs, window.active, win_debug, window_size, cx);
            }
        }
    }
}

fn open_window(launch: Option<Launch>, gallery: bool, mut debug: DebugLaunch, cx: &mut App) {
    // The gallery has no view to own a dialog, so the app opens the dialogs it
    // hosts itself once the window exists. A `window.defer` cannot do it: its
    // callback runs before GPUI records the window handle, so the `update` that
    // would open the dialog fails.
    let window_dialog = if gallery {
        match debug.dialog.take() {
            Some(dialog) if dialog.is_window_level() => Some(dialog),
            other => {
                if let Some(dialog) = other {
                    eprintln!(
                        "debug-launch: error: --dialog {} cannot be used with --gallery",
                        dialog.name()
                    );
                }
                None
            }
        }
    } else {
        None
    };

    let title = if gallery {
        t!("app.gallery_title").to_string()
    } else {
        launch
            .as_ref()
            .map_or_else(|| t!("app.name").to_string(), Launch::title)
    };

    let window_size = if let Some(requested) = debug.window_size {
        eprintln!(
            "debug-launch: applied --window-size {}x{}",
            requested.width, requested.height
        );
        size(px(requested.width as f32), px(requested.height as f32))
    } else {
        size(px(1360.0), px(860.0))
    };
    // The same minimum in a debug launch as in a release one. Letting
    // `--window-size` lower it hid the layouts that break below it; a check
    // that needs a smaller window can still request one, because X11 treats
    // the minimum as a hint for the window manager.
    let min_size = size(px(MIN_WINDOW_SIZE.0), px(MIN_WINDOW_SIZE.1));

    let bounds = Bounds::centered(None, window_size, cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // The app draws its own title bar (with the repository and branch in
        // it) so the window reads as one surface on every platform. The
        // title is still set for the taskbar and window switcher.
        titlebar: Some(crate::style::window_titlebar(title)),
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

    let opened = cx.open_window(options, move |window, cx| {
        settings::apply_theme(Some(window), cx);
        if gallery {
            if debug.has_repo_actions() {
                eprintln!("debug-launch: error: repository options cannot be used with --gallery");
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
    });

    let handle = opened.expect("failed to open the Strata window");
    if let Some(dialog) = window_dialog {
        debug_launch::open_window_dialog(handle.into(), dialog, cx);
    }
}

/// The X11 window icon. Wayland draws its icon from the desktop file, and
/// macOS and Windows take theirs from the bundle or the embedded resource, so
/// this is the one platform where the app has to hand the pixels over.
#[cfg(target_os = "linux")]
pub(crate) fn linux_window_icon() -> Option<std::sync::Arc<image::RgbaImage>> {
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
