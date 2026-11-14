//! LFS parts: the row badge and the diff card that replaces pointer text.
//!
//! The card carries the sizes and shortened object ids; the download button is
//! passed in by the pane that knows which path it belongs to.

use gpui::{
    AnyElement, App, FontWeight, IntoElement, ParentElement as _, RenderOnce, SharedString,
    StyleRefinement, Styled, Window, div, px,
};
use gpui_component::StyledExt as _;
use strata_core::lfs::{LfsDiffSide, LfsDiffSummary, format_size, short_label};

use crate::style::Design;
use crate::ui::{BadgeTone, status_badge};

/// The chip a file row uses to say the blob in history is a pointer.
pub fn lfs_badge(design: &Design) -> AnyElement {
    status_badge("LFS", BadgeTone::Accent, design)
}

/// A summary of an LFS diff: both sides' sizes and object ids, and which
/// objects are missing locally. [`LfsCard::action`] adds the Download button.
pub fn lfs_card(summary: LfsDiffSummary, design: &Design) -> LfsCard {
    LfsCard {
        summary,
        design: design.clone(),
        actions: Vec::new(),
        style: StyleRefinement::default(),
    }
}

#[derive(IntoElement)]
pub struct LfsCard {
    summary: LfsDiffSummary,
    design: Design,
    actions: Vec<AnyElement>,
    style: StyleRefinement,
}

impl LfsCard {
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }
}

impl Styled for LfsCard {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for LfsCard {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let design = &self.design;
        let mut actions = div().flex().items_center().gap_2().flex_none();
        for action in self.actions {
            actions = actions.child(action);
        }

        let mut card = div()
            .flex()
            .flex_col()
            .gap_2()
            .m_3()
            .p_3()
            .rounded(px(6.0))
            .border_1()
            .border_color(design.border)
            .bg(design.field)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(lfs_badge(design))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.summary.text.clone()),
                    )
                    .child(actions),
            );

        if let Some(old) = &self.summary.old {
            card = card.child(side_row("old", old, design));
        }
        if let Some(new) = &self.summary.new {
            card = card.child(side_row("new", new, design));
        }
        card.refine_style(&self.style)
    }
}

fn side_row(label: &str, side: &LfsDiffSide, design: &Design) -> AnyElement {
    let oid = (!side.oid.is_empty()).then(|| short_label(&side.oid));
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(design.small()))
        .child(
            div()
                .w(px(28.0))
                .flex_none()
                .text_color(design.muted)
                .child(SharedString::from(label.to_owned())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(design.mono.clone())
                .text_size(px(design.code))
                .child(oid.unwrap_or_else(|| t!("lfs.worktree_file").to_string().to_owned())),
        )
        .child(
            div()
                .flex_none()
                .text_color(design.muted)
                .child(format_size(side.size)),
        );
    row = if side.present {
        row.child(status_badge(
            t!("lfs.local").to_string(),
            BadgeTone::Neutral,
            design,
        ))
    } else {
        row.child(status_badge(
            t!("lfs.not_downloaded").to_string(),
            BadgeTone::Danger,
            design,
        ))
    };
    row.into_any_element()
}

/// A short hint used by the install notice.
pub fn install_hint() -> String {
    let install = if cfg!(target_os = "macos") {
        "brew install git-lfs"
    } else if cfg!(target_os = "windows") {
        "winget install GitHub.GitLFS"
    } else {
        "your package manager"
    };
    format!("Install Git LFS to work with this repository's LFS files (for example `{install}`).")
}
