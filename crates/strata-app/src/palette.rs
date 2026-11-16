//! The command palette: every window command in one searchable list.
//!
//! `Ctrl+Shift+P` (`Cmd+Shift+P` on macOS) or `F1` opens a modal over the
//! window. The list is built from the app's GPUI actions rather than from a
//! second command table, so a palette entry runs exactly what the button or the
//! shortcut in the view runs: the palette dispatches the action to the window
//! that opened it and closes first, which hands the keyboard back to the view
//! underneath.
//!
//! What a window can offer depends on what is on screen: an action that needs a
//! repository is a command the repository view listens for, so on the welcome
//! screen it is missing from the list instead of failing silently. The key
//! shown next to a command is read from the window's keymap, never written
//! beside the entry, so a rebinding or a platform difference is reflected.
//!
//! Commands the user runs are remembered for the rest of the session (most
//! recent first) and lead the empty list, which is what makes the palette a
//! fast repeat path.

use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, Global,
    InteractiveElement as _, IntoElement, KeyBinding, OperatingSystem, ParentElement, Render,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, Window, actions, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogEntrance;
use gpui_component::input::{Input, InputEvent, InputState};

use crate::actions::{StashChanges, TogglePalette};
use crate::commands::{CommandState, RunCustomCommand};
use crate::keymap::{self, ActionSpec};
use crate::settings::Settings;
use crate::style::Design;
use crate::ui;

/// Key context of the palette's card. Keystrokes bound with it do not reach
/// the view underneath while the palette is open.
const PALETTE: &str = "Palette";
/// The card's width; the list is as tall as its entries.
const WIDTH: f32 = 560.0;
/// How many commands the empty list remembers running.
const MAX_RECENT: usize = 5;

actions!(
    strata_palette,
    [
        /// Moves the selection up one command.
        PalettePrevious,
        /// Moves the selection down one command.
        PaletteNext,
        /// Closes the palette without running anything.
        PaletteDismiss,
    ]
);

/// Registers the palette's own keys and the action that opens it.
pub fn bind(cx: &mut App) {
    cx.set_global(RecentCommands::default());
    crate::keymap::bind_group(cx, crate::keymap::Group::Palette);
    cx.on_action(|_: &TogglePalette, cx: &mut App| {
        let Some(handle) = cx.active_window() else {
            return;
        };
        // The keystroke is dispatched from inside that window's own update, so
        // the window cannot be borrowed again here; open on the next effect
        // cycle, once the window is back in the app.
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| open(window, cx));
        });
    });
}

/// One command the palette can offer.
struct Command {
    source: Source,
    /// What the user searches and reads.
    label: String,
    /// The registry's English title when the interface language shows another
    /// one. Command names are git vocabulary people often think of in English
    /// whatever the interface language, so both find the command.
    alias: Option<String>,
    /// Stable identity across reloads of `commands.json`, for the recent list.
    identity: String,
    /// Commands about a repository are left out of the welcome screen's list.
    needs_repo: bool,
    /// The user's optional key for a custom command.
    key: Option<String>,
}

#[derive(Clone, Copy)]
enum Source {
    /// Built in: the registry owns the title and the action.
    Builtin(&'static ActionSpec),
    /// A command from `commands.json`, by index into the loaded list.
    Custom(usize),
}

impl Command {
    fn builtin(spec: &'static ActionSpec) -> Self {
        let slot = spec.palette.expect("palette commands carry a slot");
        Self {
            source: Source::Builtin(spec),
            label: keymap::display_title(spec),
            alias: Some(spec.title.to_owned())
                .filter(|title| *title != keymap::display_title(spec)),
            identity: keymap::short_name(spec.name).to_owned(),
            needs_repo: slot.needs_repo,
            key: None,
        }
    }

    fn custom(index: usize, command: &crate::commands::CustomCommand) -> Self {
        Self {
            source: Source::Custom(index),
            label: command.spec.title.clone(),
            alias: None,
            identity: format!("custom:{}", command.spec.title),
            needs_repo: true,
            key: command.spec.key.clone(),
        }
    }

    fn action(&self) -> Box<dyn Action> {
        match self.source {
            Source::Builtin(spec) => (spec.build)(),
            Source::Custom(index) => Box::new(RunCustomCommand { index }),
        }
    }

    fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

/// Every command the palette can offer: the registry's palette entries in
/// their chosen order, then the user's commands in file order.
fn commands(cx: &App) -> Vec<Command> {
    let mut builtins: Vec<&'static ActionSpec> = keymap::ACTIONS
        .iter()
        .filter(|spec| spec.palette.is_some())
        .collect();
    builtins.sort_by_key(|spec| spec.palette.map(|slot| slot.order));
    let mut commands: Vec<Command> = builtins.into_iter().map(Command::builtin).collect();
    for (index, command) in CommandState::get(cx).commands.iter().enumerate() {
        commands.push(Command::custom(index, command));
    }
    commands
}

/// Commands run from the palette this session, most recent first. It is a
/// convenience, not a preference, so it is not written to disk.
#[derive(Default)]
struct RecentCommands(Vec<String>);

impl Global for RecentCommands {}

impl RecentCommands {
    fn remember(&mut self, command: &str) {
        self.0.retain(|seen| seen != command);
        self.0.insert(0, command.to_owned());
        self.0.truncate(MAX_RECENT);
    }

    fn position(&self, command: &str) -> Option<usize> {
        self.0.iter().position(|seen| seen == command)
    }
}

/// What the palette knows about one command in one window.
struct Entry {
    /// Index into the palette's command list.
    command: usize,
    /// The focused view listens for the action, so running it does something.
    handled: bool,
    /// The key that runs it, spelled for the platform.
    binding: Option<SharedString>,
}

struct Palette {
    entity: Entity<Palette>,
    /// The text field's focus handle; the palette focuses it once the dialog
    /// exists, because opening a dialog takes the focus first.
    focus: FocusHandle,
    /// What had the keyboard when the palette opened. The dialog gives it back
    /// when it closes, and the command is dispatched on that view's own node.
    return_focus: Option<FocusHandle>,
    input: Entity<InputState>,
    commands: Vec<Command>,
    entries: Vec<Entry>,
    /// A repository view was on screen when the palette opened.
    repo_open: bool,
    /// Position in the filtered list, not an index into `entries`.
    selected: usize,
    _input_events: Subscription,
}

impl Palette {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let commands = commands(cx);
        let (entries, repo_open) = snapshot(&commands, window, cx);
        let return_focus = window.focused(cx);
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("palette.placeholder").to_string())
        });
        let focus = input.focus_handle(cx);
        let _input_events =
            cx.subscribe(&input, |this, _input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    // The list narrowed, so the old position points at something
                    // the user is not looking at anymore.
                    this.selected = 0;
                    cx.notify();
                }
                // The field owns `Enter` while it has focus, and handles it before
                // a binding on an ancestor would run. Submitting from the event
                // keeps one Enter, one command.
                InputEvent::PressEnter {
                    secondary: false, ..
                } => {
                    let Some(handle) = cx.active_window() else {
                        return;
                    };
                    let position = this.selected;
                    // The subscriber already holds `this`, so the command runs
                    // through that borrow instead of leasing the entity again.
                    let _ = handle.update(cx, |_, window, cx| this.run(position, window, cx));
                }
                _ => {}
            });
        Self {
            entity: cx.entity(),
            focus,
            return_focus,
            input,
            commands,
            entries,
            repo_open,
            selected: 0,
            _input_events,
        }
    }

    /// Indices into `entries` for the current query, best match first.
    fn filtered(&self, cx: &App) -> Vec<usize> {
        let recents = cx.global::<RecentCommands>();
        let candidates: Vec<Candidate<'_>> = self
            .entries
            .iter()
            .map(|entry| {
                let command = &self.commands[entry.command];
                Candidate {
                    label: &command.label,
                    alias: command.alias.as_deref(),
                    visible: entry.handled && (self.repo_open || !command.needs_repo),
                    recent: recents.position(&command.identity),
                }
            })
            .collect();
        let query = self.input.read(cx).value().to_string();
        rank(&candidates, &query)
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.filtered(cx).len();
        // Nothing to move to, and `Esc` or typing is still the way out.
        if len > 0 {
            self.selected = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
            cx.notify();
        }
        cx.stop_propagation();
    }

    /// Runs the command at `position` in the filtered list.
    fn run(&mut self, position: usize, window: &mut Window, cx: &mut App) {
        let Some(&entry) = self.filtered(cx).get(position) else {
            return;
        };
        let command_index = self.entries[entry].command;
        let command = &self.commands[command_index];
        let action = command.action();
        cx.global_mut::<RecentCommands>()
            .remember(&command.identity);
        // The dialog owns the keyboard while it is open, so the action has to
        // reach the view the palette opened from rather than whatever holds the
        // focus afterwards. Closing gives the keyboard back; the command then
        // runs on that view's own node one effect cycle later, because the
        // keystroke that submitted the palette has to finish first or its
        // remaining handlers would act on the command's own dialog.
        let focus = self.return_focus.clone();
        let handle = window.window_handle();
        window.close_dialog(cx);
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                if let Some(focus) = &focus {
                    focus.dispatch_action(&*action, window, cx);
                } else {
                    window.dispatch_action(action, cx);
                }
            });
        });
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        cx.stop_propagation();
    }

    fn row(&self, position: usize, entry: usize, query: &str, design: &Design) -> AnyElement {
        let command = &self.commands[self.entries[entry].command];
        // When only the English title matched, show it, or the row would seem
        // to have nothing to do with what was typed.
        let matched_alias = command
            .alias
            .as_ref()
            .filter(|_| !query.trim().is_empty() && score(query, &command.label).is_none());
        let entity = self.entity.clone();
        let mut row = ui::list_row(
            ("palette-command", position),
            ui::RowState::NORMAL.selected(position == self.selected),
            design,
        )
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            entity.update(cx, |palette, cx| palette.run(position, window, cx));
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(command.label.clone()),
        );
        if let Some(alias) = matched_alias {
            row = row.child(
                div()
                    .flex_none()
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child(alias.clone()),
            );
        }
        if let Some(binding) = &self.entries[entry].binding {
            row = row.child(ui::kbd(binding.clone(), design));
        }
        row.into_any_element()
    }
}

impl Render for Palette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let filtered = self.filtered(cx);
        let query = self.input.read(cx).value().to_string();

        let mut list = div().flex().flex_col().w_full().py_1();
        if filtered.is_empty() {
            list = list.child(ui::empty_state(t!("palette.no_match").to_string(), &design));
        } else {
            list = list.children(
                filtered
                    .iter()
                    .enumerate()
                    .map(|(position, &entry)| self.row(position, entry, &query, &design)),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .key_context(PALETTE)
            .on_action(cx.listener(|this, _: &PalettePrevious, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &PaletteNext, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &PaletteDismiss, window, cx| this.dismiss(window, cx)))
            .child(Input::new(&self.input))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_1()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(t!("palette.hint").to_string())
                    .child(if filtered.len() == 1 {
                        t!("palette.commands.one", count = filtered.len()).to_string()
                    } else {
                        t!("palette.commands.other", count = filtered.len()).to_string()
                    }),
            )
            .child(list)
    }
}

/// Opens the palette over the active window.
fn open(window: &mut Window, cx: &mut App) {
    // A modal already has the keyboard; opening a second one over it would
    // close the first instead of the palette's own card.
    if window.has_active_dialog(cx) {
        return;
    }
    let palette = cx.new(|cx| Palette::new(window, cx));
    let focus = palette.read(cx).focus.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let content = palette.clone();
        let zoom = Settings::get(cx).appearance.zoom();
        dialog
            // A palette is a keyboard surface: it has to be there the moment
            // the shortcut is pressed, and gone the moment Enter is.
            .entrance(DialogEntrance::None)
            .centered()
            .w(px(WIDTH * zoom))
            .close_button(false)
            // The field owns Enter: its `PressEnter` runs the command and
            // closes the card itself. Letting the dialog confirm as well would
            // close it a second time, and that path schedules a delayed focus
            // restore to the view that would pull the keyboard back out of the
            // field the command just focused.
            .on_ok(|_, _, _| false)
            .content(move |body, _, _| body.child(content.clone()))
    });
    // The dialog's card takes the focus when it opens; the field needs it to
    // receive what the user types.
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

/// Reads what the window can run and which key runs it.
fn snapshot(commands: &[Command], window: &Window, cx: &App) -> (Vec<Entry>, bool) {
    let available = window.available_actions(cx);
    let focus = window.focused(cx);
    let os = cx.platform_info().os;
    // Stashing is a repository command and every repository view handles it, so
    // its presence is what tells the palette a repository is on screen.
    let repo_open = holds(&available, &StashChanges);
    let entries = commands
        .iter()
        .enumerate()
        .map(|(command, spec)| {
            let action = spec.action();
            let handled = match spec.source {
                Source::Builtin(_) => holds(&available, &*action),
                // The action a custom command dispatches is handled by the
                // repository view, which is also what makes it runnable.
                Source::Custom(_) => repo_open,
            };
            // `bindings_for_action` reads the context stack the last render
            // left behind; asking for the focused handle's own stack is what
            // finds the view's bindings while the palette's dialog does not
            // exist yet.
            let binding = spec.key().map(|key| keymap::shortcut(key, os)).or_else(|| {
                focus.as_ref().and_then(|handle| {
                    window
                        .bindings_for_action_in(&*action, handle)
                        .last()
                        .map(|binding| SharedString::from(shortcut(binding, os)))
                })
            });
            Entry {
                command,
                handled,
                binding,
            }
        })
        .collect();
    (entries, repo_open)
}

/// Whether the focused view listens for `action`.
fn holds(available: &[Box<dyn Action>], action: &dyn Action) -> bool {
    let wanted = action.as_any().type_id();
    available
        .iter()
        .any(|candidate| candidate.as_any().type_id() == wanted)
}

/// The key that runs `action`, as the platform spells shortcuts.
pub(crate) fn shortcut(binding: &KeyBinding, os: OperatingSystem) -> String {
    binding
        .keystrokes()
        .iter()
        .map(|keystroke| shortcut_label(&keystroke.display(os).to_string()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Turns GPUI's keystroke text (`ctrl-shift-P`, `⌘⇧P`) into the form a shortcut
/// is written in: `Ctrl+Shift+P` on Windows and Linux, the symbols on macOS.
pub(crate) fn shortcut_label(keystroke: &str) -> String {
    // macOS prints the modifiers as one run of symbols, nothing to join.
    if keystroke.contains(['⌘', '⌥', '⇧', '⌃', '❖', '⊞']) {
        return keystroke.to_owned();
    }
    keystroke
        .split('-')
        .map(|part| {
            match part {
                "ctrl" => "Ctrl",
                "alt" => "Alt",
                "shift" => "Shift",
                "fn" => "Fn",
                "escape" => "Esc",
                "tab" => "Tab",
                "pageup" => "PgUp",
                "pagedown" => "PgDn",
                // Function keys print lower-case and are written upper-case.
                _ if part.len() > 1
                    && part.starts_with('f')
                    && part[1..].chars().all(|c| c.is_ascii_digit()) =>
                {
                    return part.to_uppercase();
                }
                other => other,
            }
            .to_owned()
        })
        .collect::<Vec<_>>()
        .join("+")
}

// ---------------------------------------------------------------------------
// Matching and ranking (pure, unit-tested at the bottom).
// ---------------------------------------------------------------------------

/// One thing the palette can offer, as [`rank`] sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate<'a> {
    pub(crate) label: &'a str,
    /// A second name the query may match instead, such as the English title.
    pub(crate) alias: Option<&'a str>,
    /// Whether this opening of the palette may show it.
    pub(crate) visible: bool,
    /// Position in the recently-used list; `None` when never run.
    pub(crate) recent: Option<usize>,
}

/// Indices into `candidates`, best match first.
///
/// An empty query keeps every visible candidate: recently run ones first, in
/// the order they were run, then the rest in table order. A query keeps only
/// the candidates its letters appear in, scored by [`score`], with recency
/// settling ties.
pub(crate) fn rank(candidates: &[Candidate<'_>], query: &str) -> Vec<usize> {
    let query = query.trim();
    let mut rows: Vec<(usize, i32, Option<usize>)> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.visible)
        .filter_map(|(index, candidate)| {
            if query.is_empty() {
                Some((index, 0, candidate.recent))
            } else {
                let label = score(query, candidate.label);
                let alias = candidate.alias.and_then(|alias| score(query, alias));
                label
                    .max(alias)
                    .map(|score| (index, score, candidate.recent))
            }
        })
        .collect();

    if query.is_empty() {
        rows.sort_by_key(|&(index, _, recent)| (recent.is_none(), recent.unwrap_or(0), index));
        rows.into_iter().map(|(index, _, _)| index).collect()
    } else {
        rows.sort_by_key(|&(index, score, recent)| {
            (
                std::cmp::Reverse(score),
                recent.is_none(),
                recent.unwrap_or(0),
                index,
            )
        });
        rows.into_iter().map(|(index, _, _)| index).collect()
    }
}

/// What one matched character is worth, before the position bonuses.
const MATCH: i32 = 10;
/// The first character of the label matched, or the whole query is its prefix.
const PREFIX_BONUS: i32 = 40;
/// The first character of a word in the label matched.
const WORD_START_BONUS: i32 = 25;
/// The character follows the one matched before it.
const CONSECUTIVE_BONUS: i32 = 15;

/// How well `label` matches `query`, or `None` when it does not.
///
/// The match is a case-insensitive subsequence: every letter of the query has
/// to appear in the label, in order, so `pu` finds "Push branch" but not
/// "Pull request" (no `u` after the `p`). Higher is better: a prefix, a word
/// start and runs of adjacent letters score above letters scattered through
/// the label.
pub(crate) fn score(query: &str, label: &str) -> Option<i32> {
    let query: Vec<char> = query.trim().chars().flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return Some(0);
    }
    let label: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
    let prefix = label.starts_with(&query);

    let mut total = 0;
    let mut cursor = 0;
    let mut previous: Option<usize> = None;
    for wanted in &query {
        let position = cursor + label.get(cursor..)?.iter().position(|c| c == wanted)?;
        total += MATCH;
        if position == 0 {
            total += PREFIX_BONUS;
        } else if is_word_start(&label, position) {
            total += WORD_START_BONUS;
        }
        match previous {
            Some(last) if position == last + 1 => total += CONSECUTIVE_BONUS,
            Some(last) => total -= (position - last - 1) as i32,
            None => {}
        }
        previous = Some(position);
        cursor = position + 1;
    }
    if prefix {
        total += PREFIX_BONUS;
    }
    Some(total)
}

fn is_word_start(label: &[char], position: usize) -> bool {
    position > 0 && matches!(label[position - 1], ' ' | '-' | '_' | '/' | '.' | ':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(labels: &[&'static str]) -> Vec<Candidate<'static>> {
        labels
            .iter()
            .map(|label| Candidate {
                label,
                alias: None,
                visible: true,
                recent: None,
            })
            .collect()
    }

    #[test]
    fn a_command_is_found_by_its_english_title_too() {
        let candidates = [
            Candidate {
                label: "リモートをフェッチ",
                alias: Some("Fetch remotes"),
                visible: true,
                recent: None,
            },
            Candidate {
                label: "ブランチをプッシュ",
                alias: Some("Push branch"),
                visible: true,
                recent: None,
            },
        ];
        assert_eq!(rank(&candidates, "fetch"), vec![0]);
        assert_eq!(rank(&candidates, "プッシュ"), vec![1]);
    }

    #[test]
    fn letters_match_in_order_ignoring_case() {
        assert!(score("pu", "Push branch").is_some());
        assert!(score("PU", "push branch").is_some());
        assert!(score("sh", "Stash changes").is_some());
        assert_eq!(score("", "anything"), Some(0), "an empty query matches");
        assert_eq!(score("  ", "anything"), Some(0), "so does blank space");
        assert_eq!(score("xb", "Push branch"), None, "missing letters fail");
        assert_eq!(score("ubp", "Push branch"), None, "order matters");
    }

    #[test]
    fn word_starts_and_prefixes_beat_scattered_letters() {
        let prefix = score("op", "Open settings").unwrap();
        let word_start = score("op", "Reorder open").unwrap();
        assert!(prefix > word_start, "{prefix} should beat {word_start}");
        let adjacent = score("st", "Stash changes").unwrap();
        let split = score("st", "Sort items").unwrap();
        assert!(adjacent > split, "{adjacent} should beat {split}");
    }

    #[test]
    fn the_best_match_is_first_and_non_matches_are_dropped() {
        let labels = [
            "Open repository…",
            "Pull branch",
            "Push branch",
            "Search commits",
        ];
        assert_eq!(rank(&candidates(&labels), "pu"), vec![1, 2]);
        assert!(rank(&candidates(&labels), "zz").is_empty());
    }

    #[test]
    fn an_empty_query_lists_recent_commands_first() {
        let mut candidates = candidates(&["Open repository…", "Open settings", "Fetch remotes"]);
        candidates[2].recent = Some(0);
        candidates[1].recent = Some(1);
        assert_eq!(rank(&candidates, ""), vec![2, 1, 0]);
        assert_eq!(rank(&candidates, "   "), vec![2, 1, 0]);
    }

    #[test]
    fn recency_breaks_ties_and_hidden_commands_stay_hidden() {
        let mut candidates = candidates(&["Pull branch", "Push branch", "Fetch remotes"]);
        candidates[1].recent = Some(0);
        // Both labels score the same for "pu"; the one just run wins.
        assert_eq!(rank(&candidates, "pu"), vec![1, 0]);
        candidates[2].visible = false;
        assert!(rank(&candidates, "fe").is_empty());
        assert_eq!(rank(&candidates, ""), vec![1, 0]);
    }

    #[test]
    fn remembering_a_command_moves_it_to_the_front_and_caps_the_list() {
        let mut recents = RecentCommands::default();
        recents.remember("SwitchBranch");
        recents.remember("Quit");
        recents.remember("SwitchBranch");
        assert_eq!(recents.0, vec!["SwitchBranch", "Quit"]);
        for command in 0..MAX_RECENT + 2 {
            recents.remember(&format!("command-{command}"));
        }
        assert_eq!(recents.0.len(), MAX_RECENT);
        assert_eq!(recents.0[0], format!("command-{}", MAX_RECENT + 1));
        assert_eq!(recents.position("SwitchBranch"), None);
    }

    #[test]
    fn shortcuts_are_spelled_the_way_each_platform_does() {
        assert_eq!(shortcut_label("ctrl-shift-P"), "Ctrl+Shift+P");
        assert_eq!(shortcut_label("ctrl-alt-L"), "Ctrl+Alt+L");
        assert_eq!(shortcut_label("f12"), "F12");
        assert_eq!(shortcut_label("escape"), "Esc");
        assert_eq!(shortcut_label("⌘⇧P"), "⌘⇧P");
        assert_eq!(shortcut_label("⌘,"), "⌘,");
    }

    #[test]
    fn every_palette_command_has_one_title_and_one_action() {
        let specs: Vec<&ActionSpec> = keymap::ACTIONS
            .iter()
            .filter(|spec| spec.palette.is_some())
            .collect();
        let expected_count = if cfg!(feature = "terminal") { 52 } else { 51 };
        assert_eq!(specs.len(), expected_count);
        for (index, spec) in specs.iter().enumerate() {
            assert!(
                !spec.title.is_empty() && spec.title.len() <= 40,
                "labels stay readable: {}",
                spec.title
            );
            let action = (spec.build)();
            assert!(
                specs
                    .iter()
                    .skip(index + 1)
                    .all(|other| (other.build)().as_any().type_id() != action.as_any().type_id()),
                "duplicate action for {}",
                spec.title
            );
        }
    }
}
