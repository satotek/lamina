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
use gpui_component::ActiveTheme as _;

use super::{PAGE, RepoView};
use crate::actions::{
    self, OpenInEditor, OpenTerminalHere, SelectFirst, SelectLast, SelectNext, SelectPageDown,
    SelectPageUp, SelectPrevious, StashChanges, ToggleFrameStats,
};
use crate::branches::CreateBranchFromHead;
use crate::data::Source;
use crate::remote::{Command, FetchRemotes, PullBranch, PushBranch};
use crate::search::{FocusSearch, SearchDismiss, SearchNext, SearchPrevious, SearchSubmit};
use crate::settings::Settings;
use crate::style::Design;
use crate::worktree_panel::MainMode;

impl Render for RepoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let started = self.metrics.begin_frame();
        self.target_frame_ms = RepoView::target_frame_ms(window, cx);
        self.sync_title(window);

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
            .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| this.move_selection(-PAGE, cx)))
            .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| this.move_selection(PAGE, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.select_last(cx)))
            .on_action(cx.listener(|this, _: &ToggleFrameStats, _, cx| this.toggle_frame_stats(cx)))
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
            .on_action(cx.listener(|this, _: &OpenTerminalHere, _, cx| this.open_terminal_here(cx)))
            .on_action(
                cx.listener(|this, _: &OpenInEditor, _, cx| this.open_selection_in_editor(cx)),
            )
            .on_action(cx.listener(|this, _: &CreateBranchFromHead, window, cx| {
                this.create_branch_from_head(window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchRemotes, window, cx| {
                this.remote_command(Command::Fetch, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PullBranch, window, cx| {
                this.remote_command(Command::Pull, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PushBranch, window, cx| {
                this.remote_command(Command::Push, window, cx)
            }));
        if let Some(font) = &design.ui_font {
            root = root.font_family(font.clone());
        }

        root = root.child(self.header(&design));
        if let Some((path, error)) = &self.error {
            root = root.child(self.error_screen(path, error, &design));
        } else {
            let sidebar = self.sidebar(cx, &design);
            // The worktree view owns the main area while it is open; a file
            // history belongs to the history view, so it is dropped instead of
            // coming back stale.
            if self.mode != MainMode::History {
                self.discard_file_history();
            }
            let content = if self.mode == MainMode::History && self.file_history.is_some() {
                self.file_history_view(entity.clone(), cx)
            } else {
                match self.mode {
                    MainMode::History => div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(self.commit_pane(entity.clone(), &design))
                        .child(self.diff_pane(&design, cx))
                        .into_any_element(),
                    MainMode::Worktree => self.worktree_view(entity.clone(), window, cx),
                }
            };
            // The banner spans the top of the main area, above whichever view
            // is showing, so a stopped operation is visible from the history
            // and not only from the working tree.
            let mut main = div().flex().flex_col().flex_1().min_w_0().min_h_0();
            if let Some(banner) = self.in_progress_banner(entity.clone(), &design) {
                main = main.child(banner);
            }
            root = root
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(sidebar)
                        .child(main.child(content)),
                )
                .child(self.status_bar(&design));
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
            .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                "{:.2} ms/frame mean · p95 {:.2} ms · {:.1} fps",
                stats.frame_ms, stats.p95_ms, stats.fps
            )))
            .child(format!(
                "render {:.2} ms · {} samples · {} frames",
                stats.render_ms,
                stats.samples,
                self.metrics.frames()
            ))
            .child(format!(
                "display target {:.2} ms ({:.0} Hz)",
                target, target_hz
            ))
            .child(format!(
                "{commits} commits · selected {}",
                self.selected
                    .map_or_else(|| "none".to_owned(), |index| index.to_string())
            ));

        if stats.samples > 0 {
            panel = panel.child(div().text_color(design.muted).child(
                if stats.frame_ms <= target * 1.05 {
                    format!("smooth ({target_hz:.0} fps target)")
                } else {
                    format!("below {target_hz:.0} fps target")
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
                    .child("Could not open repository"),
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
                    .child("usage: strata [PATH] · strata --fake [N]"),
            )
            .into_any_element()
    }
}
