//! Rewriting commits that sit behind a merge, or that a merge brought in.
//!
//! `git rebase -i` refuses a merge in the range. `--rebase-merges` does not:
//! git prints a todo whose `merge -C <original>` lines recreate each merge, and
//! the sequencer runs that todo the same way it runs a linear one. The branch
//! points in that todo (`label`, `reset`) are git's; rebuilding them here would
//! risk dropping a side branch or replaying a commit twice. So the plan is
//! captured from git, one `pick` is edited, and the result is handed back
//! through the same sequence editor the linear rebase uses.
//!
//! The capture points `GIT_SEQUENCE_EDITOR` at a command that copies the
//! generated todo and then exits non-zero. Git aborts before replaying
//! anything, which leaves the repository out of a rebase. The real run uses
//! `GIT_SEQUENCE_EDITOR=cp` and `GIT_EDITOR=true`, as [`super::rebase`] does, so
//! a squash keeps the message git prepared and a conflict or an `edit` stop is
//! an ordinary in-progress rebase.
//!
//! `commit.gpgsign` is left to git. This path does not pass `--no-gpg-sign`, so
//! a repository that signs commits signs the replayed ones too.

use std::collections::HashSet;
use std::fs;

use crate::repo::Repo;
use crate::{Error, HeadInfo, Oid, RepoState, Result};

use super::Git;
use super::rebase::{MoveDirection, Scratch, shell_quote, state_name};

/// How many commits a walk follows before it gives up. A history deeper than
/// this is not one a single menu action is meant to rewrite.
const LINE_LIMIT: usize = 100_000;

/// What to do to one commit that HEAD can reach, replaying through merges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcrossMergesAction {
    /// Leave the commit out of the rewritten history.
    Drop,
    /// Meld the commit into its parent, keeping both messages.
    Squash,
    /// Meld the commit into its parent and keep the parent's message.
    Fixup,
    /// Swap with the neighbouring `pick` on the same chain.
    Move(MoveDirection),
    /// Replay the commit and stop so it can be amended.
    Edit,
}

impl Git {
    /// Rewrite `target` with `git rebase -i --rebase-merges`.
    ///
    /// `target` may lie on HEAD's first-parent line or on a side branch that a
    /// merge brought in. The checked-out branch stays checked out. A conflict,
    /// or an [`AcrossMergesAction::Edit`] stop, returns `Ok` and leaves
    /// [`RepoState::Rebasing`]; [`Git::continue_in_progress`] and
    /// [`Git::abort_in_progress`] finish it.
    ///
    /// Refused when HEAD is detached, when an operation is already in progress,
    /// when the worktree has tracked changes, when HEAD cannot reach `target`,
    /// and when `target` is itself a merge. A squash, fixup or downward move
    /// also needs the parent to be a non-merge commit on the same chain of the
    /// plan; anything else is [`Error::Other`].
    pub fn rewrite_across_merges(&self, target: Oid, action: AcrossMergesAction) -> Result<()> {
        if target.is_null() {
            return Err(Error::other("cannot rewrite: no commit was given"));
        }
        let repo = Repo::open(self.workdir())?;
        let state = repo.state()?;
        if state != RepoState::Normal {
            return Err(Error::other(format!(
                "cannot rewrite: {} is in progress; finish or abort it first",
                state_name(state)
            )));
        }
        let head = match repo.head()? {
            HeadInfo::Branch {
                target: Some(head), ..
            } => head,
            HeadInfo::Branch { target: None, .. } => {
                return Err(Error::other("cannot rewrite: the branch has no commits"));
            }
            HeadInfo::Detached { .. } => {
                return Err(Error::other(
                    "cannot rewrite: HEAD is detached; check out a branch first",
                ));
            }
        };
        // Untracked files do not block a rebase; tracked ones would mix with a
        // conflict and could not be told apart afterwards.
        let status = self.run(&["status", "--porcelain", "--untracked-files=no"], None)?;
        if !status.stdout.iter().all(u8::is_ascii_whitespace) {
            return Err(Error::other(
                "cannot rewrite: the working tree has uncommitted changes; \
                 commit or stash them first",
            ));
        }
        if !repo.is_ancestor(target, head)? {
            return Err(Error::other(format!(
                "cannot rewrite {}: HEAD does not reach it",
                short(target)
            )));
        }
        let parents = repo.parents(target)?;
        if parents.len() > 1 {
            return Err(Error::other(format!(
                "cannot {} {}: it is a merge commit",
                verb(action),
                short(target)
            )));
        }

        let onto = choose_onto(&repo, head, target, action, &parents)?;
        let scratch = Scratch::new()?;
        let plan = self.capture_plan(onto, &scratch)?;
        let parent = parents.first().map(|id| id.to_hex().to_string());
        let rewritten = rewrite_merges_todo(
            &plan,
            &target.to_hex().to_string(),
            parent.as_deref(),
            action,
        )?;
        self.run_plan(onto, &rewritten, &scratch)
    }

    /// Copy the todo `git rebase -i --rebase-merges` would open, then make git
    /// abort before it replays a commit.
    fn capture_plan(&self, onto: Oid, scratch: &Scratch) -> Result<String> {
        let generated = scratch.path("plan");
        // `$1` is the todo path git appends. Exiting non-zero is what makes git
        // abandon the rebase; `cp` the other way, as the real run does, would
        // replace the plan with an empty file instead of reading it.
        let editor = format!(
            "f() {{ cp \"$1\" {}; exit 1; }}; f",
            shell_quote(&generated)
        );
        let onto = onto.to_string();
        let result = self
            .clone()
            .with_env("GIT_SEQUENCE_EDITOR", editor)
            .with_env("GIT_EDITOR", "true")
            .run_internal(&["rebase", "-i", "--rebase-merges", &onto], None);

        if self.state()? == RepoState::Rebasing {
            let _ = self.run(&["rebase", "--abort"], None);
            return Err(Error::other(
                "cannot rewrite: reading the rebase plan left a rebase in progress",
            ));
        }
        // The editor exits non-zero on purpose, after the copy. Success means
        // git ignored that and replayed the branch; the copy is not a plan we
        // can trust on top of a history that already moved.
        match result {
            Ok(_) => Err(Error::other(
                "cannot rewrite: git ran the rebase instead of handing over its plan",
            )),
            Err(_) if generated.is_file() => {
                let text = fs::read_to_string(&generated)?;
                if text.trim().is_empty() {
                    return Err(Error::other(
                        "cannot rewrite: git produced an empty rebase plan",
                    ));
                }
                Ok(text)
            }
            Err(error) => Err(error),
        }
    }

    /// Run `todo` as `git rebase -i --rebase-merges`. A conflict or an `edit`
    /// stop leaves the rebase in progress and is `Ok`.
    fn run_plan(&self, onto: Oid, todo: &str, scratch: &Scratch) -> Result<()> {
        let path = scratch.path("todo");
        fs::write(&path, todo)?;
        let onto = onto.to_string();
        let result = self
            .clone()
            .with_env("GIT_SEQUENCE_EDITOR", format!("cp {}", shell_quote(&path)))
            .with_env("GIT_EDITOR", "true")
            .run(
                &[
                    "rebase",
                    "-i",
                    "--rebase-merges",
                    // An empty pick must stop. Dropping it would silently lose a
                    // commit the plan still named.
                    "--empty=stop",
                    &onto,
                ],
                None,
            );
        match result {
            Ok(_) => Ok(()),
            Err(error @ Error::GitCommand { code: Some(1), .. }) => {
                if self.state()? == RepoState::Rebasing {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }
}

/// Apply `action` to `target` inside a `git rebase --rebase-merges` todo.
///
/// `parent` is the first parent of `target`. Squash and fixup require it to be
/// the previous `pick` on the same chain; the other actions ignore it. Commit
/// ids may be abbreviated the way the todo writes them, as long as one is a
/// prefix of the other.
pub fn rewrite_merges_todo(
    todo: &str,
    target: &str,
    parent: Option<&str>,
    action: AcrossMergesAction,
) -> Result<String> {
    let target = target.trim();
    if !is_hex_oid(target) {
        return Err(Error::other(
            "cannot rewrite: the commit id is not a hex object name",
        ));
    }
    let mut lines = parse_todo(todo)?;
    apply(&mut lines, target, parent, action)?;
    Ok(render_todo(&lines))
}

/// The commit the replay starts after.
///
/// On the first-parent line that is the parent of the oldest commit the action
/// has to see: the target itself, or its parent when the parent must be in the
/// plan (squash, fixup, and a downward move). A merged-in commit instead uses
/// the mainline commit its side branch forked from, so every commit on that
/// side is part of `onto..HEAD`.
fn choose_onto(
    repo: &Repo,
    head: Oid,
    target: Oid,
    action: AcrossMergesAction,
    parents: &[Oid],
) -> Result<Oid> {
    let line = first_parent_line(repo, head)?;
    if !line.contains(&target) {
        let mainline = line.into_iter().collect();
        return fork_point(repo, &mainline, target);
    }

    let anchor = if needs_parent_in_plan(action) {
        let Some(parent) = parents.first().copied() else {
            return Err(no_base(action, target, false));
        };
        parent
    } else {
        target
    };
    let onto = if anchor == target {
        parents.first().copied()
    } else {
        repo.parents(anchor)?.first().copied()
    };
    onto.ok_or_else(|| no_base(action, target, anchor != target))
}

/// Squash and fixup meld into the parent, and a downward move swaps with it, so
/// the parent has to be a line of the plan rather than the `onto` commit.
fn needs_parent_in_plan(action: AcrossMergesAction) -> bool {
    matches!(
        action,
        AcrossMergesAction::Squash
            | AcrossMergesAction::Fixup
            | AcrossMergesAction::Move(MoveDirection::Down)
    )
}

fn no_base(action: AcrossMergesAction, target: Oid, parent_is_root: bool) -> Error {
    let verb = verb(action);
    let id = short(target);
    if parent_is_root {
        Error::other(format!(
            "cannot {verb} {id}: its parent is a root commit, so there is no base to replay onto"
        ))
    } else {
        Error::other(format!(
            "cannot {verb} {id}: it is a root commit, so there is no base to replay onto"
        ))
    }
}

fn first_parent_line(repo: &Repo, head: Oid) -> Result<Vec<Oid>> {
    let mut line = Vec::new();
    let mut current = head;
    loop {
        if line.len() >= LINE_LIMIT {
            return Err(Error::other(format!(
                "cannot rewrite: the first-parent line is longer than {LINE_LIMIT} commits"
            )));
        }
        line.push(current);
        match repo.parents(current)?.first() {
            Some(parent) => current = *parent,
            None => return Ok(line),
        }
    }
}

/// The mainline commit `target`'s side branch forked from.
fn fork_point(repo: &Repo, mainline: &HashSet<Oid>, target: Oid) -> Result<Oid> {
    let mut current = target;
    for _ in 0..LINE_LIMIT {
        let parents = repo.parents(current)?;
        let Some(parent) = parents.first().copied() else {
            return Err(Error::other(format!(
                "cannot rewrite {}: it does not fork from the first-parent line of HEAD",
                short(target)
            )));
        };
        if mainline.contains(&parent) {
            return Ok(parent);
        }
        current = parent;
    }
    Err(Error::other(format!(
        "cannot rewrite {}: the walk from it did not meet the first-parent line within {LINE_LIMIT} commits",
        short(target)
    )))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Blank,
    Comment(String),
    Commit {
        command: CommitCommand,
        hash: String,
        /// Tokens such as `fixup`'s `-C`, kept so a plan round-trips.
        flags: Vec<String>,
        comment: Option<String>,
    },
    Label {
        name: String,
        comment: Option<String>,
    },
    Reset {
        target: String,
        comment: Option<String>,
    },
    Merge {
        original: Option<MergeOriginal>,
        labels: Vec<String>,
        comment: Option<String>,
    },
    UpdateRef {
        name: String,
        comment: Option<String>,
    },
    Exec {
        command: String,
        comment: Option<String>,
    },
    Break {
        comment: Option<String>,
    },
    /// A command this parser does not model. Kept verbatim so a newer git does
    /// not lose a line when the plan is written back.
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitCommand {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeOriginal {
    /// `-c` opens the message editor. `-C` reuses the message.
    reword: bool,
    commit: String,
}

fn parse_todo(todo: &str) -> Result<Vec<Line>> {
    todo.lines().map(parse_line).collect()
}

fn parse_line(raw: &str) -> Result<Line> {
    if raw.trim().is_empty() {
        return Ok(Line::Blank);
    }
    if raw.trim_start().starts_with('#') {
        return Ok(Line::Comment(raw.trim_end().to_owned()));
    }
    let (body, comment) = split_inline_comment(raw.trim());
    let mut parts = body.split_whitespace();
    let Some(verb) = parts.next() else {
        return Err(bad_line(raw));
    };
    let rest: Vec<&str> = parts.collect();
    let verb = verb.to_ascii_lowercase();
    match verb.as_str() {
        "pick" | "p" => parse_commit(CommitCommand::Pick, &rest, comment, raw),
        "reword" | "r" => parse_commit(CommitCommand::Reword, &rest, comment, raw),
        "edit" | "e" => parse_commit(CommitCommand::Edit, &rest, comment, raw),
        "squash" | "s" => parse_commit(CommitCommand::Squash, &rest, comment, raw),
        "fixup" | "f" => parse_commit(CommitCommand::Fixup, &rest, comment, raw),
        "drop" | "d" => parse_commit(CommitCommand::Drop, &rest, comment, raw),
        "label" | "l" => one_arg(&rest, raw).map(|name| Line::Label {
            name: name.to_owned(),
            comment,
        }),
        "reset" | "t" => one_arg(&rest, raw).map(|target| Line::Reset {
            target: target.to_owned(),
            comment,
        }),
        "merge" | "m" => parse_merge(&rest, comment, raw),
        "update-ref" | "u" => one_arg(&rest, raw).map(|name| Line::UpdateRef {
            name: name.to_owned(),
            comment,
        }),
        "exec" | "x" => {
            if rest.is_empty() {
                return Err(bad_line(raw));
            }
            Ok(Line::Exec {
                command: rest.join(" "),
                comment,
            })
        }
        "break" | "b" => {
            if !rest.is_empty() {
                return Err(bad_line(raw));
            }
            Ok(Line::Break { comment })
        }
        _ => Ok(Line::Other(raw.trim_end().to_owned())),
    }
}

fn parse_commit(
    command: CommitCommand,
    rest: &[&str],
    comment: Option<String>,
    line: &str,
) -> Result<Line> {
    let mut flags = Vec::new();
    let mut hash = None;
    for token in rest {
        if hash.is_none() && (*token == "-C" || *token == "-c") {
            if !flags.is_empty() {
                return Err(bad_line(line));
            }
            flags.push((*token).to_owned());
            continue;
        }
        if hash.is_none() && is_hex_oid(token) {
            hash = Some((*token).to_owned());
            continue;
        }
        return Err(bad_line(line));
    }
    let Some(hash) = hash else {
        return Err(bad_line(line));
    };
    Ok(Line::Commit {
        command,
        hash,
        flags,
        comment,
    })
}

fn parse_merge(rest: &[&str], comment: Option<String>, line: &str) -> Result<Line> {
    let mut original = None;
    let mut labels = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let token = rest[index];
        if token == "-C" || token == "-c" {
            if original.is_some() {
                return Err(bad_line(line));
            }
            let Some(hash) = rest.get(index + 1) else {
                return Err(bad_line(line));
            };
            if !is_hex_oid(hash) {
                return Err(bad_line(line));
            }
            original = Some(MergeOriginal {
                reword: token == "-c",
                commit: (*hash).to_owned(),
            });
            index += 2;
            continue;
        }
        labels.push(token.to_owned());
        index += 1;
    }
    if labels.is_empty() {
        return Err(bad_line(line));
    }
    Ok(Line::Merge {
        original,
        labels,
        comment,
    })
}

fn one_arg<'a>(rest: &[&'a str], line: &str) -> Result<&'a str> {
    match rest {
        [token] => Ok(*token),
        _ => Err(bad_line(line)),
    }
}

/// Split ` # subject` off a command. A `#` only starts a comment when it is
/// preceded by whitespace, which is how git writes the subject.
fn split_inline_comment(line: &str) -> (&str, Option<String>) {
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'#' && index > 0 && bytes[index - 1].is_ascii_whitespace() {
            let comment = line[index + 1..].trim();
            let comment = if comment.is_empty() {
                None
            } else {
                Some(comment.to_owned())
            };
            return (line[..index].trim_end(), comment);
        }
    }
    (line, None)
}

fn bad_line(line: &str) -> Error {
    let shown: String = line.trim().chars().take(80).collect();
    Error::other(format!("cannot read the rebase plan: {shown}"))
}

fn is_hex_oid(token: &str) -> bool {
    (4..=64).contains(&token.len()) && token.chars().all(|character| character.is_ascii_hexdigit())
}

fn render_todo(lines: &[Line]) -> String {
    let mut text = String::new();
    for line in lines {
        text.push_str(&render_line(line));
        text.push('\n');
    }
    text
}

fn render_line(line: &Line) -> String {
    match line {
        Line::Blank => String::new(),
        Line::Comment(text) => text.clone(),
        Line::Other(text) => text.clone(),
        Line::Commit {
            command,
            hash,
            flags,
            comment,
        } => {
            let mut text = command_name(*command).to_owned();
            for flag in flags {
                text.push(' ');
                text.push_str(flag);
            }
            text.push(' ');
            text.push_str(hash);
            push_comment(&mut text, comment);
            text
        }
        Line::Label { name, comment } => {
            let mut text = format!("label {name}");
            push_comment(&mut text, comment);
            text
        }
        Line::Reset { target, comment } => {
            let mut text = format!("reset {target}");
            push_comment(&mut text, comment);
            text
        }
        Line::Merge {
            original,
            labels,
            comment,
        } => {
            let mut text = String::from("merge");
            if let Some(original) = original {
                text.push(' ');
                text.push_str(if original.reword { "-c" } else { "-C" });
                text.push(' ');
                text.push_str(&original.commit);
            }
            for label in labels {
                text.push(' ');
                text.push_str(label);
            }
            push_comment(&mut text, comment);
            text
        }
        Line::UpdateRef { name, comment } => {
            let mut text = format!("update-ref {name}");
            push_comment(&mut text, comment);
            text
        }
        Line::Exec { command, comment } => {
            let mut text = format!("exec {command}");
            push_comment(&mut text, comment);
            text
        }
        Line::Break { comment } => {
            let mut text = String::from("break");
            push_comment(&mut text, comment);
            text
        }
    }
}

fn command_name(command: CommitCommand) -> &'static str {
    match command {
        CommitCommand::Pick => "pick",
        CommitCommand::Reword => "reword",
        CommitCommand::Edit => "edit",
        CommitCommand::Squash => "squash",
        CommitCommand::Fixup => "fixup",
        CommitCommand::Drop => "drop",
    }
}

fn push_comment(line: &mut String, comment: &Option<String>) {
    if let Some(comment) = comment {
        line.push_str(" # ");
        line.push_str(comment);
    }
}

fn apply(
    lines: &mut [Line],
    target: &str,
    parent: Option<&str>,
    action: AcrossMergesAction,
) -> Result<()> {
    let index = find_target(lines, target, action)?;
    match action {
        AcrossMergesAction::Drop => set_pick(lines, index, CommitCommand::Drop, action, target),
        AcrossMergesAction::Edit => set_pick(lines, index, CommitCommand::Edit, action, target),
        AcrossMergesAction::Squash => {
            squash_into_parent(lines, index, parent, CommitCommand::Squash, action, target)
        }
        AcrossMergesAction::Fixup => {
            squash_into_parent(lines, index, parent, CommitCommand::Fixup, action, target)
        }
        AcrossMergesAction::Move(direction) => move_pick(lines, index, direction, action, target),
    }
}

fn find_target(lines: &[Line], target: &str, action: AcrossMergesAction) -> Result<usize> {
    let mut found = Vec::new();
    let mut merge = false;
    for (index, line) in lines.iter().enumerate() {
        match line {
            Line::Commit { hash, .. } if same_commit(hash, target) => found.push(index),
            Line::Merge {
                original: Some(source),
                ..
            } if same_commit(&source.commit, target) => merge = true,
            _ => {}
        }
    }
    match found.as_slice() {
        [index] => Ok(*index),
        [] if merge => Err(fail(action, target, "it is a merge commit")),
        [] => Err(fail(action, target, "it is not in the rebase plan")),
        _ => Err(fail(
            action,
            target,
            "it appears more than once in the rebase plan",
        )),
    }
}

fn set_pick(
    lines: &mut [Line],
    index: usize,
    command: CommitCommand,
    action: AcrossMergesAction,
    target: &str,
) -> Result<()> {
    ensure_pick(lines, index, action, target)?;
    if let Line::Commit {
        command: slot,
        flags,
        ..
    } = &mut lines[index]
    {
        *slot = command;
        flags.clear();
    }
    Ok(())
}

fn squash_into_parent(
    lines: &mut [Line],
    index: usize,
    parent: Option<&str>,
    command: CommitCommand,
    action: AcrossMergesAction,
    target: &str,
) -> Result<()> {
    ensure_pick(lines, index, action, target)?;
    let Some(parent) = parent.filter(|parent| is_hex_oid(parent)) else {
        return Err(fail(
            action,
            target,
            "its parent is not a commit on the same chain",
        ));
    };
    match neighbor(lines, index, MoveDirection::Down) {
        Step::Commit(previous) => match &lines[previous] {
            Line::Commit {
                command: CommitCommand::Pick,
                hash,
                ..
            } if same_commit(hash, parent) => set_pick(lines, index, command, action, target),
            Line::Commit { .. } => Err(fail(
                action,
                target,
                "the previous commit on this chain is not its parent",
            )),
            _ => Err(fail(
                action,
                target,
                "its parent is not a commit on the same chain",
            )),
        },
        Step::Blocked {
            kind: Block::Merge,
            index: at,
        } => {
            let is_parent = matches!(
                &lines[at],
                Line::Merge { original: Some(source), .. } if same_commit(&source.commit, parent)
            );
            if is_parent {
                Err(fail(action, target, "the parent is a merge commit"))
            } else {
                Err(fail(
                    action,
                    target,
                    "its parent is not a commit on the same chain",
                ))
            }
        }
        Step::Blocked { .. } | Step::End => Err(fail(
            action,
            target,
            "its parent is not a commit on the same chain",
        )),
    }
}

fn move_pick(
    lines: &mut [Line],
    index: usize,
    direction: MoveDirection,
    action: AcrossMergesAction,
    target: &str,
) -> Result<()> {
    ensure_pick(lines, index, action, target)?;
    match neighbor(lines, index, direction) {
        Step::Commit(other) => {
            if !matches!(
                lines[other],
                Line::Commit {
                    command: CommitCommand::Pick,
                    ..
                }
            ) {
                return Err(fail(action, target, "the neighbour is not a pick"));
            }
            lines.swap(index, other);
            Ok(())
        }
        Step::Blocked { kind, .. } => Err(fail(action, target, kind.reason())),
        Step::End => {
            let reason = match direction {
                MoveDirection::Up => "it is already the newest commit on its chain",
                MoveDirection::Down => "it is already the oldest commit on its chain",
            };
            Err(fail(action, target, reason))
        }
    }
}

fn ensure_pick(
    lines: &[Line],
    index: usize,
    action: AcrossMergesAction,
    target: &str,
) -> Result<()> {
    match &lines[index] {
        Line::Commit {
            command: CommitCommand::Pick,
            ..
        } => Ok(()),
        _ => Err(fail(action, target, "it is not picked in the plan")),
    }
}

enum Step {
    Commit(usize),
    Blocked { kind: Block, index: usize },
    End,
}

#[derive(Clone, Copy)]
enum Block {
    Merge,
    Label,
    Reset,
    UpdateRef,
    Exec,
    Break,
    Other,
}

impl Block {
    fn reason(self) -> &'static str {
        match self {
            Self::Merge => "a merge sits next to it",
            Self::Label => "a branch label sits next to it",
            Self::Reset => "a branch reset sits next to it",
            Self::UpdateRef => "a branch update sits next to it",
            Self::Exec => "a command sits next to it",
            Self::Break => "a break sits next to it",
            Self::Other => "the chain stops next to it",
        }
    }
}

fn neighbor(lines: &[Line], index: usize, direction: MoveDirection) -> Step {
    let delta: isize = match direction {
        MoveDirection::Up => 1,
        MoveDirection::Down => -1,
    };
    let mut cursor = index as isize;
    loop {
        cursor += delta;
        if cursor < 0 || cursor >= lines.len() as isize {
            return Step::End;
        }
        let at = cursor as usize;
        match &lines[at] {
            Line::Blank | Line::Comment(_) => continue,
            Line::Commit { .. } => return Step::Commit(at),
            Line::Merge { .. } => {
                return Step::Blocked {
                    kind: Block::Merge,
                    index: at,
                };
            }
            Line::Label { .. } => {
                return Step::Blocked {
                    kind: Block::Label,
                    index: at,
                };
            }
            Line::Reset { .. } => {
                return Step::Blocked {
                    kind: Block::Reset,
                    index: at,
                };
            }
            Line::UpdateRef { .. } => {
                return Step::Blocked {
                    kind: Block::UpdateRef,
                    index: at,
                };
            }
            Line::Exec { .. } => {
                return Step::Blocked {
                    kind: Block::Exec,
                    index: at,
                };
            }
            Line::Break { .. } => {
                return Step::Blocked {
                    kind: Block::Break,
                    index: at,
                };
            }
            Line::Other(_) => {
                return Step::Blocked {
                    kind: Block::Other,
                    index: at,
                };
            }
        }
    }
}

/// `token` and `candidate` name the same commit when the shorter one is a
/// prefix of the longer. Git writes a unique abbreviation, so a prefix of the
/// target is that commit.
fn same_commit(token: &str, candidate: &str) -> bool {
    let token = token.trim();
    let candidate = candidate.trim();
    if !is_hex_oid(token) || !is_hex_oid(candidate) {
        return false;
    }
    let (short, long) = if token.len() <= candidate.len() {
        (token, candidate)
    } else {
        (candidate, token)
    };
    long.get(..short.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(short))
}

fn fail(action: AcrossMergesAction, target: &str, reason: &str) -> Error {
    Error::other(format!(
        "cannot {} {}: {reason}",
        verb(action),
        short_hex(target)
    ))
}

fn verb(action: AcrossMergesAction) -> &'static str {
    match action {
        AcrossMergesAction::Drop => "drop",
        AcrossMergesAction::Squash => "squash",
        AcrossMergesAction::Fixup => "fixup",
        AcrossMergesAction::Move(MoveDirection::Up) => "move up",
        AcrossMergesAction::Move(MoveDirection::Down) => "move down",
        AcrossMergesAction::Edit => "edit",
    }
}

fn short(id: Oid) -> String {
    short_hex(&id.to_hex().to_string()).to_owned()
}

fn short_hex(hex: &str) -> &str {
    let hex = hex.trim();
    &hex[..7.min(hex.len())]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::test_support::TestRepo;

    const C2: &str = "1111111111111111111111111111111111111111";
    const T1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const T2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const M1: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const C3: &str = "dddddddddddddddddddddddddddddddddddddddd";
    const U1: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const M2: &str = "ffffffffffffffffffffffffffffffffffffffff";
    const C4: &str = "4444444444444444444444444444444444444444";

    /// The shape `git rebase -i --rebase-merges` writes for
    /// `c1-c2-(t1-t2)-M1-c3-(u1)-M2-c4`, including an `update-ref` where a
    /// config with `rebase.updateRefs` puts one.
    fn plan() -> String {
        format!(
            "\
label onto

# Branch topic
reset onto
pick {C2} # c2
label branch-point
pick {T1} # t1
pick {T2} # t2
update-ref refs/heads/topic

label topic

reset branch-point # c2
merge -C {M1} topic # M1
pick {C3} # c3
label branch-point-2
pick {U1} # u1
update-ref refs/heads/topic2

label topic2

reset branch-point-2 # c3
merge -C {M2} topic2 # M2
pick {C4} # c4

# Rebase commands are listed above.
"
        )
    }

    fn commands(todo: &str) -> Vec<String> {
        todo.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect()
    }

    fn has_command(todo: &str, expected: &str) -> bool {
        commands(todo).iter().any(|line| line == expected)
    }

    #[test]
    fn the_plan_round_trips_and_keeps_every_command_kind() {
        let lines = parse_todo(&plan()).expect("plan parses");
        assert_eq!(
            parse_todo(&render_todo(&lines)).expect("render parses"),
            lines
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| matches!(line, Line::Merge { .. }))
                .count(),
            2
        );

        let rich = "\
label onto
reset onto
pick 0123456789abcdef0123456789abcdef01234567 # kept
reword abcdef0123456789abcdef0123456789abcdef01
fixup -C fedcba9876543210fedcba9876543210fedcba98 # folded
exec echo kept
break # pause
merge -C 00112233445566778899aabbccddeeff00112233 left right # octopus
update-ref refs/heads/side
# pick 0123456789abcdef0123456789abcdef01234567
something-unknown
";
        let lines = parse_todo(rich).expect("rich plan parses");
        let rendered = render_todo(&lines);
        assert_eq!(parse_todo(&rendered).expect("rich render parses"), lines);
        assert!(
            rendered
                .contains("merge -C 00112233445566778899aabbccddeeff00112233 left right # octopus"),
            "{rendered}"
        );
        assert!(rendered.contains("fixup -C fedcba9876543210fedcba9876543210fedcba98 # folded"));
        assert!(rendered.contains("something-unknown"));
        assert!(rendered.contains("# pick 0123456789abcdef0123456789abcdef01234567"));
    }

    #[test]
    fn a_malformed_command_is_refused() {
        let error = parse_todo("pick\n").expect_err("a pick needs a commit");
        assert!(error.to_string().contains("rebase plan"), "{error}");
        let error =
            parse_todo("merge -C abc\n").expect_err("the hash is too short and there is no label");
        assert!(error.to_string().contains("rebase plan"), "{error}");
    }

    #[test]
    fn drop_and_edit_change_only_the_matching_pick() {
        let dropped =
            rewrite_merges_todo(&plan(), C2, None, AcrossMergesAction::Drop).expect("drop");
        assert!(has_command(&dropped, &format!("drop {C2} # c2")));
        assert!(
            !commands(&dropped)
                .iter()
                .any(|line| line.starts_with("pick") && line.contains(C2))
        );
        assert!(has_command(&dropped, &format!("merge -C {M1} topic # M1")));
        assert!(has_command(&dropped, "update-ref refs/heads/topic"));
        assert!(dropped.contains("# Rebase commands are listed above."));

        let edited =
            rewrite_merges_todo(&plan(), C2, None, AcrossMergesAction::Edit).expect("edit");
        assert!(has_command(&edited, &format!("edit {C2} # c2")));
        assert!(has_command(&edited, &format!("pick {T1} # t1")));
    }

    #[test]
    fn an_abbreviated_hash_and_a_short_command_match() {
        let full = "aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111";
        let todo = format!("p aaaa1111 # side\npick {C4} # c4\n");
        let dropped =
            rewrite_merges_todo(&todo, full, None, AcrossMergesAction::Drop).expect("drop");
        assert!(has_command(&dropped, "drop aaaa1111 # side"), "{dropped}");
        assert!(has_command(&dropped, &format!("pick {C4} # c4")));
    }

    #[test]
    fn squash_and_fixup_require_the_parent_on_the_same_chain() {
        let squashed = rewrite_merges_todo(&plan(), T2, Some(T1), AcrossMergesAction::Squash)
            .expect("t2 squashes into t1");
        assert!(has_command(&squashed, &format!("squash {T2} # t2")));
        assert!(has_command(&squashed, &format!("pick {T1} # t1")));

        let fixed = rewrite_merges_todo(&plan(), T2, Some(T1), AcrossMergesAction::Fixup)
            .expect("t2 fixes up into t1");
        assert!(has_command(&fixed, &format!("fixup {T2} # t2")));

        let merge_parent = rewrite_merges_todo(&plan(), C3, Some(M1), AcrossMergesAction::Squash)
            .expect_err("c3's parent is the merge");
        assert!(
            merge_parent
                .to_string()
                .contains("parent is a merge commit"),
            "{merge_parent}"
        );

        let other_chain = rewrite_merges_todo(&plan(), T1, Some(C2), AcrossMergesAction::Fixup)
            .expect_err("a label sits between c2 and t1");
        assert!(
            other_chain
                .to_string()
                .contains("not a commit on the same chain"),
            "{other_chain}"
        );

        let todo = format!("pick {C2} # c2\npick {C3} # c3\npick {C4} # c4\n");
        let skipped = rewrite_merges_todo(&todo, C4, Some(C2), AcrossMergesAction::Squash)
            .expect_err("c3 is between c4 and its claimed parent");
        assert!(skipped.to_string().contains("not its parent"), "{skipped}");

        let missing = rewrite_merges_todo(&plan(), T2, None, AcrossMergesAction::Squash)
            .expect_err("squash needs the parent");
        assert!(missing.to_string().contains("same chain"), "{missing}");
    }

    #[test]
    fn moves_swap_adjacent_picks_and_stop_at_a_boundary() {
        let up = rewrite_merges_todo(
            &plan(),
            T1,
            None,
            AcrossMergesAction::Move(MoveDirection::Up),
        )
        .expect("t1 swaps with t2");
        let cmds = commands(&up);
        let t1_at = cmds.iter().position(|line| line.contains(T1)).expect("t1");
        let t2_at = cmds.iter().position(|line| line.contains(T2)).expect("t2");
        assert!(t2_at < t1_at, "moving up applies t1 later: {cmds:?}");

        let down = rewrite_merges_todo(
            &plan(),
            T2,
            None,
            AcrossMergesAction::Move(MoveDirection::Down),
        )
        .expect("t2 swaps with t1");
        let cmds = commands(&down);
        let t1_at = cmds.iter().position(|line| line.contains(T1)).expect("t1");
        let t2_at = cmds.iter().position(|line| line.contains(T2)).expect("t2");
        assert!(t2_at < t1_at, "moving down applies t2 earlier: {cmds:?}");

        let newest = rewrite_merges_todo(
            &plan(),
            C4,
            None,
            AcrossMergesAction::Move(MoveDirection::Up),
        )
        .expect_err("c4 is the last pick");
        assert!(newest.to_string().contains("newest"), "{newest}");
        let merge_side = rewrite_merges_todo(
            &plan(),
            C4,
            None,
            AcrossMergesAction::Move(MoveDirection::Down),
        )
        .expect_err("M2 is the previous command");
        assert!(
            merge_side.to_string().contains("a merge sits next to it"),
            "{merge_side}"
        );

        let label = rewrite_merges_todo(
            &plan(),
            C2,
            None,
            AcrossMergesAction::Move(MoveDirection::Up),
        )
        .expect_err("the branch point label follows c2");
        assert!(label.to_string().contains("branch label"), "{label}");
        let reset = rewrite_merges_todo(
            &plan(),
            C2,
            None,
            AcrossMergesAction::Move(MoveDirection::Down),
        )
        .expect_err("reset onto precedes c2");
        assert!(reset.to_string().contains("branch reset"), "{reset}");

        let update = rewrite_merges_todo(
            &plan(),
            T2,
            None,
            AcrossMergesAction::Move(MoveDirection::Up),
        )
        .expect_err("update-ref follows t2");
        assert!(update.to_string().contains("branch update"), "{update}");

        let blocked = "\
pick 1111111111111111111111111111111111111111 # a
something-else
pick 2222222222222222222222222222222222222222 # b
";
        let stopped = rewrite_merges_todo(
            blocked,
            "1111111111111111111111111111111111111111",
            None,
            AcrossMergesAction::Move(MoveDirection::Up),
        )
        .expect_err("an unknown command is a boundary");
        assert!(stopped.to_string().contains("chain stops"), "{stopped}");
        let dropped = rewrite_merges_todo(
            blocked,
            "1111111111111111111111111111111111111111",
            None,
            AcrossMergesAction::Drop,
        )
        .expect("drop still keeps the unknown line");
        assert!(dropped.contains("something-else"), "{dropped}");
    }

    #[test]
    fn a_merge_commit_in_the_plan_is_refused() {
        let error = rewrite_merges_todo(&plan(), M1, None, AcrossMergesAction::Drop)
            .expect_err("M1 is a merge command, not a pick");
        assert!(error.to_string().contains("merge commit"), "{error}");
        let missing = rewrite_merges_todo(
            &plan(),
            "abcabcabcabcabcabcabcabcabcabcabcabcabca",
            None,
            AcrossMergesAction::Edit,
        )
        .expect_err("the id is not in the plan");
        assert!(
            missing.to_string().contains("not in the rebase plan"),
            "{missing}"
        );
    }

    /// Every commit of the fixture. A test reads the ones it rewrites; the rest
    /// stay so the history is named in one place.
    #[allow(dead_code)]
    struct Graph {
        repo: TestRepo,
        c1: Oid,
        c2: Oid,
        t1: Oid,
        t2: Oid,
        m1: Oid,
        c3: Oid,
        u1: Oid,
        m2: Oid,
        c4: Oid,
    }

    /// `c1-c2-(topic t1,t2)-M1-c3-(topic2 u1)-M2-c4`.
    ///
    /// `conflict_on_main` makes c2 and c3 both rewrite `shared.txt`, so dropping
    /// c2 makes the replay of c3 conflict.
    fn history(conflict_on_main: bool) -> Graph {
        let repo = TestRepo::new();
        if conflict_on_main {
            repo.write("shared.txt", "base\n");
        } else {
            repo.write("c1.txt", "c1\n");
        }
        let c1 = repo.commit("c1");
        if conflict_on_main {
            repo.write("shared.txt", "c2\n");
        } else {
            repo.write("c2.txt", "c2\n");
        }
        let c2 = repo.commit("c2");
        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.write("t1.txt", "t1\n");
        let t1 = repo.commit("t1");
        repo.write("t2.txt", "t2\n");
        let t2 = repo.commit("t2");
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["merge", "--no-ff", "-q", "-m", "M1", "topic"]);
        let m1 = repo.head();
        if conflict_on_main {
            repo.write("shared.txt", "c3\n");
        } else {
            repo.write("c3.txt", "c3\n");
        }
        let c3 = repo.commit("c3");
        repo.git(&["checkout", "-q", "-b", "topic2"]);
        repo.write("u1.txt", "u1\n");
        let u1 = repo.commit("u1");
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["merge", "--no-ff", "-q", "-m", "M2", "topic2"]);
        let m2 = repo.head();
        repo.write("c4.txt", "c4\n");
        let c4 = repo.commit("c4");
        Graph {
            repo,
            c1,
            c2,
            t1,
            t2,
            m1,
            c3,
            u1,
            m2,
            c4,
        }
    }

    fn subject(repo: &TestRepo, spec: &str) -> String {
        repo.git(&["log", "-1", "--format=%s", spec])
            .trim()
            .to_owned()
    }

    fn message(repo: &TestRepo, spec: &str) -> String {
        repo.git(&["log", "-1", "--format=%B", spec])
            .trim_end()
            .to_owned()
    }

    fn parent_count(repo: &TestRepo, commit: Oid) -> usize {
        repo.git(&["rev-list", "--parents", "-n", "1", &commit.to_string()])
            .split_whitespace()
            .count()
            - 1
    }

    fn files(repo: &TestRepo, spec: &str) -> Vec<String> {
        let mut names: Vec<String> = repo
            .git(&["ls-tree", "-r", "--name-only", spec])
            .lines()
            .map(str::to_owned)
            .collect();
        names.sort();
        names
    }

    fn branch(repo: &TestRepo) -> String {
        repo.git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_owned()
    }

    fn commit_with_subject(repo: &TestRepo, subject: &str) -> Oid {
        for line in repo.git(&["log", "--format=%H %s", "HEAD"]).lines() {
            let (id, message) = line.split_once(' ').expect("subject");
            if message == subject {
                return id.parse().expect("oid");
            }
        }
        panic!("no commit with subject {subject} reachable from HEAD");
    }

    fn has_subject(repo: &TestRepo, subject: &str) -> bool {
        repo.git(&["log", "--format=%s", "HEAD"])
            .lines()
            .any(|line| line == subject)
    }

    /// Subject, tree and parent count of every commit HEAD reaches.
    fn facts(repo: &TestRepo) -> BTreeMap<String, (String, usize)> {
        let mut facts = BTreeMap::new();
        for line in repo
            .git(&["log", "--format=%s%x1f%T%x1f%P", "HEAD"])
            .lines()
        {
            let mut parts = line.split('\u{1f}');
            let subject = parts.next().expect("subject").to_owned();
            let tree = parts.next().expect("tree").to_owned();
            let parents = parts.next().expect("parents");
            let count = if parents.is_empty() {
                0
            } else {
                parents.split_whitespace().count()
            };
            facts.insert(subject, (tree, count));
        }
        facts
    }

    #[test]
    fn drop_on_the_main_line_keeps_the_later_merges() {
        let graph = history(false);
        let git = graph.repo.git_handle();
        git.rewrite_across_merges(graph.c2, AcrossMergesAction::Drop)
            .expect("drop c2");

        assert_eq!(branch(&graph.repo), "main");
        assert_eq!(
            graph
                .repo
                .git(&["log", "--first-parent", "--format=%s"])
                .lines()
                .collect::<Vec<_>>(),
            ["c4", "M2", "c3", "M1", "c1"]
        );
        assert!(!has_subject(&graph.repo, "c2"));
        let m1 = commit_with_subject(&graph.repo, "M1");
        let m2 = commit_with_subject(&graph.repo, "M2");
        assert_ne!(m1, graph.m1, "the merge is recreated without c2");
        assert_eq!(parent_count(&graph.repo, m1), 2);
        assert_eq!(parent_count(&graph.repo, m2), 2);
        assert_eq!(message(&graph.repo, &m1.to_string()), "M1");
        assert_eq!(message(&graph.repo, &m2.to_string()), "M2");
        // c2 was the only commit between c1 and the merge, so the recreated
        // merge's first parent is still c1 and the side branch is unchanged.
        assert_eq!(graph.repo.rev(&format!("{m1}^1")), graph.c1);
        assert_eq!(subject(&graph.repo, &format!("{m1}^2")), "t2");
        assert_eq!(subject(&graph.repo, &format!("{m1}^2^")), "t1");
        assert_eq!(
            files(&graph.repo, "HEAD"),
            ["c1.txt", "c3.txt", "c4.txt", "t1.txt", "t2.txt", "u1.txt"]
        );
        assert_eq!(
            files(&graph.repo, &m1.to_string()),
            ["c1.txt", "t1.txt", "t2.txt"]
        );

        let rebases: Vec<_> = git
            .log()
            .into_iter()
            .filter(|record| {
                record.args.first().is_some_and(|arg| arg == "rebase") && !record.internal
            })
            .collect();
        assert_eq!(rebases.len(), 1, "reading the plan is an internal command");
        assert!(rebases[0].args.iter().any(|arg| arg == "--rebase-merges"));
        assert!(rebases[0].args.iter().any(|arg| arg == "--empty=stop"));
        assert!(
            !rebases[0].args.iter().any(|arg| arg == "--no-gpg-sign"),
            "commit.gpgsign is left to git: {:?}",
            rebases[0].args
        );
    }

    #[test]
    fn squash_refuses_a_merge_parent_and_melds_a_side_commit() {
        let graph = history(false);
        let head = graph.repo.head();
        let error = graph
            .repo
            .git_handle()
            .rewrite_across_merges(graph.c3, AcrossMergesAction::Squash)
            .expect_err("c3's parent is M1");
        assert!(error.to_string().contains("merge"), "{error}");
        assert_eq!(graph.repo.head(), head, "a refusal rewrites nothing");

        let graph = history(false);
        graph
            .repo
            .git_handle()
            .rewrite_across_merges(graph.t2, AcrossMergesAction::Squash)
            .expect("t2 squashes into t1");
        let m1 = commit_with_subject(&graph.repo, "M1");
        let m2 = commit_with_subject(&graph.repo, "M2");
        assert_eq!(parent_count(&graph.repo, m1), 2);
        assert_eq!(parent_count(&graph.repo, m2), 2);
        assert_eq!(message(&graph.repo, &format!("{m1}^2")), "t1\n\nt2");
        assert!(!has_subject(&graph.repo, "t2"));
        assert_eq!(
            files(&graph.repo, &format!("{m1}^2")),
            ["c1.txt", "c2.txt", "t1.txt", "t2.txt"]
        );
        assert_eq!(branch(&graph.repo), "main");
    }

    #[test]
    fn fixup_keeps_the_parent_message_across_the_merges() {
        let graph = history(false);
        graph
            .repo
            .git_handle()
            .rewrite_across_merges(graph.t2, AcrossMergesAction::Fixup)
            .expect("t2 fixes up into t1");
        let m1 = commit_with_subject(&graph.repo, "M1");
        let m2 = commit_with_subject(&graph.repo, "M2");
        assert_eq!(parent_count(&graph.repo, m1), 2);
        assert_eq!(parent_count(&graph.repo, m2), 2);
        assert_eq!(message(&graph.repo, &format!("{m1}^2")), "t1");
        assert_eq!(subject(&graph.repo, &format!("{m1}^2^")), "c2");
        assert!(!has_subject(&graph.repo, "t2"));
        assert_eq!(
            files(&graph.repo, "HEAD"),
            [
                "c1.txt", "c2.txt", "c3.txt", "c4.txt", "t1.txt", "t2.txt", "u1.txt"
            ]
        );
    }

    #[test]
    fn move_refuses_head_and_swaps_the_side_branch() {
        let graph = history(false);
        let head = graph.repo.head();
        let git = graph.repo.git_handle();
        let up = git
            .rewrite_across_merges(graph.c4, AcrossMergesAction::Move(MoveDirection::Up))
            .expect_err("c4 is already the newest");
        assert!(up.to_string().contains("newest"), "{up}");
        let down = git
            .rewrite_across_merges(graph.c4, AcrossMergesAction::Move(MoveDirection::Down))
            .expect_err("M2 sits below c4");
        assert!(down.to_string().contains("merge"), "{down}");
        assert_eq!(graph.repo.head(), head);

        let graph = history(false);
        graph
            .repo
            .git_handle()
            .rewrite_across_merges(graph.t1, AcrossMergesAction::Move(MoveDirection::Up))
            .expect("t1 swaps with t2");
        let m1 = commit_with_subject(&graph.repo, "M1");
        let m2 = commit_with_subject(&graph.repo, "M2");
        assert_eq!(parent_count(&graph.repo, m1), 2);
        assert_eq!(parent_count(&graph.repo, m2), 2);
        assert_eq!(subject(&graph.repo, &format!("{m1}^2")), "t1");
        assert_eq!(subject(&graph.repo, &format!("{m1}^2^")), "t2");
        assert_eq!(graph.repo.rev(&format!("{m1}^2^^")), graph.c2);
        assert_eq!(
            files(&graph.repo, &m1.to_string()),
            ["c1.txt", "c2.txt", "t1.txt", "t2.txt"]
        );
        assert_eq!(branch(&graph.repo), "main");
    }

    #[test]
    fn drop_a_merged_in_commit_keeps_both_merges() {
        let graph = history(false);
        graph
            .repo
            .git_handle()
            .rewrite_across_merges(graph.t1, AcrossMergesAction::Drop)
            .expect("drop t1");
        let m1 = commit_with_subject(&graph.repo, "M1");
        let m2 = commit_with_subject(&graph.repo, "M2");
        assert_eq!(parent_count(&graph.repo, m1), 2);
        assert_eq!(parent_count(&graph.repo, m2), 2);
        assert!(!has_subject(&graph.repo, "t1"));
        assert_eq!(subject(&graph.repo, &format!("{m1}^2")), "t2");
        assert_eq!(graph.repo.rev(&format!("{m1}^2^")), graph.c2);
        assert_eq!(message(&graph.repo, &m1.to_string()), "M1");
        assert_eq!(message(&graph.repo, &m2.to_string()), "M2");
        assert_eq!(
            files(&graph.repo, "HEAD"),
            ["c1.txt", "c2.txt", "c3.txt", "c4.txt", "t2.txt", "u1.txt"]
        );
        assert_eq!(branch(&graph.repo), "main");
    }

    #[test]
    fn edit_stops_and_continue_replays_the_same_trees() {
        let graph = history(false);
        let before = facts(&graph.repo);
        let git = graph.repo.git_handle();
        git.rewrite_across_merges(graph.c2, AcrossMergesAction::Edit)
            .expect("edit stops inside the rebase");
        assert_eq!(
            graph.repo.open().state().expect("state"),
            RepoState::Rebasing
        );
        assert_eq!(subject(&graph.repo, "HEAD"), "c2");

        git.continue_in_progress()
            .expect("continue finishes the rebase");
        assert_eq!(graph.repo.open().state().expect("state"), RepoState::Normal);
        assert_eq!(branch(&graph.repo), "main");
        assert_eq!(
            facts(&graph.repo),
            before,
            "replaying without an amend keeps every tree, message and parent count"
        );
        assert_eq!(
            parent_count(&graph.repo, commit_with_subject(&graph.repo, "M1")),
            2
        );
        assert_eq!(
            parent_count(&graph.repo, commit_with_subject(&graph.repo, "M2")),
            2
        );
    }

    #[test]
    fn a_conflicting_drop_leaves_a_rebase_that_abort_undoes() {
        let graph = history(true);
        let tip = graph.repo.head();
        let git = graph.repo.git_handle();
        git.rewrite_across_merges(graph.c2, AcrossMergesAction::Drop)
            .expect("a conflict is a resumable stop");
        assert_eq!(
            graph.repo.open().state().expect("state"),
            RepoState::Rebasing
        );
        assert!(
            graph
                .repo
                .open()
                .conflicts()
                .expect("conflicts")
                .iter()
                .any(|entry| entry.path == "shared.txt"),
            "c3 cannot apply once c2 is gone"
        );

        git.abort_in_progress().expect("abort");
        assert_eq!(graph.repo.open().state().expect("state"), RepoState::Normal);
        assert_eq!(graph.repo.head(), tip);
        assert_eq!(branch(&graph.repo), "main");
        assert_eq!(
            std::fs::read_to_string(graph.repo.path().join("shared.txt")).expect("worktree file"),
            "c3\n"
        );
        assert_eq!(subject(&graph.repo, "HEAD"), "c4");
    }

    #[test]
    fn refuses_a_detached_head() {
        let graph = history(false);
        let tip = graph.repo.head();
        graph.repo.git(&["checkout", "-q", "--detach"]);
        let error = graph
            .repo
            .git_handle()
            .rewrite_across_merges(graph.c2, AcrossMergesAction::Drop)
            .expect_err("detached HEAD is refused");
        assert!(error.to_string().contains("detached"), "{error}");
        graph.repo.git(&["checkout", "-q", "main"]);
        assert_eq!(graph.repo.head(), tip);
    }

    #[test]
    fn refuses_a_dirty_worktree() {
        let graph = history(false);
        graph.repo.write("c4.txt", "dirty\n");
        let error = graph
            .repo
            .git_handle()
            .rewrite_across_merges(graph.c4, AcrossMergesAction::Drop)
            .expect_err("a dirty worktree is refused");
        assert!(error.to_string().contains("uncommitted"), "{error}");
        assert_eq!(graph.repo.head(), graph.c4);
    }

    #[test]
    fn refuses_an_operation_in_progress() {
        let repo = TestRepo::new();
        repo.write("conflict.txt", "base\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.write("conflict.txt", "topic\n");
        let topic = repo.commit("topic");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("conflict.txt", "main\n");
        repo.commit("main");
        let merge = repo.git_output(&["merge", "topic"]);
        assert!(!merge.status.success(), "the merge conflicts");

        let error = repo
            .git_handle()
            .rewrite_across_merges(topic, AcrossMergesAction::Drop)
            .expect_err("a merge in progress is refused");
        assert!(error.to_string().contains("in progress"), "{error}");
        assert_eq!(repo.open().state().expect("state"), RepoState::Merging);
    }

    #[test]
    fn refuses_a_commit_head_cannot_reach_and_a_merge() {
        let graph = history(false);
        graph
            .repo
            .git(&["checkout", "-q", "-b", "elsewhere", &graph.c1.to_string()]);
        graph.repo.write("else.txt", "else\n");
        let side = graph.repo.commit("elsewhere");
        graph.repo.git(&["checkout", "-q", "main"]);
        let error = graph
            .repo
            .git_handle()
            .rewrite_across_merges(side, AcrossMergesAction::Drop)
            .expect_err("an unmerged commit is out of reach");
        assert!(error.to_string().contains("does not reach"), "{error}");
        assert_eq!(graph.repo.head(), graph.c4);

        let error = graph
            .repo
            .git_handle()
            .rewrite_across_merges(graph.m1, AcrossMergesAction::Edit)
            .expect_err("a merge commit is not a pick");
        assert!(error.to_string().contains("merge commit"), "{error}");
        assert_eq!(graph.repo.head(), graph.c4);
    }
}
