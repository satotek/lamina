//! The frame: [`RepoView`]'s `Render` implementation, plus the frame-stats
//! overlay and the error screen it falls back to.
//!
//! The pieces of the window live in the sibling modules; this one only lays
//! them out and turns actions into calls on the view. Nothing here calls git or
//! the session. Colours and sizes come from [`Design`], one palette resolved per
//! frame from the theme and the user's settings; the layout follows the design
//! mockups.

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Render,
    Styled, Window, div, px,
};
#[cfg(feature = "terminal")]
use gpui::{Axis, Bounds, Pixels, relative};
use gpui_component::ActiveTheme as _;

use super::navigation;
use super::{PAGE, RepoView};
#[cfg(feature = "terminal")]
use crate::actions::ToggleTerminal;
use crate::actions::{
    self, CloseCompare, ExtendSelectionNext, ExtendSelectionPrevious, GoToChild, GoToHead,
    GoToParent, GoToSecondParent, NavigateBack, NavigateForward, OpenInEditor, OpenTerminalHere,
    RedoLastAction, SelectFirst, SelectLast, SelectNext, SelectPageDown, SelectPageUp,
    SelectPrevious, StashChanges, SwitchBranch, ToggleDetailsMaximized, ToggleFrameStats,
    ToggleSidebar, ToggleWholeFile, UndoLastAction,
};
use crate::branches::CreateBranchFromHead;
use crate::commands::RunCustomCommand;
use crate::data::Source;
use crate::pull_requests::{CheckoutPullRequest, RefreshPullRequests};
use crate::remote::{Command, FetchRemotes, PullBranch, PushBranch};
use crate::search::{FocusSearch, SearchDismiss, SearchNext, SearchPrevious, SearchSubmit};
use crate::settings::Settings;
use crate::style::{self, Design};
use crate::worktree_panel::MainMode;

impl Render for RepoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let started = self.metrics.begin_frame();
        self.target_frame_ms = RepoView::target_frame_ms(window, cx);
        self.sync_title(window);
        // The first frame of a repository asks for its pull requests, waiting
        // for tool detection when that is still running.
        self.start_pull_request_refresh(cx);

        // Snapshot publishes arrive in chunks; only the rows after the last
        // observed one are scanned for the graph column's width.
        let observed = self.source();
        if let Some(Source::Repo(snapshot)) = &observed {
            self.lanes.observe(&snapshot.commits);
        }

        if self.overlay && self.metrics.log_due() {
            let stats = self.metrics.stats();
            eprintln!(
                "frame {:.2} ms ({:.1} fps, p95 {:.2} ms) · build {:.2} ms",
                stats.frame_ms, stats.fps, stats.p95_ms, stats.render_ms
            );
        }

        if self.smoke {
            self.advance_smoke(cx);
        }

        let entity = cx.entity();
        let design = Design::new(cx.theme(), Settings::get(cx));
        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(design.background)
            .text_color(design.text)
            .text_size(px(design.ui))
            .track_focus(&self.focus)
            .key_context(actions::REPO_VIEW)
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(
                cx.listener(|this, _: &ExtendSelectionPrevious, _, cx| {
                    this.extend_selection(-1, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ExtendSelectionNext, _, cx| this.extend_selection(1, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| this.move_selection(-PAGE, cx)))
            .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| this.move_selection(PAGE, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.select_last(cx)))
            .on_action(cx.listener(|this, _: &ToggleFrameStats, _, cx| this.toggle_frame_stats(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleDetailsMaximized, _, cx| {
                this.toggle_details_maximized(cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleWholeFile, _, cx| this.toggle_whole_file(cx)))
            .on_action(
                cx.listener(|this, _: &CloseCompare, window, cx| this.close_compare(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SearchDismiss, window, cx| this.dismiss(window, cx)))
            .on_action(cx.listener(|this, _: &SearchNext, _, cx| this.step_results(1, cx)))
            .on_action(cx.listener(|this, _: &SearchPrevious, _, cx| this.step_results(-1, cx)))
            .on_action(cx.listener(|this, _: &SearchSubmit, _, cx| this.submit_search(cx)))
            .on_action(
                cx.listener(|this, _: &StashChanges, window, cx| this.stash_changes(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &UndoLastAction, window, cx| {
                    this.undo_last_action(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &RedoLastAction, window, cx| {
                    this.redo_last_action(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &OpenTerminalHere, _, cx| this.open_terminal_here(cx)))
            .on_action(
                cx.listener(|this, _: &OpenInEditor, _, cx| this.open_selected_file_in_editor(cx)),
            )
            .on_action(cx.listener(|this, _: &CreateBranchFromHead, window, cx| {
                this.create_branch_from_head(window, cx)
            }))
            .on_action(cx.listener(|this, _: &SwitchBranch, window, cx| {
                this.open_branch_switcher(window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchRemotes, window, cx| {
                this.remote_command(Command::Fetch, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PullBranch, window, cx| {
                this.remote_command(Command::Pull, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PushBranch, window, cx| {
                this.remote_command(Command::Push, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RefreshPullRequests, _, cx| {
                this.refresh_pull_requests_command(cx)
            }))
            .on_action(cx.listener(|this, _: &CheckoutPullRequest, window, cx| {
                this.open_checkout_dialog(window, cx)
            }))
            .on_action(cx.listener(|this, action: &RunCustomCommand, window, cx| {
                this.run_custom_command(action.index, window, cx)
            }));
        #[cfg(feature = "terminal")]
        {
            root =
                root.on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                    this.toggle_terminal(window, cx)
                }));
        }
        root = root
            .on_action(cx.listener(|this, _: &NavigateBack, window, cx| {
                this.navigate_back(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NavigateForward, window, cx| {
                this.navigate_forward(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToParent, window, cx| {
                this.graph_move(navigation::GraphMove::Parent, window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToSecondParent, window, cx| {
                this.graph_move(navigation::GraphMove::SecondParent, window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToChild, window, cx| {
                this.graph_move(navigation::GraphMove::Child, window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToHead, window, cx| {
                this.graph_move(navigation::GraphMove::Head, window, cx);
            }));
        if let Some(font) = &design.ui_font {
            root = root.font_family(font.clone());
        }
        root.text_style().font_fallbacks = Some(style::font_fallbacks());
        root = root.child(self.header(&design, window));
        if let Some(workspace) = self.workspace.as_ref().and_then(|ws| ws.upgrade())
            && let Some(strip) = crate::workspace::Workspace::render_tab_strip(
                &workspace,
                self.repo_display_name(),
                self.has_operation_in_progress(),
                &design,
                cx,
            )
        {
            root = root.child(strip);
        }
        if let Some((path, error)) = &self.error {
            root = root.child(self.error_screen(path, error, &design));
        } else {
            // The body is laid out from the window width rather than from
            // measured bounds: the sidebar, the commit list's columns and the
            // title bar all decide what they show in the same frame.
            let viewport_width = window.viewport_size().width.as_f32();
            // The merge editor takes the whole repository view: the sidebar
            // steps aside with the working-tree lists so the three panes get
            // the width. Escape or the editor's close button restores it.
            let merge_full_window =
                self.mode == MainMode::Worktree && self.worktree.conflicts.is_full_window();
            let sidebar_width = if merge_full_window {
                0.0
            } else {
                super::sidebar::layout_width(viewport_width, self.sidebar_collapsed(cx))
            };
            let content_width = (viewport_width - sidebar_width).max(0.0);
            let sidebar = self.sidebar(cx, &design, sidebar_width);
            // The worktree view owns the main area while it is open; a file
            // history belongs to the history view, so it is dropped instead of
            // coming back stale.
            if self.mode != MainMode::History {
                self.discard_file_history();
                self.discard_compare();
            }
            let content = if self.mode == MainMode::History && self.file_history.is_some() {
                self.file_history_view(entity.clone(), cx)
            } else if self.mode == MainMode::History && self.compare.is_some() {
                self.compare_view(entity.clone(), cx)
            } else {
                match self.mode {
                    MainMode::History => {
                        self.history_split(entity.clone(), &design, content_width, cx)
                    }
                    MainMode::Worktree => self.worktree_view(entity.clone(), window, cx),
                    MainMode::Reflog => self.reflog_view(entity.clone(), &design, cx),
                }
            };
            // The banner spans the top of the main area, above whichever view
            // is showing, so a stopped operation is visible from the history
            // and not only from the working tree.
            let mut main = div().flex().flex_col().flex_1().min_w_0().min_h_0();
            if let Some(text) = self.lfs_notice.clone()
                && !self.lfs_notice_dismissed
            {
                let dismiss_entity = self.entity();
                main = main.child(
                    crate::ui::banner(crate::ui::BannerKind::Warning, text, &design).action(
                        crate::ui::button("dismiss-lfs-notice", t!("common.close").to_string())
                            .size(crate::ui::ButtonSize::Tiny)
                            .on_click(move |_, _, cx| {
                                dismiss_entity.update(cx, |this, cx| {
                                    this.lfs_notice_dismissed = true;
                                    cx.notify();
                                });
                            }),
                    ),
                );
            }
            if let Some(banner) = self.in_progress_banner(entity.clone(), &design) {
                main = main.child(banner);
            }
            if let Some(banner) = self.commit_graph_banner(entity.clone(), &design, cx) {
                main = main.child(banner);
            }
            let mut columns = div().flex().flex_1().min_h_0().min_w_0();
            if !merge_full_window {
                columns = columns.child(sidebar);
            }
            let main_area = columns.child(main.child(content));
            #[cfg(feature = "terminal")]
            let body = if self.terminal_open
                && let Some(terminal) = self.terminal.clone()
            {
                let top_fraction = self.terminal_top_fraction;
                let measured = self.terminal_split.get();
                let mut top_pane = div()
                    .relative()
                    .flex()
                    .flex_col()
                    .w_full()
                    .min_w_0()
                    .min_h_0()
                    .flex_none()
                    .child(main_area);
                top_pane = match measured {
                    Some(area) => top_pane.h(px(crate::ui::split_pixels(
                        top_fraction,
                        area.size(),
                        Self::MAIN_MIN_HEIGHT,
                        Self::TERMINAL_MIN_HEIGHT,
                    ))),
                    None => top_pane.flex_basis(relative(top_fraction)),
                };
                let drag_entity = entity.clone();
                let extent = self.terminal_split.clone();
                let reset_entity = entity.clone();
                top_pane = top_pane.child(crate::ui::split_handle(
                    "terminal-split-handle",
                    Axis::Vertical,
                    move |event, _window, cx| {
                        let Some(area) = extent.get() else {
                            return;
                        };
                        let fraction = area.fraction_at(event.event.position.y);
                        drag_entity.update(cx, |this, cx| {
                            this.set_terminal_top_fraction(fraction, area.size(), cx);
                        });
                    },
                    move |_event, _window, cx| {
                        reset_entity.update(cx, |this, cx| this.reset_terminal_top_fraction(cx));
                    },
                ));

                let measured_cell = self.terminal_split.clone();
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .on_children_prepainted(move |bounds: Vec<Bounds<Pixels>>, _, _| {
                        if let Some(area) = super::split::measure(&bounds, Axis::Vertical) {
                            measured_cell.set(Some(area));
                        }
                    })
                    .child(top_pane)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .child(terminal),
                    )
                    .into_any_element()
            } else {
                main_area.into_any_element()
            };
            #[cfg(not(feature = "terminal"))]
            let body = main_area;
            root = root.child(body).child(self.status_bar(&design));
        }

        if self.overlay {
            root = root.child(self.overlay_panel(&design));
        }

        self.metrics.end_frame(started);

        if self.overlay && !self.finished {
            window.request_animation_frame();
        }

        root
    }
}

impl RepoView {
    fn overlay_panel(&self, design: &Design) -> AnyElement {
        let stats = self.metrics.stats();
        let target = self.target_frame_ms;
        let target_hz = if target > 0.0 { 1000.0 / target } else { 60.0 };
        let commits = self.commit_count();

        let mut panel = div()
            .absolute()
            .bottom(px(10.0))
            .right(px(12.0))
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_md()
            .bg(design.panel)
            .border_1()
            .border_color(design.border)
            .text_size(px(11.0))
            .text_color(design.text)
            .child(
                div().font_weight(FontWeight::SEMIBOLD).child(
                    t!(
                        "chrome.frame_summary",
                        frame = format!("{:.2}", stats.frame_ms),
                        p95 = format!("{:.2}", stats.p95_ms),
                        fps = format!("{:.1}", stats.fps)
                    )
                    .to_string(),
                ),
            )
            .child(
                t!(
                    "chrome.frame_render",
                    render = format!("{:.2}", stats.render_ms),
                    samples = stats.samples,
                    frames = self.metrics.frames()
                )
                .to_string(),
            )
            .child(
                t!(
                    "chrome.frame_target",
                    target = format!("{target:.2}"),
                    hz = format!("{target_hz:.0}")
                )
                .to_string(),
            )
            .child(
                t!(
                    "chrome.frame_commits",
                    commits = commits,
                    selected = self
                        .selected
                        .map_or_else(|| t!("common.none").to_string(), |index| index.to_string())
                )
                .to_string(),
            );

        if stats.samples > 0 {
            panel = panel.child(div().text_color(design.muted).child(
                if stats.frame_ms <= target * 1.05 {
                    t!("chrome.smooth", hz = format!("{target_hz:.0}")).to_string()
                } else {
                    t!("chrome.below_target", hz = format!("{target_hz:.0}")).to_string()
                },
            ));
        }

        panel.into_any_element()
    }

    fn error_screen(&self, path: &str, error: &str, design: &Design) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .flex_1()
            .p_8()
            .child(
                div()
                    .text_size(px(18.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(design.remove_text)
                    .child(t!("chrome.could_not_open").to_string()),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(design.muted)
                    .child(path.to_owned()),
            )
            .child(
                div()
                    .max_w(px(760.0))
                    .text_size(px(12.0))
                    .child(error.to_owned()),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(design.muted)
                    .child(t!("chrome.usage").to_string()),
            )
            .into_any_element()
    }
}
