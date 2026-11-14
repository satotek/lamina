//! User-defined commands from `commands.json`.
//!
//! A command is an argv vector, not a shell line: it is spawned directly, so a
//! file name with a space stays one argument and nothing is expanded unless
//! the user writes `["sh", "-c", …]` themselves. The variables `${repo}`,
//! `${branch}`, `${commit}`, `${file}` and `${remote}` are filled from the
//! current selection; a command whose variables cannot be resolved is shown
//! disabled with the reason rather than failing after it starts.
//!
//! Commands appear in the command palette and under a "Custom" group in the
//! matching context menus. Output goes to the repository's command log, so the
//! status line names what ran and the log keeps stdout and stderr.

use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui::{App, Global, KeyBinding, KeyBindingContextPredicate, Keystroke, WeakEntity, Window};
use gpui_component::WindowExt as _;
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use serde::Deserialize;
use strata_core::git_cli::{CommandLog, CommandRecord};

use crate::actions::REPO_VIEW;
use crate::settings;

/// Where a custom command may appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    /// Needs no selection, so it is offered everywhere.
    #[default]
    Repository,
    Branch,
    Commit,
    File,
}

impl Context {
    pub fn label(self) -> &'static str {
        match self {
            Context::Repository => "repository",
            Context::Branch => "branch",
            Context::Commit => "commit",
            Context::File => "file",
        }
    }
}

/// One command as read from `commands.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CommandSpec {
    pub title: String,
    /// Program and arguments. No shell is involved.
    pub command: Vec<String>,
    #[serde(default)]
    pub context: Context,
    /// Ask before running.
    #[serde(default)]
    pub confirm: bool,
    /// Optional keystroke, in the same syntax as `keymap.json`.
    #[serde(default)]
    pub key: Option<String>,
}

/// A loaded command, in file order.
#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub spec: CommandSpec,
}

/// The loaded commands and what the last load reported.
#[derive(Default)]
pub struct CommandState {
    pub path: Option<PathBuf>,
    pub commands: Vec<CustomCommand>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    modified: Option<SystemTime>,
}

impl Global for CommandState {}

impl CommandState {
    pub fn get(cx: &App) -> &CommandState {
        cx.global::<CommandState>()
    }
}

/// The values the variables expand to, as the current view sees them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Variables {
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub file: Option<String>,
    pub remote: Option<String>,
}

/// The command file next to `settings.json`.
pub fn commands_file() -> Option<PathBuf> {
    settings::config_file("commands.json")
}

/// Reads the file into the global. Call once at startup.
pub fn init(cx: &mut App) {
    cx.set_global(CommandState::default());
    load(cx);
}

fn load(cx: &mut App) {
    let Some(path) = commands_file() else {
        return;
    };
    let os = cx.platform_info().os;
    let modified = std::fs::metadata(&path)
        .ok()
        .and_then(|meta| meta.modified().ok());
    let mut state = CommandState {
        path: Some(path.clone()),
        modified,
        ..CommandState::default()
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => match parse(&content) {
            Ok((mut commands, mut warnings)) => {
                for (index, command) in commands.iter().enumerate() {
                    if commands
                        .iter()
                        .take(index)
                        .any(|other| other.spec.title == command.spec.title)
                    {
                        warnings.push(format!("duplicate title `{}`", command.spec.title));
                    }
                    if let Some(key) = &command.spec.key
                        && Keystroke::parse_with_os(key, os).is_err()
                    {
                        warnings.push(format!(
                            "{}: `{key}` is not a keystroke",
                            command.spec.title
                        ));
                    }
                }
                state.commands = std::mem::take(&mut commands);
                state.warnings = warnings;
            }
            Err(error) => state.errors.push(error),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => state.errors.push(format!("cannot read: {error}")),
    }
    for problem in state.errors.iter().chain(state.warnings.iter()) {
        log::warn!("{}: {problem}", path.display());
    }
    *cx.global_mut::<CommandState>() = state;
}

/// Parses the file, or names the line it could not be parsed at.
fn parse(content: &str) -> Result<(Vec<CustomCommand>, Vec<String>), String> {
    let specs: Vec<CommandSpec> =
        serde_json::from_str(content).map_err(|error| format!("line {}: {error}", error.line()))?;
    let mut commands = Vec::with_capacity(specs.len());
    let mut warnings = Vec::new();
    for (index, mut spec) in specs.into_iter().enumerate() {
        let number = index + 1;
        if spec.title.trim().is_empty() {
            warnings.push(format!("entry {number}: `title` is empty"));
            continue;
        }
        if spec.command.is_empty() {
            warnings.push(format!("{}: `command` is empty", spec.title));
            continue;
        }
        if spec.command[0].is_empty() {
            warnings.push(format!("{}: the program is empty", spec.title));
            continue;
        }
        spec.title = spec.title.trim().to_owned();
        commands.push(CustomCommand { spec });
    }
    Ok((commands, warnings))
}

/// Binds the optional keys of loaded commands, inside the repository view.
pub fn bind_keys(cx: &mut App) {
    let os = cx.platform_info().os;
    let state = cx.global::<CommandState>();
    let bindings: Vec<KeyBinding> = state
        .commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let key = command.spec.key.as_deref()?;
            if Keystroke::parse_with_os(key, os).is_err() {
                return None;
            }
            let context = KeyBindingContextPredicate::parse(REPO_VIEW)
                .ok()
                .map(Rc::new);
            KeyBinding::load(
                key,
                Box::new(RunCustomCommand { index }),
                context,
                false,
                None,
                cx,
            )
            .ok()
        })
        .collect();
    cx.bind_keys(bindings);
}

/// Re-reads `commands.json` and rebuilds the keymap.
pub fn reload(cx: &mut App) {
    load(cx);
    crate::keymap::reload(cx);
}

/// Watches the file and reloads after an editor finishes writing it.
pub fn watch(cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut last = cx.update(|cx| cx.global::<CommandState>().modified);
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(700))
                .await;
            let Some(modified) = cx.update(|_| {
                commands_file()
                    .and_then(|path| std::fs::metadata(path).ok())
                    .and_then(|meta| meta.modified().ok())
            }) else {
                return;
            };
            if last == Some(modified) {
                continue;
            }
            last = Some(modified);
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            cx.update(|cx| {
                reload(cx);
                let problems = {
                    let state = CommandState::get(cx);
                    state
                        .errors
                        .iter()
                        .chain(state.warnings.iter())
                        .cloned()
                        .collect::<Vec<_>>()
                };
                if problems.is_empty() {
                    return;
                }
                if let Some(handle) = cx.active_window() {
                    let message = problems.join("\n");
                    let _ = handle.update(cx, |_, window, cx| {
                        window.push_notification(
                            gpui_component::notification::Notification::warning(message),
                            cx,
                        );
                    });
                }
            });
        }
    })
    .detach();
}

/// Shows load problems in the active window, if there is one.
pub fn report_to_window(cx: &mut App) {
    let state = CommandState::get(cx);
    let problems: Vec<String> = state
        .errors
        .iter()
        .chain(state.warnings.iter())
        .cloned()
        .collect();
    if problems.is_empty() {
        return;
    }
    let Some(handle) = cx.active_window() else {
        return;
    };
    let message = problems.join("\n");
    let _ = handle.update(cx, |_, window, cx| {
        window.push_notification(
            gpui_component::notification::Notification::warning(message),
            cx,
        );
    });
}

/// The command action: an index into the loaded list.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = strata, no_json)]
pub struct RunCustomCommand {
    pub index: usize,
}

/// Replaces `${name}` in every argument. Unknown or unresolved variables are
/// an error, so a command never runs half-expanded.
pub fn expand(argv: &[String], variables: &Variables) -> Result<Vec<String>, String> {
    argv.iter()
        .map(|argument| {
            let mut out = String::with_capacity(argument.len());
            let mut rest = argument.as_str();
            while let Some(start) = rest.find("${") {
                out.push_str(&rest[..start]);
                let after = &rest[start + 2..];
                let Some(end) = after.find('}') else {
                    return Err(format!("unterminated variable in `{argument}`"));
                };
                let name = &after[..end];
                out.push_str(value(name, variables)?);
                rest = &after[end + 1..];
            }
            out.push_str(rest);
            Ok(out)
        })
        .collect()
}

fn value<'a>(name: &str, variables: &'a Variables) -> Result<&'a str, String> {
    let (slot, missing) = match name {
        "repo" => (&variables.repo, "no repository is open"),
        "branch" => (&variables.branch, "no branch is checked out"),
        "commit" => (&variables.commit, "no commit is selected"),
        "file" => (&variables.file, "no file is selected"),
        "remote" => (&variables.remote, "the branch has no upstream remote"),
        other => return Err(format!("unknown variable `${{{other}}}`")),
    };
    slot.as_deref().ok_or_else(|| missing.to_owned())
}

/// The command line as the confirmation dialog shows it. Arguments with spaces
/// are quoted for reading; the process still receives them verbatim.
pub fn display_line(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| {
            if argument.is_empty()
                || argument
                    .chars()
                    .any(|character| character.is_whitespace() || character == '"')
            {
                format!("\"{}\"", argument.replace('"', "\\\""))
            } else {
                argument.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Adds the "Custom" submenu for `context` when any command matches.
pub fn submenu(
    menu: PopupMenu,
    context: Context,
    variables: &Variables,
    window: &mut Window,
    cx: &mut gpui::Context<PopupMenu>,
) -> PopupMenu {
    let state = CommandState::get(cx);
    let entries: Vec<(usize, String, Option<String>)> = state
        .commands
        .iter()
        .enumerate()
        .filter(|(_, command)| {
            matches!(command.spec.context, Context::Repository) || command.spec.context == context
        })
        .map(|(index, command)| {
            let reason = expand(&command.spec.command, variables).err();
            (index, command.spec.title.clone(), reason)
        })
        .collect();
    if entries.is_empty() {
        return menu;
    }
    menu.submenu("Custom", window, cx, move |menu, _, _| {
        entries.iter().fold(menu, |menu, (index, title, reason)| {
            let label = match reason {
                Some(reason) => format!("{title} ({reason})"),
                None => title.clone(),
            };
            let index = *index;
            menu.item(
                PopupMenuItem::new(label)
                    .disabled(reason.is_some())
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(RunCustomCommand { index }), cx);
                    }),
            )
        })
    })
}

/// One finished run.
#[derive(Debug, Clone)]
pub struct Finished {
    pub title: String,
    pub exit_code: Option<i32>,
    pub stderr: String,
}

/// Runs `argv` in `dir` on a background thread and hands the outcome back to
/// `view`.
///
/// git's askpass bridge is not attached: it belongs to a session operation and
/// its prompt is keyed by operation id, so a user command that needs a
/// credential fails with git's own "terminal prompts disabled" error instead of
/// hanging behind an invisible prompt.
pub fn execute(
    dir: PathBuf,
    argv: Vec<String>,
    title: String,
    log: Arc<CommandLog>,
    view: WeakEntity<crate::repo_view::RepoView>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let program = argv[0].clone();
        // The command log has no program field and prints every entry as
        // `git <args>`, so only git runs belong there; anything else would
        // read as a git command it never was.
        let is_git = std::path::Path::new(&program)
            .file_stem()
            .is_some_and(|stem| stem == "git");
        let args = argv[1..].to_vec();
        let run_dir = dir.clone();
        let run_title = title.clone();
        let result = cx
            .background_executor()
            .spawn(async move { run(&run_dir, &program, &args, &run_title) })
            .await;
        let (finished, record) = result;
        if is_git {
            log.record(record);
        }
        let _ = view.update(cx, |this, cx| {
            this.custom_command_finished(finished, cx);
        });
    })
    .detach();
}

fn run(
    dir: &std::path::Path,
    program: &str,
    args: &[String],
    title: &str,
) -> (Finished, CommandRecord) {
    let started_at = SystemTime::now();
    let output = std::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output();
    let duration = started_at.elapsed().unwrap_or_default();
    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let record = CommandRecord {
                // Without the program, as the git CLI records its runs.
                args: args.to_vec(),
                started_at,
                duration,
                exit_code: output.status.code(),
                stdout,
                stderr: stderr.clone(),
            };
            (
                Finished {
                    title: title.to_owned(),
                    exit_code: output.status.code(),
                    stderr,
                },
                record,
            )
        }
        Err(error) => {
            let stderr = error.to_string();
            let record = CommandRecord {
                // Without the program, as the git CLI records its runs.
                args: args.to_vec(),
                started_at,
                duration,
                exit_code: None,
                stdout: String::new(),
                stderr: stderr.clone(),
            };
            (
                Finished {
                    title: title.to_owned(),
                    exit_code: None,
                    stderr,
                },
                record,
            )
        }
    }
}

/// The first line of a failed command worth reporting.
pub fn failure_line(finished: &Finished) -> String {
    finished
        .stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or_else(
            || format!("exit {}", finished.exit_code.unwrap_or(-1)),
            str::to_owned,
        )
}

/// Creates `commands.json` with an empty list so the settings button has a file
/// to open on a fresh install.
pub fn ensure_file() -> Result<PathBuf, String> {
    let path =
        commands_file().ok_or_else(|| "the configuration directory is unknown".to_owned())?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        std::fs::write(&path, b"[]\n")
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(path)
}

/// How the settings page lists the loaded commands.
pub struct CommandRow<'a> {
    pub title: &'a str,
    pub context: &'a str,
    pub line: String,
    pub confirm: bool,
    pub key: Option<&'a str>,
}

/// Rows for the settings page, in file order.
pub fn rows(state: &CommandState) -> Vec<CommandRow<'_>> {
    state
        .commands
        .iter()
        .map(|command| CommandRow {
            title: &command.spec.title,
            context: command.spec.context.label(),
            line: display_line(&command.spec.command),
            confirm: command.spec.confirm,
            key: command.spec.key.as_deref(),
        })
        .collect()
}

/// A map of the resolved variables, for tests and callers building them by
/// name.
#[cfg(test)]
pub fn variables(pairs: &[(&str, &str)]) -> Variables {
    let map: std::collections::HashMap<&str, &str> = pairs.iter().copied().collect();
    Variables {
        repo: map.get("repo").map(|value| (*value).to_owned()),
        branch: map.get("branch").map(|value| (*value).to_owned()),
        commit: map.get("commit").map(|value| (*value).to_owned()),
        file: map.get("file").map(|value| (*value).to_owned()),
        remote: map.get("remote").map(|value| (*value).to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_parses_with_its_optional_fields() {
        let (commands, warnings) = parse(
            r#"[
                {"title": "Push to staging", "command": ["git", "push", "staging", "${branch}"], "context": "branch", "confirm": true, "key": "cmd-alt-s"},
                {"title": "Status", "command": ["git", "status"]}
            ]"#,
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].spec.context, Context::Branch);
        assert!(commands[0].spec.confirm);
        assert_eq!(commands[0].spec.key.as_deref(), Some("cmd-alt-s"));
        assert_eq!(commands[1].spec.context, Context::Repository);
        assert!(!commands[1].spec.confirm);
    }

    #[test]
    fn a_broken_file_names_the_line() {
        let error =
            parse("[\n {\"title\": \"x\", \"command\": []},\n {\"title\": }\n]").unwrap_err();
        assert!(error.starts_with("line 3"), "{error}");
    }

    #[test]
    fn an_empty_command_is_a_warning_not_a_panic() {
        let (commands, warnings) = parse(r#"[{"title": "Empty", "command": []}]"#).unwrap();
        assert!(commands.is_empty());
        assert!(warnings[0].contains("command"));
    }

    #[test]
    fn variables_expand_inside_one_argument() {
        let vars = variables(&[
            ("repo", "/work/strata"),
            ("branch", "feat/x"),
            ("commit", "abc123"),
            ("file", "src/main.rs"),
            ("remote", "origin"),
        ]);
        let argv = vec![
            "git".to_owned(),
            "push".to_owned(),
            "${remote}".to_owned(),
            "${branch}:${branch}".to_owned(),
            "--repo=${repo}".to_owned(),
        ];
        assert_eq!(
            expand(&argv, &vars).unwrap(),
            vec![
                "git",
                "push",
                "origin",
                "feat/x:feat/x",
                "--repo=/work/strata"
            ]
        );
    }

    #[test]
    fn a_missing_variable_disables_the_command_with_a_reason() {
        let vars = variables(&[("repo", "/work/strata")]);
        let argv = vec!["git".to_owned(), "show".to_owned(), "${commit}".to_owned()];
        assert_eq!(expand(&argv, &vars).unwrap_err(), "no commit is selected");
        let unknown = vec!["echo".to_owned(), "${nope}".to_owned()];
        assert!(
            expand(&unknown, &vars)
                .unwrap_err()
                .contains("unknown variable")
        );
    }

    #[test]
    fn an_unterminated_variable_is_an_error() {
        let vars = Variables::default();
        let argv = vec!["echo".to_owned(), "${branch".to_owned()];
        assert!(expand(&argv, &vars).is_err());
    }

    #[test]
    fn arguments_with_spaces_stay_one_argument() {
        let dir = tempfile::tempdir().unwrap();
        let argv = ["printf".to_owned(), "%s".to_owned(), "a b".to_owned()];
        let (finished, record) = run(dir.path(), &argv[0], &argv[1..], "printf");
        assert_eq!(finished.exit_code, Some(0));
        assert_eq!(record.stdout, "a b");
        assert_eq!(record.args, argv[1..]);
    }

    #[test]
    fn there_is_no_shell_expansion() {
        let dir = tempfile::tempdir().unwrap();
        // A shell would turn `$HOME` into a path and split it; argv does not.
        let argv = ["printf".to_owned(), "%s".to_owned(), "$HOME *".to_owned()];
        let (finished, record) = run(dir.path(), &argv[0], &argv[1..], "printf");
        assert_eq!(finished.exit_code, Some(0));
        assert_eq!(record.stdout, "$HOME *");
    }

    #[test]
    fn a_non_zero_exit_keeps_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let (finished, record) = run(
            dir.path(),
            "sh",
            &["-c".to_owned(), "echo bad >&2; exit 3".to_owned()],
            "sh",
        );
        assert_eq!(finished.exit_code, Some(3));
        assert_eq!(record.exit_code, Some(3));
        assert!(finished.stderr.contains("bad"));
        assert_eq!(failure_line(&finished), "bad");
    }

    #[test]
    fn the_display_line_quotes_for_reading_only() {
        let argv = vec![
            "git".to_owned(),
            "commit".to_owned(),
            "-m".to_owned(),
            "two words".to_owned(),
        ];
        assert_eq!(display_line(&argv), "git commit -m \"two words\"");
    }
}
