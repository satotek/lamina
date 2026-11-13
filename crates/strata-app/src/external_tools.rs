//! Opening a terminal or an editor for a path.
//!
//! The commands come from `tools.terminal` and `tools.editor` in the settings.
//! They are split into arguments and the placeholders are substituted inside
//! one argument at a time, then the program is spawned directly: no shell ever
//! parses the command, so a path with spaces, quotes or shell metacharacters
//! reaches the program exactly as written.
//!
//! A configured command that cannot start is reported to the caller; the
//! spawn never blocks the UI thread, and the child is reaped on a small
//! background thread so it cannot linger as a zombie.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::settings::Settings;

/// Opens a terminal in `dir`.
///
/// `settings.tools.terminal` is used when set; otherwise the platform's
/// terminal opens in that directory.
pub fn open_terminal(dir: &Path, settings: &Settings) -> Result<(), String> {
    match &settings.tools.terminal {
        Some(template) => {
            let mut command = configured_command(template, dir, None)?;
            command.current_dir(dir);
            spawn_detached(command, "terminal")
        }
        None => default_terminal(dir),
    }
}

/// Opens `path` in the editor.
///
/// `line` is substituted into `{line}` when the configured command has it.
/// Without a configured editor the OS opener receives the file, which cannot
/// take a line number.
pub fn open_in_editor(path: &Path, line: Option<u32>, settings: &Settings) -> Result<(), String> {
    match &settings.tools.editor {
        Some(template) => {
            let command = configured_command(template, path, line)?;
            spawn_detached(command, "editor")
        }
        None => default_editor(path),
    }
}

/// Splits a command into arguments.
///
/// Whitespace separates arguments; single and double quotes group text that
/// contains it, and adjacent quoted and unquoted pieces form one argument.
/// Backslashes are literal, so a Windows path in the settings survives. There
/// is no variable expansion, command substitution or other shell syntax.
fn split_command(command: &str) -> Result<Vec<String>, String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;

    for character in command.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => current.push(character),
            None => match character {
                '\'' | '"' => {
                    quote = Some(character);
                    started = true;
                }
                character if character.is_whitespace() => {
                    if started {
                        arguments.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                character => {
                    current.push(character);
                    started = true;
                }
            },
        }
    }

    if quote.is_some() {
        return Err(format!("unterminated quote in command: {command}"));
    }
    if started {
        arguments.push(current);
    }
    Ok(arguments)
}

/// Replaces `{path}` and `{line}` in one argument.
///
/// The replacement is one pass, so a path that itself contains `{line}` is not
/// substituted a second time.
fn substitute(template: &str, path: &Path, line: Option<u32>) -> String {
    let path = path.to_string_lossy();
    let line = line.map(|line| line.to_string()).unwrap_or_default();
    let mut result = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(index) = rest.find('{') {
        result.push_str(&rest[..index]);
        let tail = &rest[index..];
        if let Some(after) = tail.strip_prefix("{path}") {
            result.push_str(&path);
            rest = after;
        } else if let Some(after) = tail.strip_prefix("{line}") {
            result.push_str(&line);
            rest = after;
        } else {
            // An unknown placeholder (or a lone brace) stays as written.
            result.push('{');
            rest = &tail[1..];
        }
    }
    result.push_str(rest);
    result
}

/// Builds a command from a configured template, substituting per argument.
fn configured_command(template: &str, path: &Path, line: Option<u32>) -> Result<Command, String> {
    let arguments = split_command(template)?;
    let Some((program, arguments)) = arguments.split_first() else {
        return Err("the command in settings is empty".to_owned());
    };
    let mut command = Command::new(substitute(program, path, line));
    command.args(
        arguments
            .iter()
            .map(|argument| substitute(argument, path, line)),
    );
    Ok(command)
}

/// Starts `command` without waiting for it.
fn spawn_detached(mut command: Command, what: &str) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start the {what} ({program}): {error}"))?;

    // Dropping a `Child` does not reap it on Unix; wait on a thread so the UI
    // never blocks on a program that stays open for as long as the user uses
    // it. A failed thread spawn only leaves the child unreaped until exit.
    if let Err(error) = std::thread::Builder::new()
        .name("strata-external-tool".to_owned())
        .spawn(move || {
            let _ = child.wait();
        })
    {
        log::warn!("could not track the {what} process: {error}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn default_terminal(dir: &Path) -> Result<(), String> {
    let mut command = Command::new("open");
    command.arg("-a").arg("Terminal").arg(dir);
    spawn_detached(command, "terminal")
}

#[cfg(target_os = "windows")]
fn default_terminal(dir: &Path) -> Result<(), String> {
    // Windows Terminal is not installed everywhere; `cmd` is the fallback that
    // every Windows install has.
    let mut windows_terminal = Command::new("wt");
    windows_terminal.arg("-d").arg(dir);
    match spawn_detached(windows_terminal, "terminal") {
        Ok(()) => Ok(()),
        Err(_) => {
            let mut command = Command::new("cmd");
            command
                .args(["/c", "start", "cmd", "/K", "cd", "/d"])
                .arg(dir);
            spawn_detached(command, "terminal")
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn default_terminal(dir: &Path) -> Result<(), String> {
    // `$TERMINAL` is the user's own choice and may carry arguments, so it goes
    // through the same parser as a configured command.
    if let Some(terminal) = std::env::var("TERMINAL")
        .ok()
        .filter(|terminal| !terminal.trim().is_empty())
    {
        let mut command = configured_command(&terminal, dir, None)?;
        command.current_dir(dir);
        return spawn_detached(command, "terminal");
    }

    // The first one that starts wins. Every candidate also gets `dir` as its
    // working directory, because only two of them take it as an argument.
    const CANDIDATES: &[(&str, Option<&str>)] = &[
        ("x-terminal-emulator", None),
        ("gnome-terminal", Some("--working-directory")),
        ("konsole", Some("--workdir")),
        ("xterm", None),
    ];
    let mut last_error = None;
    for (program, directory_argument) in CANDIDATES {
        let mut command = Command::new(program);
        command.current_dir(dir);
        if let Some(argument) = directory_argument {
            command.arg(argument).arg(dir);
        }
        match spawn_detached(command, "terminal") {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "no terminal program found".to_owned()))
}

#[cfg(target_os = "macos")]
fn default_editor(path: &Path) -> Result<(), String> {
    let mut command = Command::new("open");
    command.arg(path);
    spawn_detached(command, "editor")
}

#[cfg(target_os = "windows")]
fn default_editor(path: &Path) -> Result<(), String> {
    // `start` treats its first quoted argument as the window title, so the
    // empty title keeps the path from being swallowed as one.
    let mut command = Command::new("cmd");
    command.args(["/c", "start", ""]).arg(path);
    spawn_detached(command, "editor")
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn default_editor(path: &Path) -> Result<(), String> {
    let mut command = Command::new("xdg-open");
    command.arg(path);
    spawn_detached(command, "editor")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn whitespace_separates_arguments() {
        assert_eq!(
            split_command("  alacritty   --working-directory  ").unwrap(),
            ["alacritty", "--working-directory"]
        );
        assert!(split_command("").unwrap().is_empty());
        assert!(split_command("   ").unwrap().is_empty());
    }

    #[test]
    fn quotes_group_text_with_spaces() {
        assert_eq!(
            split_command(r#"code --goto "/tmp/a b/file.rs:12""#).unwrap(),
            ["code", "--goto", "/tmp/a b/file.rs:12"]
        );
        assert_eq!(
            split_command("open -a 'Visual Studio Code'").unwrap(),
            ["open", "-a", "Visual Studio Code"]
        );
    }

    #[test]
    fn adjacent_pieces_form_one_argument() {
        assert_eq!(
            split_command(r#"--arg=one" two"three"#).unwrap(),
            ["--arg=one twothree"]
        );
        assert_eq!(
            split_command(r#"--arg=one" two three""#).unwrap(),
            ["--arg=one two three"]
        );
        assert_eq!(split_command(r#""""#).unwrap(), [""]);
    }

    #[test]
    fn backslashes_are_literal_for_windows_paths() {
        assert_eq!(
            split_command(r#"code C:\Users\me\src\main.rs"#).unwrap(),
            [r"code", r"C:\Users\me\src\main.rs"]
        );
    }

    #[test]
    fn an_unterminated_quote_is_an_error() {
        assert!(split_command(r#"code "unfinished"#).is_err());
        assert!(split_command("code 'unfinished").is_err());
    }

    #[test]
    fn placeholders_are_replaced_per_argument() {
        let path = PathBuf::from("/tmp/a b/file.rs");
        assert_eq!(
            substitute("{path}:{line}", &path, Some(12)),
            "/tmp/a b/file.rs:12"
        );
        assert_eq!(
            substitute("--file={path}", &path, None),
            "--file=/tmp/a b/file.rs"
        );
        assert_eq!(substitute("--line={line}", &path, None), "--line=");
    }

    #[test]
    fn unknown_placeholders_and_braces_stay_as_written() {
        let path = PathBuf::from("/tmp/file.rs");
        assert_eq!(substitute("{unknown}", &path, Some(1)), "{unknown}");
        assert_eq!(substitute("a { b", &path, Some(1)), "a { b");
        assert_eq!(
            substitute("no placeholders", &path, Some(1)),
            "no placeholders"
        );
    }

    #[test]
    fn a_path_containing_a_placeholder_is_not_replaced_again() {
        let path = PathBuf::from("/tmp/{line}/file.rs");
        assert_eq!(substitute("{path}", &path, Some(7)), "/tmp/{line}/file.rs");
    }

    #[test]
    fn a_configured_command_keeps_each_substituted_argument_whole() {
        let path = PathBuf::from("/tmp/a b/file.rs");
        let command = configured_command("code --goto {path}:{line}", &path, Some(42)).unwrap();
        assert_eq!(command.get_program(), "code");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--goto", "/tmp/a b/file.rs:42"]
        );
    }

    #[test]
    fn an_empty_configured_command_is_reported() {
        assert!(configured_command("   ", Path::new("/tmp"), None).is_err());
    }
}
