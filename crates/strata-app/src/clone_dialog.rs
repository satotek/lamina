//! The clone and init dialogs: the two ways to start a repository.
//!
//! Neither runs through a [`Session`](strata_core::session::Session), because
//! there is no repository yet. The dialogs call the standalone core functions
//! in [`strata_core::git_cli`], then hand the finished path to
//! [`Workspace`](crate::workspace::Workspace), which opens it (in the current
//! tab or a new one) and records it in the recent list.
//!
//! Cloning reports git's progress lines while it runs and can be cancelled.
//! Cancelling only removes the directory when this attempt created it; an
//! existing empty destination folder is left alone. The worker owns the
//! blocking `git` call and reports through a channel, so the UI never blocks.
//!
//! A private or password-protected remote asks for credentials through the
//! askpass bridge: the worker forwards the question over the same channel and
//! waits for the dialog's answer. A cancel there fails the clone with git's
//! authentication error, and the dialog's own Cancel stops the clone.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, Styled, Subscription, WeakEntity,
    Window, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::checkbox::Checkbox;
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::progress::Progress;
use strata_core::askpass::Answer;
use strata_core::git_cli::{CloneOptions, CloneOutcome, Git, clone_repository, init_repository};

use crate::picker::OpenTarget;
use crate::settings::Settings;
use crate::style::Design;
use crate::tools::Tooling;
use crate::ui;
use crate::workspace::Workspace;

/// How often the dialog drains what the clone worker reported.
const POLL: Duration = Duration::from_millis(50);

/// How often the worker checks whether the dialog it is waiting on was closed.
const PROMPT_POLL: Duration = Duration::from_millis(100);

/// Open the clone dialog. `target` is where a successful clone is opened,
/// carried over from the picker that launched the dialog.
pub(crate) fn open_clone(
    workspace: Entity<Workspace>,
    target: OpenTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog = cx.new(|cx| CloneDialog::new(workspace.downgrade(), target, window, cx));
    let focus = dialog.read(cx).url_focus(cx);
    let content = dialog.clone();
    let cancel = dialog.clone();
    let confirm = dialog.clone();
    let close = dialog;
    window.open_dialog(cx, move |card, _, cx| {
        let content = content.clone();
        let cancel = cancel.clone();
        let confirm = confirm.clone();
        let close = close.clone();
        let running = close.read(cx).is_running();
        card.title(t!("clone.title").to_string())
            .w(px(640.0))
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                DialogFooter::new()
                    .child(
                        ui::button("clone-cancel", t!("common.cancel").to_string())
                            .size(ui::ButtonSize::Medium)
                            .on_click(move |_, window, cx| {
                                cancel.update(cx, |dialog, cx| {
                                    dialog.cancel_or_close(window, cx);
                                });
                            }),
                    )
                    .child(
                        ui::button("clone-ok", t!("clone.clone").to_string())
                            .size(ui::ButtonSize::Medium)
                            .disabled(running)
                            .on_click(move |_, window, cx| {
                                confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                            }),
                    ),
            )
            .on_cancel(move |_, _, cx| {
                // While git runs, Escape and the close button ask the clone to
                // stop instead of dropping the only handle to its cancel flag.
                if close.read(cx).is_running() {
                    close.update(cx, |dialog, cx| dialog.request_cancel(cx));
                    false
                } else {
                    true
                }
            })
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

/// Open the init dialog, which creates a repository in a chosen folder.
pub(crate) fn open_init(
    workspace: Entity<Workspace>,
    target: OpenTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog = cx.new(|cx| InitDialog::new(workspace.downgrade(), target, window, cx));
    let focus = dialog.read(cx).folder_focus(cx);
    let content = dialog.clone();
    let confirm = dialog;
    window.open_dialog(cx, move |card, _, cx| {
        let content = content.clone();
        let confirm = confirm.clone();
        let guard = confirm.clone();
        let busy = confirm.read(cx).busy;
        card.title(t!("init.title").to_string())
            .w(px(560.0))
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                DialogFooter::new()
                    .child(
                        ui::button("init-cancel", t!("common.cancel").to_string())
                            .size(ui::ButtonSize::Medium)
                            .disabled(busy)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child({
                        let confirm = confirm.clone();
                        ui::button("init-ok", t!("init.create").to_string())
                            .size(ui::ButtonSize::Medium)
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                            })
                    }),
            )
            .on_ok(move |_, window, cx| {
                if confirm.read(cx).busy {
                    return false;
                }
                confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                false
            })
            .on_cancel(move |_, _, cx| !guard.read(cx).busy)
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

/// Open `path` in the workspace the dialog came from.
fn open_repository(
    workspace: &WeakEntity<Workspace>,
    target: OpenTarget,
    path: &Path,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), String> {
    let Some(workspace) = workspace.upgrade() else {
        return Err("The window that started this is gone.".to_owned());
    };
    workspace.update(cx, |workspace, cx| match target {
        OpenTarget::Current => workspace.open_path(path, window, cx),
        OpenTarget::NewTab => workspace.open_path_in_new_tab(path, window, cx),
    })
}

/// A [`Git`] whose working directory is the folder `target` lives in.
fn runner_for(target: &Path, binary: Option<PathBuf>) -> Git {
    let workdir = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let runner = Git::new(workdir);
    match binary {
        Some(binary) => runner.with_git_binary(binary),
        None => runner,
    }
}

/// git's own stderr when the command failed, so a credential helper's or a
/// server's explanation reaches the user unchanged.
fn error_text(error: strata_core::Error) -> String {
    match error {
        strata_core::Error::GitCommand { stderr, .. } if !stderr.trim().is_empty() => stderr,
        other => other.to_string(),
    }
}

/// A label above a text field. Forms are not a shared part yet, so the dialogs
/// that have one draw the same two-line shape here.
fn field(
    label: impl Into<gpui::SharedString>,
    input: &Entity<InputState>,
    design: &Design,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .child(
            div()
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .child(label.into()),
        )
        .child(Input::new(input).w_full().bg(design.field))
        .into_any_element()
}

fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// The folder name a clone URL suggests.
///
/// Handles `https://host/org/repo.git`, the scp-like `git@host:org/repo.git`
/// (`:` separates the host from the path, not a port), trailing slashes and a
/// `.git` suffix. `None` means the URL names no usable folder, and the dialog
/// then asks for a name instead of guessing.
pub(crate) fn directory_name(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    // A URL that is only a host (`https://example.com`) names no folder; the
    // host is not a repository name.
    if let Some((_, rest)) = trimmed.split_once("://")
        && !rest.contains('/')
    {
        return None;
    }
    let last = trimmed.rsplit(['/', ':']).next()?;
    let lower = last.to_ascii_lowercase();
    let name = match lower.strip_suffix(".git") {
        Some(_) => &last[..last.len() - 4],
        None => last,
    };
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name.to_owned())
}

/// The state of a running clone, for the bar and its phase text.
enum Phase {
    Idle,
    Clone { text: String, percent: Option<u8> },
    Cancelling,
}

/// What the clone worker reports through the channel.
enum Message {
    Progress {
        text: String,
        percent: Option<u8>,
    },
    Done(Result<CloneOutcome, String>),
    /// git is waiting for a credential. The answer goes back on `reply`; a
    /// dropped reply cancels, so a closed dialog never holds the clone.
    Askpass {
        prompt: String,
        secret: bool,
        reply: mpsc::Sender<Option<String>>,
    },
}

/// What the UI does after the worker reports.
enum Finished {
    Cloned(PathBuf),
    Cancelled,
    Failed,
}

struct CloneDialog {
    workspace: WeakEntity<Workspace>,
    /// Where a successful clone opens; inherited from the picker.
    target: OpenTarget,
    url: Entity<InputState>,
    /// The folder the new repository's directory is created in.
    parent: Entity<InputState>,
    name: Entity<InputState>,
    branch: Entity<InputState>,
    depth: Entity<InputState>,
    /// The last name derived from the URL, so typing a custom name sticks.
    derived_name: String,
    /// The last destination folder derived from the URL, so a user-picked
    /// folder is not overwritten on every keystroke.
    derived_parent: Option<String>,
    /// The exact destination of the running clone, frozen while fields stay
    /// editable.
    attempt: Option<PathBuf>,
    recurse_submodules: bool,
    options_open: bool,
    phase: Phase,
    /// Why the last attempt could not start, or git's failure text.
    error: Option<String>,
    /// Present while a clone runs; also the signal for Cancel to stop it.
    cancel: Option<Arc<AtomicBool>>,
    focus: gpui::FocusHandle,
    _url_events: Subscription,
}

impl CloneDialog {
    fn new(
        workspace: WeakEntity<Workspace>,
        target: OpenTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("clone.url_placeholder").to_string())
        });
        let parent = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("clone.destination_placeholder").to_string())
                .default_value(home_directory().to_string_lossy().into_owned())
        });
        let name = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("clone.folder_placeholder").to_string())
        });
        let branch = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("clone.branch_placeholder").to_string())
        });
        let depth = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("clone.depth_placeholder").to_string())
        });
        let url_events = cx.subscribe_in(
            &url,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.derive_name(window, cx);
                    this.error = None;
                    cx.notify();
                }
            },
        );
        Self {
            workspace,
            target,
            url,
            parent,
            name,
            branch,
            depth,
            derived_name: String::new(),
            derived_parent: None,
            attempt: None,
            recurse_submodules: false,
            options_open: false,
            phase: Phase::Idle,
            error: None,
            cancel: None,
            focus: cx.focus_handle(),
            _url_events: url_events,
        }
    }

    fn url_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.url.focus_handle(cx)
    }

    fn is_running(&self) -> bool {
        self.cancel.is_some()
    }

    /// Keeps the folder name and ghq destination in step with the URL until
    /// the user edits them.
    fn derive_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.url.read(cx).value().to_string();
        let Some(derived) = directory_name(&url) else {
            return;
        };
        let current = self.name.read(cx).value().to_string();
        if current.trim().is_empty() || current == self.derived_name {
            self.name
                .update(cx, |state, cx| state.set_value(derived.clone(), window, cx));
            self.derived_name = derived;
        }
        self.apply_ghq_destination(&url, window, cx);
    }

    /// A URL ghq understands defaults the destination to
    /// `<ghq root>/<host>/<owner>/<repo>`, as long as the user has not picked
    /// a folder themselves.
    fn apply_ghq_destination(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(destination) = self.ghq_destination(url, cx) else {
            return;
        };
        let Some(parent) = destination.parent() else {
            return;
        };
        let parent = parent.to_string_lossy().into_owned();
        let current_parent = self.parent.read(cx).value().to_string();
        if current_parent.trim().is_empty()
            || Some(&current_parent) == self.derived_parent.as_ref()
            || current_parent == home_directory().to_string_lossy().into_owned()
        {
            self.parent
                .update(cx, |state, cx| state.set_value(parent.clone(), window, cx));
            self.derived_parent = Some(parent);
        }

        if let Some(name) = destination
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        {
            let current = self.name.read(cx).value().to_string();
            if current.trim().is_empty() || current == self.derived_name {
                self.name
                    .update(cx, |state, cx| state.set_value(name.clone(), window, cx));
                self.derived_name = name;
            }
        }
    }

    fn ghq_destination(&self, url: &str, cx: &App) -> Option<PathBuf> {
        let settings = Settings::get(cx);
        if !settings.tools.ghq.enabled {
            return None;
        }
        let tooling = Tooling::get(cx)?;
        let ghq = tooling.ghq()?;
        let root = settings
            .tools
            .ghq
            .root
            .as_deref()
            .unwrap_or(ghq.root.as_path());
        strata_core::tools::ghq_destination(root, url)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        self.error = None;
        let url = self.url.read(cx).value().trim().to_owned();
        if url.is_empty() {
            self.error = Some(t!("clone.err.url").to_string());
            cx.notify();
            return;
        }
        let parent_text = self.parent.read(cx).value().trim().to_owned();
        if parent_text.is_empty() {
            self.error = Some(t!("clone.err.parent").to_string());
            cx.notify();
            return;
        }
        let name = match self.name.read(cx).value().trim().to_owned() {
            name if name.is_empty() => {
                let Some(derived) = directory_name(&url) else {
                    self.error = Some(t!("clone.err.name").to_string());
                    cx.notify();
                    return;
                };
                derived
            }
            name => name,
        };
        if name.contains(['/', '\\']) || name == "." || name == ".." {
            self.error = Some(t!("clone.err.name_shape").to_string());
            cx.notify();
            return;
        }
        let depth_text = self.depth.read(cx).value().trim().to_owned();
        let depth = if depth_text.is_empty() {
            None
        } else {
            match depth_text.parse::<u32>() {
                Ok(depth) if depth > 0 => Some(depth),
                _ => {
                    self.error = Some(t!("clone.err.depth").to_string());
                    cx.notify();
                    return;
                }
            }
        };
        let branch = {
            let branch = self.branch.read(cx).value().trim().to_owned();
            (!branch.is_empty()).then_some(branch)
        };
        let target = PathBuf::from(parent_text).join(name);
        let options = CloneOptions {
            branch,
            depth,
            recurse_submodules: self.recurse_submodules,
        };
        self.start(url, target, options, window, cx);
    }

    fn start(
        &mut self,
        url: String,
        target: PathBuf,
        options: CloneOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let binary = Settings::get(cx).behaviour.git_executable.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(Arc::clone(&cancel));
        self.attempt = Some(target.clone());
        self.phase = Phase::Clone {
            text: t!("clone.starting").to_string(),
            percent: None,
        };
        self.error = None;
        cx.notify();

        let (sender, receiver) = mpsc::channel();
        let worker_cancel = Arc::clone(&cancel);
        let prompt_cancel = Arc::clone(&cancel);
        let worker_target = target.clone();
        std::thread::spawn(move || {
            let progress_sender = sender.clone();
            // The dialog can be closed while git waits for a credential, so the
            // handler watches the same flag the clone itself does.
            let prompt_sender = sender.clone();
            let runner = runner_for(&worker_target, binary).with_askpass(move |prompt| {
                let (reply, answer) = mpsc::channel();
                let message = Message::Askpass {
                    prompt: prompt.text,
                    secret: prompt.secret,
                    reply,
                };
                if prompt_sender.send(message).is_err() {
                    return None;
                }
                loop {
                    if prompt_cancel.load(Ordering::SeqCst) {
                        return None;
                    }
                    match answer.recv_timeout(PROMPT_POLL) {
                        Ok(answer) => return answer.map(Answer::from_string),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                    }
                }
            });
            let result = clone_repository(
                &runner,
                &url,
                &worker_target,
                &options,
                &move || worker_cancel.load(Ordering::SeqCst),
                |text, percent| {
                    let _ = progress_sender.send(Message::Progress {
                        text: text.to_owned(),
                        percent,
                    });
                },
            )
            .map_err(error_text);
            let _ = sender.send(Message::Done(result));
        });

        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = loop {
                cx.background_executor().timer(POLL).await;
                let mut progress = Vec::new();
                let mut prompts = Vec::new();
                let mut done = None;
                let mut disconnected = false;
                loop {
                    match receiver.try_recv() {
                        Ok(Message::Progress { text, percent }) => {
                            progress.push((text, percent));
                        }
                        Ok(Message::Done(result)) => done = Some(result),
                        Ok(Message::Askpass {
                            prompt,
                            secret,
                            reply,
                        }) => prompts.push((prompt, secret, reply)),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                if !progress.is_empty()
                    && this
                        .update(cx, |dialog, cx| dialog.show_progress(&progress, cx))
                        .is_err()
                {
                    return;
                }
                for (prompt, secret, reply) in prompts {
                    // The dialog may have been closed between the worker asking
                    // and this frame; the clone is cancelled then, so no prompt
                    // is shown and the worker gets its cancel.
                    let running = this
                        .update(cx, |dialog, _| dialog.is_running())
                        .unwrap_or(false);
                    if !running {
                        let _ = reply.send(None);
                        continue;
                    }
                    let _ = handle.update(cx, |_, window, cx| {
                        crate::ui::askpass_prompt(
                            window,
                            cx,
                            "Authentication",
                            prompt,
                            secret,
                            move |answer, _window, _cx| {
                                let _ = reply.send(answer);
                            },
                        );
                    });
                }
                if let Some(result) = done {
                    break result;
                }
                if disconnected {
                    break Err(t!("clone.stopped").to_string());
                }
            };

            let Ok(finished) = this.update(cx, |dialog, cx| dialog.finish(result, cx)) else {
                return;
            };
            match finished {
                Finished::Cloned(path) => {
                    let _ = handle.update(cx, |_, window, cx| {
                        let _ = this.update(cx, |dialog, cx| {
                            if dialog.open_cloned(&path, window, cx).is_err() {
                                cx.notify();
                            }
                        });
                    });
                }
                Finished::Cancelled => {
                    let _ = handle.update(cx, |_, window, cx| window.close_dialog(cx));
                }
                Finished::Failed => {}
            }
        })
        .detach();
    }

    fn show_progress(&mut self, lines: &[(String, Option<u8>)], cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Clone { .. }) {
            return;
        }
        let mut text = None;
        let mut percent = None;
        for (line, value) in lines {
            text = Some(line.clone());
            if value.is_some() {
                percent = *value;
            }
        }
        let Phase::Clone {
            text: current,
            percent: current_percent,
        } = &mut self.phase
        else {
            return;
        };
        if let Some(text) = text {
            *current = text;
        }
        if percent.is_some() {
            *current_percent = percent;
        }
        cx.notify();
    }

    fn finish(&mut self, result: Result<CloneOutcome, String>, cx: &mut Context<Self>) -> Finished {
        self.cancel = None;
        match result {
            Ok(CloneOutcome::Cloned) => {
                let target = self.attempt.take().unwrap_or_default();
                self.phase = Phase::Idle;
                cx.notify();
                Finished::Cloned(target)
            }
            Ok(CloneOutcome::Cancelled) => {
                self.attempt = None;
                self.phase = Phase::Idle;
                cx.notify();
                Finished::Cancelled
            }
            Err(message) => {
                self.attempt = None;
                self.phase = Phase::Idle;
                self.error = Some(message);
                cx.notify();
                Finished::Failed
            }
        }
    }

    fn open_cloned(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let result = open_repository(&self.workspace, self.target, path, window, cx);
        if let Err(message) = &result {
            self.error = Some(message.clone());
        }
        result
    }

    fn request_cancel(&mut self, cx: &mut Context<Self>) {
        let Some(cancel) = &self.cancel else {
            return;
        };
        cancel.store(true, Ordering::SeqCst);
        self.phase = Phase::Cancelling;
        cx.notify();
    }

    fn cancel_or_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_running() {
            self.request_cancel(cx);
        } else {
            window.close_dialog(cx);
        }
    }

    fn choose_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("clone.choose_parent").to_string().into()),
        });
        let handle = window.window_handle();
        let this = cx.entity().downgrade();
        cx.spawn(async move |_this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                // Cancelled.
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |dialog, cx| {
                        dialog.error = Some(t!("clone.choose_failed", error = error).to_string());
                        cx.notify();
                    });
                    return;
                }
                Err(_) => return,
            };
            let Some(path) = path else {
                return;
            };
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |dialog, cx| {
                    dialog.parent.update(cx, |state, cx| {
                        state.set_value(path.to_string_lossy().into_owned(), window, cx)
                    });
                    dialog.error = None;
                    cx.notify();
                });
            });
        })
        .detach();
    }
}

impl Drop for CloneDialog {
    fn drop(&mut self) {
        // A dialog dismissed with the window leaves no one to press Cancel;
        // stopping here keeps the child from finishing into a removed folder.
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::SeqCst);
        }
    }
}

impl Render for CloneDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let options_entity = cx.entity();
        let mut form = div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .track_focus(&self.focus)
            .child(field(t!("clone.url_field").to_string(), &self.url, &design))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .child(
                        div()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child(t!("clone.destination").to_string()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Input::new(&self.parent).flex_1().bg(design.field))
                            .child(
                                ui::button("clone-choose-parent", t!("clone.choose").to_string())
                                    .kind(ui::ButtonKind::Normal)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_parent(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(field(
                t!("clone.folder_name").to_string(),
                &self.name,
                &design,
            ))
            .child(
                ui::section_header("clone-options", t!("clone.options").to_string(), &design)
                    .disclosure(self.options_open)
                    .px(px(0.0))
                    .on_click(move |_, cx| {
                        options_entity.update(cx, |this, cx| {
                            this.options_open = !this.options_open;
                            cx.notify();
                        });
                    }),
            )
            .when(self.options_open, |form| {
                form.child(field(t!("clone.branch").to_string(), &self.branch, &design))
                    .child(field(t!("clone.depth").to_string(), &self.depth, &design))
                    .child(
                        Checkbox::new("clone-submodules")
                            .checked(self.recurse_submodules)
                            .label(t!("clone.recurse_submodules").to_string())
                            .on_change(cx.listener(|this, checked, _, cx| {
                                this.recurse_submodules = *checked;
                                cx.notify();
                            })),
                    )
            });

        match &self.phase {
            Phase::Idle => {}
            Phase::Clone { text, percent } => {
                form = form.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .pt_1()
                        .child(
                            Progress::new("clone-progress")
                                .value(percent.map_or(0.0, f32::from))
                                .loading(percent.is_none())
                                .w_full(),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(design.tiny()))
                                .text_color(design.muted)
                                .child(text.clone()),
                        ),
                );
            }
            Phase::Cancelling => {
                form = form.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .pt_1()
                        .child(Progress::new("clone-progress").loading(true).w_full())
                        .child(
                            div()
                                .text_size(px(design.tiny()))
                                .text_color(design.muted)
                                .child(t!("clone.cancelling").to_string()),
                        ),
                );
            }
        }

        if let Some(error) = &self.error {
            form = form.child(ui::banner(ui::BannerKind::Danger, error.clone(), &design));
        }

        form
    }
}

struct InitDialog {
    workspace: WeakEntity<Workspace>,
    target: OpenTarget,
    folder: Entity<InputState>,
    branch: Entity<InputState>,
    busy: bool,
    error: Option<String>,
    focus: gpui::FocusHandle,
}

impl InitDialog {
    fn new(
        workspace: WeakEntity<Workspace>,
        target: OpenTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let suggested = home_directory().join("new-repository");
        let folder = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("init.folder_placeholder").to_string())
                .default_value(suggested.to_string_lossy().into_owned())
        });
        let branch = cx.new(|cx| InputState::new(window, cx).default_value("main"));
        Self {
            workspace,
            target,
            folder,
            branch,
            busy: false,
            error: None,
            focus: cx.focus_handle(),
        }
    }

    fn folder_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.folder.focus_handle(cx)
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("init.choose_folder").to_string().into()),
        });
        let handle = window.window_handle();
        let this = cx.entity().downgrade();
        cx.spawn(async move |_this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                // Cancelled.
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |dialog, cx| {
                        dialog.error = Some(t!("clone.choose_failed", error = error).to_string());
                        cx.notify();
                    });
                    return;
                }
                Err(_) => return,
            };
            let Some(path) = path else {
                return;
            };
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |dialog, cx| {
                    dialog.folder.update(cx, |state, cx| {
                        state.set_value(path.to_string_lossy().into_owned(), window, cx)
                    });
                    dialog.error = None;
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.error = None;
        let folder = self.folder.read(cx).value().trim().to_owned();
        if folder.is_empty() {
            self.error = Some("Choose the folder for the new repository.".to_owned());
            cx.notify();
            return;
        }
        let branch = {
            let branch = self.branch.read(cx).value().trim().to_owned();
            (!branch.is_empty()).then_some(branch)
        };
        let path = PathBuf::from(folder);
        let binary = Settings::get(cx).behaviour.git_executable.clone();
        self.busy = true;
        cx.notify();

        let worker_path = path.clone();
        let task = cx.background_executor().spawn(async move {
            let runner = runner_for(&worker_path, binary);
            init_repository(&runner, &worker_path, branch.as_deref()).map_err(error_text)
        });
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |dialog, cx| {
                    dialog.finish(result, &path, window, cx);
                });
            });
        })
        .detach();
    }

    fn finish(
        &mut self,
        result: Result<(), String>,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        match result {
            Ok(()) => {
                if let Err(message) =
                    open_repository(&self.workspace, self.target, path, window, cx)
                {
                    self.error = Some(message);
                }
            }
            Err(message) => self.error = Some(message),
        }
        cx.notify();
    }
}

impl Render for InitDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let mut form = div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .track_focus(&self.focus)
            .child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.muted)
                    .child(t!("init.description").to_string()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .child(
                        div()
                            .text_size(px(design.tiny()))
                            .text_color(design.muted)
                            .child("FOLDER"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Input::new(&self.folder).flex_1().bg(design.field))
                            .child(
                                ui::button("init-choose-folder", t!("init.choose").to_string())
                                    .kind(ui::ButtonKind::Normal)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_folder(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(field(
                t!("init.initial_branch").to_string(),
                &self.branch,
                &design,
            ));

        if let Some(error) = &self.error {
            form = form.child(ui::banner(ui::BannerKind::Danger, error.clone(), &design));
        }

        form
    }
}

#[cfg(test)]
mod tests {
    use super::directory_name;

    #[test]
    fn derives_a_folder_name_from_common_urls() {
        assert_eq!(
            directory_name("https://example.com/org/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(
            directory_name("https://example.com/org/repo").as_deref(),
            Some("repo")
        );
        assert_eq!(
            directory_name("https://example.com/org/repo.git/").as_deref(),
            Some("repo")
        );
        assert_eq!(
            directory_name("git@host.example:org/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(
            directory_name("ssh://git@host.example/org/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(
            directory_name("file:///tmp/sources/strata.git").as_deref(),
            Some("strata")
        );
    }

    #[test]
    fn derives_a_name_through_noise_around_the_path() {
        assert_eq!(
            directory_name("  https://example.com/org/repo.git  ").as_deref(),
            Some("repo")
        );
        assert_eq!(
            directory_name("https://example.com/org/repo.GIT").as_deref(),
            Some("repo")
        );
        assert_eq!(
            directory_name("https://example.com/org/repo///").as_deref(),
            Some("repo")
        );
    }

    #[test]
    fn urls_without_a_folder_name_have_none() {
        assert_eq!(directory_name(""), None);
        assert_eq!(directory_name("   "), None);
        assert_eq!(directory_name("https://example.com/"), None);
        assert_eq!(directory_name(".git"), None);
        assert_eq!(directory_name("https://example.com/org/../"), None);
    }
}
