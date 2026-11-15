//! The status bar: the last git command, a running remote operation and the
//! last error.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};

use super::RepoView;
use crate::remote;
use crate::style::Design;

impl RepoView {
    pub(super) fn status_bar(&self, design: &Design) -> AnyElement {
        let command = self
            .last_command
            .clone()
            .unwrap_or_else(|| t!("status.no_commands").to_string());
        // The status line is one row; a command with hundreds of path
        // arguments would otherwise build a string many screens wide. The
        // truncation counts display columns, so a Japanese path is not cut
        // into an invalid boundary.
        let command = strata_core::text::truncate_width(&command, 160);
        let error = self.status_error.clone();
        let note = self.status_note.clone();
        let operation = self.remote_status(&remote::Colors {
            foreground: design.text,
            muted_foreground: design.muted,
            border: design.border,
            accent: design.accent,
            accent_foreground: design.on_accent,
            zoom: design.zoom,
        });

        let mut bar = div()
            .flex()
            .items_center()
            .justify_between()
            .h(design.px(26.0))
            .flex_none()
            .px_3()
            .gap(design.px(10.0))
            .bg(design.chrome)
            .border_t_1()
            .border_color(design.border)
            .text_size(px(design.small()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(design.px(10.0))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex_none()
                            .w(design.px(7.0))
                            .h(design.px(7.0))
                            .rounded_full()
                            .bg(design.add_text),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(design.mono.clone())
                            .text_color(design.muted)
                            .child(command),
                    ),
            );
        // Only while a fetch, pull or push runs: the bar otherwise keeps its
        // command log and last-error slots.
        if let Some(operation) = operation {
            bar = bar.child(operation);
        }
        bar.child(
            div()
                .max_w(design.px(720.0))
                .truncate()
                .text_color(if error.is_some() {
                    design.remove_text
                } else {
                    design.muted
                })
                .child(
                    error
                        .or(note)
                        .unwrap_or_else(|| t!("status.watching").to_string()),
                ),
        )
        .into_any_element()
    }
}
