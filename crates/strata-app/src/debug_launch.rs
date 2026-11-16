//! Debug launch flags for opening a specific view or dialog at startup.
//!
//! Automated UI checks pass these flags alongside `[PATH]`, `--fake`, or
//! `--gallery` so they can land directly on the view under test without
//! hunting for button coordinates. Parsing is pure and returns errors as
//! strings; the caller prints them to stderr and continues with a normal
//! launch when a value is invalid or unknown.

use std::path::PathBuf;

use gpui::{AnyWindowHandle, App, Window};
use strata_core::session::RepoSnapshot;
use strata_core::{HeadInfo, Oid};

use gpui_component::WindowExt as _;

use crate::actions::{OpenSettings, TogglePalette};
use crate::fake;

/// Initial window size requested via `--window-size <W>x<H>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowSize {
    pub width: u32,
    pub height: u32,
}

/// Dialogs and secondary windows that can be opened with `--dialog <name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DebugDialog {
    CreateBranch,
    Stash,
    Settings,
    Palette,
    OpenRepository,
    AddWorktree,
    CreateTag,
}

impl DebugDialog {
    pub(crate) const ALL_NAMES: &'static [&'static str] = &[
        "create-branch",
        "stash",
        "settings",
        "palette",
        "open-repository",
        "add-worktree",
        "create-tag",
    ];

    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "create-branch" => Some(Self::CreateBranch),
            "stash" => Some(Self::Stash),
            "settings" => Some(Self::Settings),
            "palette" => Some(Self::Palette),
            "open-repository" => Some(Self::OpenRepository),
            "add-worktree" => Some(Self::AddWorktree),
            "create-tag" => Some(Self::CreateTag),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::CreateBranch => "create-branch",
            Self::Stash => "stash",
            Self::Settings => "settings",
            Self::Palette => "palette",
            Self::OpenRepository => "open-repository",
            Self::AddWorktree => "add-worktree",
            Self::CreateTag => "create-tag",
        }
    }

    pub(crate) fn needs_repo(self) -> bool {
        match self {
            Self::CreateBranch | Self::Stash | Self::AddWorktree | Self::CreateTag => true,
            Self::Settings | Self::Palette | Self::OpenRepository => false,
        }
    }

    /// True for the dialogs the app opens itself, on whichever window is in
    /// front, rather than a dialog a repository view owns.
    pub(crate) fn is_window_level(self) -> bool {
        match self {
            Self::Settings | Self::Palette => true,
            Self::CreateBranch
            | Self::Stash
            | Self::OpenRepository
            | Self::AddWorktree
            | Self::CreateTag => false,
        }
    }
}

/// Target for `--tree <rev>[:<dir>]` or `--tree-file <rev>:<path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TreeTarget {
    Directory { rev: String, dir: Option<String> },
    File { rev: String, path: String },
}

impl TreeTarget {
    pub(crate) fn rev(&self) -> &str {
        match self {
            Self::Directory { rev, .. } | Self::File { rev, .. } => rev,
        }
    }
}

/// Target for `--blame <rev>:<path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlameTarget {
    pub rev: String,
    pub path: String,
}

/// Debug launch options applied once the window and first snapshot are ready.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DebugLaunch {
    /// One revision, or several separated by commas for a multi-selection.
    pub select: Option<String>,
    pub file_history: Option<String>,
    pub tree: Option<TreeTarget>,
    pub blame: Option<BlameTarget>,
    pub worktree: bool,
    pub dialog: Option<DebugDialog>,
    pub window_size: Option<WindowSize>,
}

impl DebugLaunch {
    pub(crate) fn has_repo_actions(&self) -> bool {
        self.select.is_some()
            || self.file_history.is_some()
            || self.tree.is_some()
            || self.blame.is_some()
            || self.worktree
            || self.dialog.is_some_and(DebugDialog::needs_repo)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.has_repo_actions() || self.dialog.is_some() || self.window_size.is_some()
    }
}

/// What the command line asked the first window to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartMode {
    /// `strata [PATH]`: a repository, or the error screen for a bad path.
    Path(PathBuf),
    /// `strata --compare A B [C]`: compare two or three files or folders.
    Compare(Vec<PathBuf>),
    /// `strata --fake [N]`: the synthetic render benchmark.
    Fake(usize),
    /// `strata --gallery`: every shared UI part, with no repository.
    Gallery,
    /// No argument: the last repository when restoring is on, else the welcome
    /// screen.
    Welcome,
}

/// Pure result of parsing command-line arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedArgs {
    pub help: bool,
    pub start: StartMode,
    pub debug: DebugLaunch,
    pub errors: Vec<String>,
}

/// Parses command-line arguments from `std::env::args().skip(1)`.
pub(crate) fn parse_args() -> ParsedArgs {
    parse_args_from(std::env::args().skip(1))
}

/// Pure CLI argument parser.
///
/// Unknown flags and invalid option values append a message to `errors` and
/// leave the affected field unset so the app continues with a normal launch.
pub(crate) fn parse_args_from<I, S>(args: I) -> ParsedArgs
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let raw: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();
    let mut index = 0;

    let mut help = false;
    let mut start: Option<StartMode> = None;
    let mut debug = DebugLaunch::default();
    let mut errors = Vec::new();
    let mut after_double_dash = false;

    while index < raw.len() {
        let arg = &raw[index];
        index += 1;

        if after_double_dash {
            record_positional(arg, &mut start, &mut errors);
            continue;
        }

        if arg == "--" {
            after_double_dash = true;
            continue;
        }

        if arg == "-h" || arg == "--help" {
            help = true;
            continue;
        }

        if arg == "--gallery" {
            match &start {
                None => start = Some(StartMode::Gallery),
                Some(StartMode::Gallery) => {}
                Some(_) => {
                    errors.push("--gallery conflicts with an earlier target (ignored)".into())
                }
            }
            continue;
        }

        if arg == "--compare" {
            let mut paths = Vec::with_capacity(3);
            while paths.len() < 3 {
                let Some(next) = raw.get(index) else {
                    break;
                };
                if !after_double_dash && next == "--" {
                    after_double_dash = true;
                    index += 1;
                    continue;
                }
                if !after_double_dash && next.starts_with('-') && next != "-" {
                    break;
                }
                paths.push(PathBuf::from(next));
                index += 1;
            }
            if paths.len() < 2 {
                errors.push("--compare requires 2 or 3 paths (ignored)".into());
            } else {
                match &start {
                    None | Some(StartMode::Compare(_)) => start = Some(StartMode::Compare(paths)),
                    Some(_) => {
                        errors.push("--compare conflicts with an earlier target (ignored)".into())
                    }
                }
            }
            continue;
        }

        if arg == "--fake" {
            let count = match raw.get(index).and_then(|next| next.parse::<usize>().ok()) {
                Some(count) => {
                    index += 1;
                    count
                }
                None => fake::COMMIT_COUNT,
            };
            match &start {
                None => start = Some(StartMode::Fake(count)),
                Some(StartMode::Fake(_)) => start = Some(StartMode::Fake(count)),
                Some(_) => errors.push("--fake conflicts with an earlier target (ignored)".into()),
            }
            continue;
        }

        if let Some(val) = arg.strip_prefix("--fake=") {
            match val.parse::<usize>() {
                Ok(count) => match &start {
                    None | Some(StartMode::Fake(_)) => start = Some(StartMode::Fake(count)),
                    Some(_) => {
                        errors.push("--fake conflicts with an earlier target (ignored)".into())
                    }
                },
                Err(_) => errors.push(format!("invalid --fake count '{val}' (ignored)")),
            }
            continue;
        }

        if arg == "--worktree" {
            debug.worktree = true;
            continue;
        }

        if let Some((flag, inline_val)) = split_flag_value(arg) {
            match flag {
                "--select" => {
                    if let Some(val) = take_value(flag, inline_val, &raw, &mut index, &mut errors) {
                        let revs = select_revs(&val);
                        if revs.is_empty() {
                            errors.push("invalid --select '': revision cannot be empty".into());
                        } else {
                            debug.select = Some(revs.join(","));
                        }
                    }
                }
                "--file-history" => {
                    if let Some(val) = take_value(flag, inline_val, &raw, &mut index, &mut errors) {
                        if val.is_empty() {
                            errors.push("invalid --file-history '': path cannot be empty".into());
                        } else {
                            debug.file_history = Some(val);
                        }
                    }
                }
                "--tree" => {
                    if let Some(val) = take_value(flag, inline_val, &raw, &mut index, &mut errors) {
                        match parse_tree_arg(&val) {
                            Ok(target) => debug.tree = Some(target),
                            Err(msg) => errors.push(msg),
                        }
                    }
                }
                "--tree-file" => {
                    if let Some(val) = take_value(flag, inline_val, &raw, &mut index, &mut errors) {
                        match parse_tree_file_arg(&val) {
                            Ok(target) => debug.tree = Some(target),
                            Err(msg) => errors.push(msg),
                        }
                    }
                }
                "--blame" => {
                    if let Some(val) = take_value(flag, inline_val, &raw, &mut index, &mut errors) {
                        match parse_rev_path("--blame", &val) {
                            Ok((rev, path)) => debug.blame = Some(BlameTarget { rev, path }),
                            Err(msg) => errors.push(msg),
                        }
                    }
                }
                "--dialog" => {
                    if let Some(val) = take_value(flag, inline_val, &raw, &mut index, &mut errors) {
                        match DebugDialog::parse(&val) {
                            Some(dialog) => debug.dialog = Some(dialog),
                            None => errors.push(format!(
                                "unknown --dialog '{val}'; supported dialogs: {}",
                                DebugDialog::ALL_NAMES.join(", ")
                            )),
                        }
                    }
                }
                "--window-size" => {
                    if let Some(val) = take_value(flag, inline_val, &raw, &mut index, &mut errors) {
                        match parse_window_size(&val) {
                            Ok(size) => debug.window_size = Some(size),
                            Err(msg) => errors.push(msg),
                        }
                    }
                }
                _ => {
                    errors.push(format!("unknown option '{arg}' (ignored)"));
                }
            }
            continue;
        }

        record_positional(arg, &mut start, &mut errors);
    }

    ParsedArgs {
        help,
        start: start.unwrap_or(StartMode::Welcome),
        debug,
        errors,
    }
}

fn record_positional(arg: &str, start: &mut Option<StartMode>, errors: &mut Vec<String>) {
    match start {
        None => *start = Some(StartMode::Path(PathBuf::from(arg))),
        Some(_) => errors.push(format!("unexpected extra argument '{arg}' (ignored)")),
    }
}

/// The revisions `--select` names: one, or several separated by commas.
pub(crate) fn select_revs(select: &str) -> Vec<&str> {
    select
        .split(',')
        .map(str::trim)
        .filter(|rev| !rev.is_empty())
        .collect()
}

fn split_flag_value(arg: &str) -> Option<(&str, Option<&str>)> {
    if !arg.starts_with('-') || arg == "-" {
        return None;
    }
    match arg.split_once('=') {
        Some((flag, value)) => Some((flag, Some(value))),
        None => Some((arg, None)),
    }
}

fn take_value(
    flag: &str,
    inline_val: Option<&str>,
    raw: &[String],
    index: &mut usize,
    errors: &mut Vec<String>,
) -> Option<String> {
    if let Some(val) = inline_val {
        return Some(val.to_owned());
    }
    if *index < raw.len() {
        let val = raw[*index].clone();
        *index += 1;
        Some(val)
    } else {
        errors.push(format!("missing value for {flag} (ignored)"));
        None
    }
}

fn parse_tree_arg(value: &str) -> Result<TreeTarget, String> {
    if value.is_empty() {
        return Err("invalid --tree '': expected <rev>[:<dir>]".into());
    }
    match value.split_once(':') {
        Some((rev, dir)) => {
            if rev.is_empty() {
                return Err(format!(
                    "invalid --tree '{value}': missing revision before ':'"
                ));
            }
            let dir = dir.trim_matches('/');
            Ok(TreeTarget::Directory {
                rev: rev.to_owned(),
                dir: (!dir.is_empty()).then(|| dir.to_owned()),
            })
        }
        None => Ok(TreeTarget::Directory {
            rev: value.to_owned(),
            dir: None,
        }),
    }
}

fn parse_tree_file_arg(value: &str) -> Result<TreeTarget, String> {
    let (rev, path) = parse_rev_path("--tree-file", value)?;
    Ok(TreeTarget::File { rev, path })
}

/// Splits `<rev>:<path>` for the flags that name one file in a revision.
fn parse_rev_path(flag: &str, value: &str) -> Result<(String, String), String> {
    let Some((rev, path)) = value.split_once(':') else {
        return Err(format!("invalid {flag} '{value}': expected <rev>:<path>"));
    };
    // A leading slash is how a path looks from the repository root, which is
    // the only base these targets accept.
    let path = path.trim_start_matches('/');
    if rev.is_empty() || path.is_empty() {
        return Err(format!("invalid {flag} '{value}': expected <rev>:<path>"));
    }
    Ok((rev.to_owned(), path.to_owned()))
}

fn parse_window_size(value: &str) -> Result<WindowSize, String> {
    let parts = value.split_once('x').or_else(|| value.split_once('X'));
    let Some((w_str, h_str)) = parts else {
        return Err(format!("invalid --window-size '{value}': expected <W>x<H>"));
    };
    let Ok(width) = w_str.parse::<u32>() else {
        return Err(format!(
            "invalid --window-size '{value}': expected positive integers"
        ));
    };
    let Ok(height) = h_str.parse::<u32>() else {
        return Err(format!(
            "invalid --window-size '{value}': expected positive integers"
        ));
    };
    if width == 0 || height == 0 {
        return Err(format!(
            "invalid --window-size '{value}': width and height must be positive"
        ));
    }
    Ok(WindowSize { width, height })
}

pub(crate) fn help_text() -> String {
    format!(
        "usage: strata [PATH] [OPTIONS]\n\
         \x20      strata --compare <A> <B> [<C>] [OPTIONS]\n\
         \x20      strata --fake [N] [OPTIONS]\n\
         \x20      strata --gallery [OPTIONS]\n\n\
         Without PATH, Strata reopens the last repository when restoring is enabled\n\
         and shows the welcome screen otherwise.\n\
         --compare opens a standalone two-way or three-way comparison of files or folders.\n\
         With three paths, <B> is the base and <A> and <C> are compared against it.\n\
         --fake renders N synthetic commits (default {}) for the render benchmark.\n\
         --gallery shows every shared UI part, and needs no repository.\n\n\
         Debug launch options (applied once the first snapshot loads):\n\
         \x20 --select <rev>         Select and scroll to a commit (id or ref)\n\
         \x20                        Comma-separate revisions to select several\n\
         \x20 --file-history <path>  Open file history for <path>\n\
         \x20 --tree <rev>[:<dir>]   Open the tree browser at <rev>, or in <dir>\n\
         \x20 --tree-file <rev>:<path>\n\
         \x20                        Open <path> in the tree browser at <rev>\n\
         \x20 --blame <rev>:<path>   Open blame for <path> at <rev>\n\
         \x20 --worktree             Start in the working-tree view\n\
         \x20 --dialog <name>        Open a dialog ({})\n\
         \x20 --window-size <W>x<H>  Request an initial window size",
        fake::COMMIT_COUNT,
        DebugDialog::ALL_NAMES.join(", ")
    )
}

/// Outcome of matching a revision string against the loaded `RepoSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotRevMatch {
    /// Resolved to an object id.
    Found(Oid),
    /// A short hex prefix matched multiple commits in the loaded snapshot.
    Ambiguous(usize),
    /// Not found in the snapshot (caller may still try a full rev-parse via core).
    NotFound,
}

/// Resolves `HEAD`, ref names, `stash@{N}`, full object ids, or unique short
/// hex prefixes from the already-loaded `RepoSnapshot`.
pub(crate) fn resolve_in_snapshot(snapshot: &RepoSnapshot, rev: &str) -> SnapshotRevMatch {
    if rev == "HEAD" {
        if let Some(head) = &snapshot.head {
            match head {
                HeadInfo::Branch {
                    target: Some(oid), ..
                }
                | HeadInfo::Detached { target: oid } => return SnapshotRevMatch::Found(*oid),
                HeadInfo::Branch { target: None, .. } => return SnapshotRevMatch::NotFound,
            }
        }
        return SnapshotRevMatch::NotFound;
    }

    if let Some(reference) = snapshot
        .refs
        .iter()
        .find(|reference| reference.full_name == rev)
        .or_else(|| {
            snapshot
                .refs
                .iter()
                .find(|reference| reference.short_name == rev)
        })
    {
        return SnapshotRevMatch::Found(reference.target);
    }

    if let Some(index) = rev
        .strip_prefix("stash@{")
        .and_then(|rest| rest.strip_suffix('}'))
        .and_then(|num| num.parse::<usize>().ok())
        && let Some(stash) = snapshot.stashes.iter().find(|stash| stash.index == index)
    {
        return SnapshotRevMatch::Found(stash.id);
    }

    if let Ok(oid) = rev.parse::<Oid>() {
        return SnapshotRevMatch::Found(oid);
    }

    // Short hex prefix against loaded commits (4+ hex chars, matching search).
    if rev.len() >= 4 && rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let prefix = rev.to_ascii_lowercase();
        let mut matched: Option<Oid> = None;
        let mut count = 0usize;
        for index in 0..snapshot.commits.len() {
            if let Some((commit, _)) = snapshot.commits.get(index)
                && commit.id.to_string().starts_with(&prefix)
            {
                matched = Some(commit.id);
                count += 1;
            }
        }
        if count > 1 {
            return SnapshotRevMatch::Ambiguous(count);
        }
        if let Some(oid) = matched
            && snapshot.commits.complete
        {
            return SnapshotRevMatch::Found(oid);
        }
    }

    SnapshotRevMatch::NotFound
}

/// Dispatches a dialog that does not require a `RepoView` method.
pub(crate) fn dispatch_window_dialog(dialog: DebugDialog, window: &mut Window, cx: &mut App) {
    match dialog {
        DebugDialog::Settings => {
            window.dispatch_action(Box::new(OpenSettings), cx);
            eprintln!("debug-launch: applied --dialog settings");
        }
        DebugDialog::Palette => {
            // `TogglePalette` opens on `cx.active_window()` inside `cx.defer`,
            // so activate the window first and dispatch once the first frame's
            // key context / available actions have been built.
            window.activate_window();
            window.on_next_frame(move |window, cx| {
                window.activate_window();
                window.dispatch_action(Box::new(TogglePalette), cx);
                // The action needs a window with the keyboard: report a launch
                // that could not give it one instead of claiming success. A
                // server without a window manager never grants focus, and the
                // palette then has nowhere to open.
                window.on_next_frame(|window, cx| {
                    if window.has_active_dialog(cx) {
                        eprintln!("debug-launch: applied --dialog palette");
                    } else {
                        eprintln!(
                            "debug-launch: error: --dialog palette could not open: no window has keyboard focus"
                        );
                    }
                });
            });
            window.refresh();
        }
        DebugDialog::OpenRepository => {
            window.dispatch_action(Box::new(crate::actions::OpenRepository), cx);
            eprintln!("debug-launch: applied --dialog open-repository");
        }
        DebugDialog::CreateBranch
        | DebugDialog::Stash
        | DebugDialog::AddWorktree
        | DebugDialog::CreateTag => {}
    }
}

/// Opens a window-level dialog once `handle` is a window the app can update.
///
/// A `window.defer` runs while the window is still being built: GPUI records
/// its handle only after the first frame, so an `AnyWindowHandle::update` from
/// there fails. Deferring on the `App` still lands on that frame when the call
/// comes from the window's own build, so a failed update is retried once on the
/// next effect cycle; only a failure that survives the retry is reported.
pub(crate) fn open_window_dialog(handle: AnyWindowHandle, dialog: DebugDialog, cx: &mut App) {
    cx.defer(move |cx| {
        if handle
            .update(cx, |_, window, cx| {
                dispatch_window_dialog(dialog, window, cx);
            })
            .is_ok()
        {
            return;
        }
        cx.defer(move |cx| {
            let opened = handle.update(cx, |_, window, cx| {
                dispatch_window_dialog(dialog, window, cx);
            });
            if let Err(error) = opened {
                eprintln!(
                    "debug-launch: error: --dialog {} could not open: {error}",
                    dialog.name()
                );
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ParsedArgs {
        parse_args_from(args.iter().copied())
    }

    #[test]
    fn empty_args_launch_welcome() {
        let parsed = parse(&[]);
        assert!(!parsed.help);
        assert_eq!(parsed.start, StartMode::Welcome);
        assert_eq!(parsed.debug, DebugLaunch::default());
        assert!(!parsed.debug.is_active());
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn positional_path_and_existing_modes_parse() {
        let path = parse(&["/tmp/repo"]);
        assert_eq!(path.start, StartMode::Path(PathBuf::from("/tmp/repo")));
        assert!(path.errors.is_empty());

        let fake_default = parse(&["--fake"]);
        assert_eq!(fake_default.start, StartMode::Fake(fake::COMMIT_COUNT));
        assert!(fake_default.errors.is_empty());

        let fake_count = parse(&["--fake", "5000"]);
        assert_eq!(fake_count.start, StartMode::Fake(5000));
        assert!(fake_count.errors.is_empty());

        let fake_followed_by_flag = parse(&["--fake", "--worktree"]);
        assert_eq!(
            fake_followed_by_flag.start,
            StartMode::Fake(fake::COMMIT_COUNT)
        );
        assert!(fake_followed_by_flag.debug.worktree);
        assert!(fake_followed_by_flag.errors.is_empty());

        let gallery = parse(&["--gallery"]);
        assert_eq!(gallery.start, StartMode::Gallery);
        assert!(gallery.errors.is_empty());
    }

    #[test]
    fn compare_flag_parses_two_or_three_paths_and_reports_errors() {
        let two = parse(&["--compare", "left.txt", "right.txt"]);
        assert_eq!(
            two.start,
            StartMode::Compare(vec![PathBuf::from("left.txt"), PathBuf::from("right.txt")])
        );
        assert!(two.errors.is_empty());

        let three = parse(&[
            "--compare",
            "left.txt",
            "base.txt",
            "right.txt",
            "--window-size",
            "1200x800",
        ]);
        assert_eq!(
            three.start,
            StartMode::Compare(vec![
                PathBuf::from("left.txt"),
                PathBuf::from("base.txt"),
                PathBuf::from("right.txt"),
            ])
        );
        assert_eq!(
            three.debug.window_size,
            Some(WindowSize {
                width: 1200,
                height: 800,
            })
        );
        assert!(three.errors.is_empty());

        let two_before_flag = parse(&["--compare", "a", "b", "--window-size=900x700"]);
        assert_eq!(
            two_before_flag.start,
            StartMode::Compare(vec![PathBuf::from("a"), PathBuf::from("b")])
        );
        assert_eq!(
            two_before_flag.debug.window_size,
            Some(WindowSize {
                width: 900,
                height: 700,
            })
        );
        assert!(two_before_flag.errors.is_empty());

        let dash_paths = parse(&["--compare", "--", "-a", "-b"]);
        assert_eq!(
            dash_paths.start,
            StartMode::Compare(vec![PathBuf::from("-a"), PathBuf::from("-b")])
        );
        assert!(dash_paths.errors.is_empty());

        let too_few = parse(&["--compare", "only_one.txt"]);
        assert_eq!(too_few.start, StartMode::Welcome);
        assert_eq!(too_few.errors.len(), 1);
        assert!(too_few.errors[0].contains("2 or 3"));

        let missing = parse(&["--compare"]);
        assert_eq!(missing.start, StartMode::Welcome);
        assert_eq!(missing.errors.len(), 1);

        let too_many = parse(&["--compare", "a", "b", "c", "d"]);
        assert_eq!(
            too_many.start,
            StartMode::Compare(vec![
                PathBuf::from("a"),
                PathBuf::from("b"),
                PathBuf::from("c"),
            ])
        );
        assert_eq!(too_many.errors.len(), 1);
        assert!(too_many.errors[0].contains("unexpected extra argument 'd'"));

        let conflict = parse(&["--gallery", "--compare", "a", "b"]);
        assert_eq!(conflict.start, StartMode::Gallery);
        assert_eq!(conflict.errors.len(), 1);
        assert!(conflict.errors[0].contains("--compare conflicts"));
    }

    #[test]
    fn help_text_explains_the_three_way_base() {
        let help = help_text();
        assert!(help.contains("strata --compare <A> <B> [<C>]"));
        assert!(
            help.contains("<B> is the base"),
            "the three-way usage has to name the middle path as the base:\n{help}"
        );
    }

    #[test]
    fn all_debug_flags_parse_with_space_or_equals() {
        let parsed = parse(&[
            ".",
            "--select",
            "HEAD~1",
            "--file-history=crates/strata-app/src/main.rs",
            "--tree",
            "main:crates/strata-app",
            "--blame",
            "HEAD:crates/strata-app/src/main.rs",
            "--worktree",
            "--dialog",
            "create-branch",
            "--window-size",
            "1280x720",
        ]);
        assert!(parsed.errors.is_empty());
        assert_eq!(parsed.start, StartMode::Path(PathBuf::from(".")));
        assert_eq!(parsed.debug.select.as_deref(), Some("HEAD~1"));
        assert_eq!(
            parsed.debug.file_history.as_deref(),
            Some("crates/strata-app/src/main.rs")
        );
        assert_eq!(
            parsed.debug.tree,
            Some(TreeTarget::Directory {
                rev: "main".into(),
                dir: Some("crates/strata-app".into()),
            })
        );
        assert_eq!(
            parsed.debug.blame,
            Some(BlameTarget {
                rev: "HEAD".into(),
                path: "crates/strata-app/src/main.rs".into(),
            })
        );
        assert!(parsed.debug.worktree);
        assert_eq!(parsed.debug.dialog, Some(DebugDialog::CreateBranch));
        assert_eq!(
            parsed.debug.window_size,
            Some(WindowSize {
                width: 1280,
                height: 720,
            })
        );
        assert!(parsed.debug.is_active());
    }

    #[test]
    fn select_accepts_several_comma_separated_revisions() {
        let parsed = parse(&[".", "--select", "HEAD, HEAD~2 ,main"]);
        assert!(parsed.errors.is_empty());
        assert_eq!(
            parsed.debug.select.as_deref(),
            Some("HEAD,HEAD~2,main"),
            "the parsed value is normalised for the runner"
        );
        assert_eq!(select_revs("HEAD,HEAD~2,main"), ["HEAD", "HEAD~2", "main"]);

        let empty = parse(&["--select", " , "]);
        assert_eq!(empty.debug.select, None);
        assert_eq!(empty.errors.len(), 1);
        assert!(empty.errors[0].starts_with("invalid --select"));
    }

    #[test]
    fn tree_and_tree_file_variants_parse() {
        let root_only = parse(&["--tree", "abc1234"]);
        assert_eq!(
            root_only.debug.tree,
            Some(TreeTarget::Directory {
                rev: "abc1234".into(),
                dir: None,
            })
        );
        assert!(root_only.errors.is_empty());

        let trailing_colon = parse(&["--tree", "HEAD:"]);
        assert_eq!(
            trailing_colon.debug.tree,
            Some(TreeTarget::Directory {
                rev: "HEAD".into(),
                dir: None,
            })
        );
        assert!(trailing_colon.errors.is_empty());

        let file = parse(&["--tree-file", "main:src/lib.rs"]);
        assert_eq!(
            file.debug.tree,
            Some(TreeTarget::File {
                rev: "main".into(),
                path: "src/lib.rs".into(),
            })
        );
        assert!(file.errors.is_empty());

        let rooted_file = parse(&["--tree-file", "HEAD:/crates/strata-app/Cargo.toml"]);
        assert_eq!(
            rooted_file.debug.tree,
            Some(TreeTarget::File {
                rev: "HEAD".into(),
                path: "crates/strata-app/Cargo.toml".into(),
            })
        );
        assert!(rooted_file.errors.is_empty());
    }

    #[test]
    fn blame_target_parses_rev_and_path() {
        let parsed = parse(&["--blame", "HEAD~2:src/repo_view.rs"]);
        assert!(parsed.errors.is_empty());
        assert_eq!(
            parsed.debug.blame,
            Some(BlameTarget {
                rev: "HEAD~2".into(),
                path: "src/repo_view.rs".into(),
            })
        );
        assert!(parsed.debug.has_repo_actions());

        let rooted = parse(&["--blame=deadbeef:/src/main.rs"]);
        assert!(rooted.errors.is_empty());
        assert_eq!(
            rooted.debug.blame,
            Some(BlameTarget {
                rev: "deadbeef".into(),
                path: "src/main.rs".into(),
            })
        );
    }

    #[test]
    fn blame_and_tree_file_reject_incomplete_targets() {
        for value in ["main", "main:", ":src/lib.rs"] {
            let parsed = parse(&["--blame", value]);
            assert_eq!(parsed.debug.blame, None, "{value}");
            assert_eq!(parsed.errors.len(), 1, "{value}");
            assert!(
                parsed.errors[0].contains("expected <rev>:<path>"),
                "{value}: {}",
                parsed.errors[0]
            );
        }

        let tree_file = parse(&["--tree-file", "main:"]);
        assert_eq!(tree_file.debug.tree, None);
        assert_eq!(tree_file.errors.len(), 1);
        assert!(tree_file.errors[0].starts_with("invalid --tree-file"));
    }

    #[test]
    fn dialog_names_parse_and_unknown_dialog_errors() {
        for name in DebugDialog::ALL_NAMES {
            let parsed = parse(&["--dialog", name]);
            assert!(parsed.errors.is_empty(), "failed on dialog {name}");
            assert_eq!(parsed.debug.dialog.map(DebugDialog::name), Some(*name));
        }

        let bad = parse(&["--dialog", "nonexistent", "."]);
        assert_eq!(bad.debug.dialog, None);
        assert_eq!(bad.start, StartMode::Path(PathBuf::from(".")));
        assert_eq!(bad.errors.len(), 1);
        assert!(bad.errors[0].contains("nonexistent"));
    }

    #[test]
    fn only_the_settings_window_and_the_palette_are_window_level() {
        // The rest open on a repository view, which has the state they need.
        for name in DebugDialog::ALL_NAMES {
            let dialog = DebugDialog::parse(name).expect(name);
            assert_eq!(
                dialog.is_window_level(),
                matches!(dialog, DebugDialog::Settings | DebugDialog::Palette),
                "{name}"
            );
        }
    }

    #[test]
    fn invalid_values_and_unknown_flags_report_errors_and_continue() {
        let parsed = parse(&[
            "--unknown-flag",
            "--window-size",
            "0x800",
            "--tree-file",
            "missing-colon",
            "--tree",
            ":dir-without-rev",
            "--select",
        ]);
        assert_eq!(parsed.start, StartMode::Welcome);
        assert_eq!(parsed.debug, DebugLaunch::default());
        assert_eq!(parsed.errors.len(), 5);

        let bad_size = parse(&["--window-size", "wide x tall", "."]);
        assert_eq!(bad_size.debug.window_size, None);
        assert_eq!(bad_size.start, StartMode::Path(PathBuf::from(".")));
        assert_eq!(bad_size.errors.len(), 1);

        let uppercase_x = parse(&["--window-size", "1024X768"]);
        assert_eq!(
            uppercase_x.debug.window_size,
            Some(WindowSize {
                width: 1024,
                height: 768,
            })
        );
        assert!(uppercase_x.errors.is_empty());
    }

    #[test]
    fn flags_before_path_work_and_double_dash_allows_dash_paths() {
        let parsed = parse(&["--worktree", "--", "-weird-repo-dir"]);
        assert!(parsed.errors.is_empty());
        assert!(parsed.debug.worktree);
        assert_eq!(
            parsed.start,
            StartMode::Path(PathBuf::from("-weird-repo-dir"))
        );
    }

    #[test]
    fn help_flag_sets_help() {
        let short = parse(&["-h"]);
        assert!(short.help);
        let long = parse(&["--help"]);
        assert!(long.help);
    }

    #[test]
    fn snapshot_rev_resolution_matches_head_refs_stashes_and_oids() {
        use std::collections::HashMap;
        use std::sync::Arc;
        use strata_core::graph::GraphRows;
        use strata_core::session::CommitList;
        use strata_core::{CommitInfo, RefInfo, RefKind, RepoState, StashInfo, WorktreeStatus};

        let oid1: Oid = "1111111111111111111111111111111111111111"
            .parse()
            .expect("oid1");
        let oid2: Oid = "1111222222222222222222222222222222222222"
            .parse()
            .expect("oid2");

        let mut rows = GraphRows::default();
        rows.push_row(0, 0, 1, &[], &[]);
        rows.push_row(0, 0, 1, &[], &[]);
        let mut commits = CommitList::default();
        commits.extend(
            vec![
                CommitInfo {
                    id: oid1,
                    parents: vec![oid2],
                    summary: "first".into(),
                    author_name: "a".into(),
                    author_email: "a@example.com".into(),
                    author_time: 10,
                    committer_time: 10,
                    shallow_boundary: false,
                },
                CommitInfo {
                    id: oid2,
                    parents: vec![],
                    summary: "second".into(),
                    author_name: "a".into(),
                    author_email: "a@example.com".into(),
                    author_time: 5,
                    committer_time: 5,
                    shallow_boundary: false,
                },
            ],
            rows,
        );
        commits.complete = true;

        let snapshot = RepoSnapshot {
            generation: 1,
            head: Some(HeadInfo::Branch {
                short_name: "main".into(),
                target: Some(oid1),
            }),
            refs: Arc::new(vec![RefInfo {
                full_name: "refs/heads/main".into(),
                short_name: "main".into(),
                kind: RefKind::LocalBranch,
                target: oid1,
            }]),
            refs_by_commit: Arc::new(HashMap::new()),
            commits,
            commit_index: Arc::new(HashMap::from([(oid1, 0), (oid2, 1)])),
            status: Arc::new(WorktreeStatus::default()),
            stashes: Arc::new(vec![StashInfo {
                index: 0,
                id: oid2,
                message: "WIP".into(),
                created_time: 12,
                branch: Some("main".into()),
            }]),
            upstreams: Arc::new(HashMap::new()),
            remotes: Vec::new(),
            state: RepoState::Normal,
            conflicts: Arc::new(Vec::new()),
            busy: None,
        };

        assert_eq!(
            resolve_in_snapshot(&snapshot, "HEAD"),
            SnapshotRevMatch::Found(oid1)
        );
        assert_eq!(
            resolve_in_snapshot(&snapshot, "main"),
            SnapshotRevMatch::Found(oid1)
        );
        assert_eq!(
            resolve_in_snapshot(&snapshot, "refs/heads/main"),
            SnapshotRevMatch::Found(oid1)
        );
        assert_eq!(
            resolve_in_snapshot(&snapshot, "stash@{0}"),
            SnapshotRevMatch::Found(oid2)
        );
        assert_eq!(
            resolve_in_snapshot(&snapshot, &oid2.to_string()),
            SnapshotRevMatch::Found(oid2)
        );
        assert_eq!(
            resolve_in_snapshot(&snapshot, "111122"),
            SnapshotRevMatch::Found(oid2)
        );
        assert_eq!(
            resolve_in_snapshot(&snapshot, "1111"),
            SnapshotRevMatch::Ambiguous(2)
        );
        assert_eq!(
            resolve_in_snapshot(&snapshot, "HEAD~1"),
            SnapshotRevMatch::NotFound
        );
    }
}
