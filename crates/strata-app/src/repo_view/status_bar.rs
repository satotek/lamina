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
            .unwrap_or_else(|| "no git commands yet".to_owned());
        let error = self.status_error.clone();
        let operation = self.remote_status(&remote::Colors {
            foreground: design.text,
            muted_foreground: design.muted,
            border: design.border,
            accent: design.accent,
            accent_foreground: design.on_accent,
        });

        let mut bar = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(26.0))
            .flex_none()
            .px_3()
            .gap(px(10.0))
            .bg(design.chrome)
            .border_t_1()
            .border_color(design.border)
            .text_size(px(design.small()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex_none()
                            .w(px(7.0))
                            .h(px(7.0))
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
                .max_w(px(720.0))
                .truncate()
                .text_color(if error.is_some() {
                    design.remove_text
                } else {
                    design.muted
                })
                .child(error.unwrap_or_else(|| "Watching for changes".to_owned())),
        )
        .into_any_element()
    }
}
