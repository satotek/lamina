//! Detection of the optional ghq and GitHub CLI installations.
//!
//! Both programs are looked up on the background executor, never on the UI
//! thread, and the answer is kept in a global for the session. Detection runs
//! at startup, when a tool's setting changes and when the settings window
//! opens, so a program installed while Lamina runs is found without a restart.
//! A missing program is an ordinary state, not an error: the UI keeps working
//! and shows why its integration is disabled.

use gpui::{App, Global};

use lamina_core::tools as core_tools;
use lamina_core::tools::{GhInfo, GhqInfo};

use crate::settings::Settings;

/// The result of the last detection pass.
#[derive(Debug, Default)]
pub struct Tooling {
    pub(crate) ghq: Option<GhqInfo>,
    pub(crate) ghq_error: Option<String>,
    pub(crate) gh: Option<GhInfo>,
    pub(crate) gh_error: Option<String>,
    /// Incremented on every pass so an older, slower answer cannot replace a
    /// newer one.
    generation: u64,
    detecting: bool,
}

impl Global for Tooling {}

impl Tooling {
    /// The detection state, if [`init`] ran. Views that can be rendered in a
    /// test without the app globals use this instead of panicking.
    pub fn get(cx: &App) -> Option<&Tooling> {
        cx.try_global::<Tooling>()
    }

    pub fn ghq(&self) -> Option<&GhqInfo> {
        self.ghq.as_ref()
    }

    pub fn gh(&self) -> Option<&GhInfo> {
        self.gh.as_ref()
    }

    /// Whether the last detection pass is still running.
    pub fn detecting(&self) -> bool {
        self.detecting
    }

    /// Why ghq cannot be used right now, for a settings row or a menu label.
    pub fn ghq_reason(&self) -> String {
        match (&self.ghq_error, self.detecting) {
            (Some(error), _) => error.clone(),
            (None, true) => t!("settings.tools.detecting_ghq").to_string(),
            (None, false) => t!("settings.tools.ghq_missing").to_string(),
        }
    }

    /// Why gh cannot be used right now, naming the one command that fixes a
    /// logged-out install.
    pub fn gh_reason(&self) -> String {
        match (&self.gh, &self.gh_error, self.detecting) {
            (Some(gh), _, _) if !gh.logged_in => t!("github.gh_not_logged_in").to_string(),
            (Some(_), _, _) => String::new(),
            (None, Some(error), _) => error.clone(),
            (None, None, true) => t!("settings.tools.detecting_gh").to_string(),
            (None, None, false) => t!("settings.tools.gh_missing").to_string(),
        }
    }
}

/// Installs the global and starts the first detection pass. Call once at
/// startup, after [`crate::settings::init`].
pub fn init(cx: &mut App) {
    cx.set_global(Tooling::default());
    redetect(cx);
}

/// Runs both detections again, off the UI thread. Called at startup, when a
/// tool's on/off setting changes, and when the settings window opens.
pub fn redetect(cx: &mut App) {
    let settings = Settings::get(cx);
    let root = settings.tools.ghq.root.clone();
    let proxy = settings.proxy.clone();
    let generation = {
        let tooling = cx.global_mut::<Tooling>();
        tooling.generation += 1;
        tooling.detecting = true;
        tooling.generation
    };

    let task = cx.background_executor().spawn(async move {
        // The lookup has to happen here, next to the detection, because it is
        // what tells "not installed" from "installed but unusable".
        let ghq_installed = core_tools::find_binary("ghq").is_some();
        let gh_installed = core_tools::find_binary("gh").is_some();
        let gh_proxy = crate::proxy::resolve_policy("https://github.com", &proxy);
        (
            install_state(ghq_installed, core_tools::detect_ghq(root.as_deref())),
            install_state(gh_installed, core_tools::detect_gh_with_proxy(&gh_proxy)),
        )
    });
    cx.spawn(async move |cx| {
        let ((ghq, ghq_error), (gh, gh_error)) = task.await;
        cx.update_global::<Tooling, _>(|tooling, cx| {
            if tooling.generation != generation {
                return;
            }
            tooling.detecting = false;
            tooling.ghq = ghq;
            tooling.ghq_error = ghq_error;
            tooling.gh = gh;
            tooling.gh_error = gh_error;
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Splits a detection result into the stored tool and the message a row shows.
///
/// A program that is not on PATH is not a failure to report: each row has its
/// own localized "not installed" text, while the core's message is English and
/// names the executable. Every other failure keeps the core message, which says
/// which command failed.
fn install_state<T>(
    installed: bool,
    result: lamina_core::Result<T>,
) -> (Option<T>, Option<String>) {
    match result {
        Ok(tool) => (Some(tool), None),
        Err(_) if !installed => (None, None),
        Err(error) => (None, Some(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn gh(logged_in: bool) -> GhInfo {
        GhInfo {
            binary: PathBuf::from("/opt/homebrew/bin/gh"),
            version: "gh version 2.60.0".into(),
            hosts: if logged_in {
                vec!["github.com".into()]
            } else {
                Vec::new()
            },
            logged_in,
        }
    }

    #[test]
    fn a_missing_tool_says_it_is_not_installed() {
        let tooling = Tooling::default();
        assert_eq!(tooling.ghq_reason(), "ghq is not installed.");
        assert_eq!(tooling.gh_reason(), "gh is not installed.");
    }

    #[test]
    fn a_logged_out_gh_names_the_command_that_fixes_it() {
        let tooling = Tooling {
            gh: Some(gh(false)),
            ..Tooling::default()
        };
        assert_eq!(
            tooling.gh_reason(),
            "gh is not logged in: run gh auth login"
        );
    }

    #[test]
    fn a_detected_and_logged_in_tool_has_no_reason() {
        let tooling = Tooling {
            gh: Some(gh(true)),
            ..Tooling::default()
        };
        assert_eq!(tooling.gh_reason(), "");
    }

    #[test]
    fn a_detection_error_is_shown_instead_of_generic_advice() {
        let tooling = Tooling {
            ghq_error: Some("ghq root failed: permission denied".into()),
            ..Tooling::default()
        };
        assert_eq!(tooling.ghq_reason(), "ghq root failed: permission denied");
    }

    #[test]
    fn a_program_missing_from_path_keeps_the_localized_not_installed_text() {
        // The core reports "ghq is not installed" in English; the row must not
        // show that, because it has its own translation of the same state.
        let (ghq, error) = install_state::<GhqInfo>(
            false,
            Err(lamina_core::Error::other("ghq is not installed")),
        );
        assert!(ghq.is_none());
        assert_eq!(error, None);

        let tooling = Tooling {
            ghq_error: error,
            ..Tooling::default()
        };
        assert_eq!(tooling.ghq_reason(), t!("settings.tools.ghq_missing"));
    }

    #[test]
    fn an_installed_program_that_fails_keeps_the_core_message() {
        let (gh, error) = install_state::<GhInfo>(
            true,
            Err(lamina_core::Error::other(
                "gh auth status failed: permission denied",
            )),
        );
        assert!(gh.is_none());

        let tooling = Tooling {
            gh_error: error,
            ..Tooling::default()
        };
        assert_eq!(
            tooling.gh_reason(),
            "gh auth status failed: permission denied"
        );
    }

    #[test]
    fn a_successful_detection_stores_the_tool_and_no_error() {
        let (gh, error) = install_state(true, Ok(gh(true)));
        assert!(error.is_none());
        let tooling = Tooling {
            gh,
            ..Tooling::default()
        };
        assert_eq!(tooling.gh_reason(), "");
    }
}
