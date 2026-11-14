//! Detection of the optional ghq and GitHub CLI installations.
//!
//! Both programs are looked up once at startup on the background executor,
//! never on the UI thread, and the answer is kept in a global for the session.
//! Toggling either setting re-runs detection. A missing program is an ordinary
//! state, not an error: the UI keeps working and shows why its integration is
//! disabled.

use gpui::{App, Global};

use strata_core::tools as core_tools;
use strata_core::tools::{GhInfo, GhqInfo};

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
            (None, true) => "Detecting ghq…".to_owned(),
            (None, false) => "ghq is not installed.".to_owned(),
        }
    }

    /// Why gh cannot be used right now, naming the one command that fixes a
    /// logged-out install.
    pub fn gh_reason(&self) -> String {
        match (&self.gh, &self.gh_error, self.detecting) {
            (Some(gh), _, _) if !gh.logged_in => {
                "gh is not logged in: run gh auth login".to_owned()
            }
            (Some(_), _, _) => String::new(),
            (None, Some(error), _) => error.clone(),
            (None, None, true) => "Detecting GitHub CLI…".to_owned(),
            (None, None, false) => "gh is not installed.".to_owned(),
        }
    }
}

/// Installs the global and starts the first detection pass. Call once at
/// startup, after [`crate::settings::init`].
pub fn init(cx: &mut App) {
    cx.set_global(Tooling::default());
    redetect(cx);
}

/// Runs both detections again, off the UI thread. Called at startup and when a
/// tool's on/off setting changes.
pub fn redetect(cx: &mut App) {
    let root = Settings::get(cx).tools.ghq.root.clone();
    let generation = {
        let tooling = cx.global_mut::<Tooling>();
        tooling.generation += 1;
        tooling.detecting = true;
        tooling.generation
    };

    let task = cx.background_executor().spawn(async move {
        (
            core_tools::detect_ghq(root.as_deref()),
            core_tools::detect_gh(),
        )
    });
    cx.spawn(async move |cx| {
        let (ghq, gh) = task.await;
        cx.update_global::<Tooling, _>(|tooling, cx| {
            if tooling.generation != generation {
                return;
            }
            tooling.detecting = false;
            match ghq {
                Ok(info) => {
                    tooling.ghq = Some(info);
                    tooling.ghq_error = None;
                }
                Err(error) => {
                    tooling.ghq = None;
                    tooling.ghq_error = Some(error.to_string());
                }
            }
            match gh {
                Ok(info) => {
                    tooling.gh = Some(info);
                    tooling.gh_error = None;
                }
                Err(error) => {
                    tooling.gh = None;
                    tooling.gh_error = Some(error.to_string());
                }
            }
            cx.refresh_windows();
        });
    })
    .detach();
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
}
