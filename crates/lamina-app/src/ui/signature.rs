//! Commit signature status badges (for the commit list) and verification
//! summary blocks (for the commit details pane).

use std::sync::Arc;

use gpui::{
    AnyElement, App, ElementId, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px,
};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, Sizable as _};
use gpui_kit_assets::IconName;
use lamina_core::repo::{CommitSignature, SignatureFormat, SignatureValidity};

use crate::style::Design;

/// Current signature state known for a commit in the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitSignatureState {
    /// `gix` detected a signature header; `git log` verification is in flight.
    Detected(SignatureFormat),
    /// Verification completed by `git log`.
    Verified(Arc<CommitSignature>),
}

/// Compact badge rendered next to signed commits in the commit list.
///
/// Returns `None` when the state represents an unsigned commit.
pub fn signature_badge(
    id: impl Into<ElementId>,
    state: &CommitSignatureState,
    design: &Design,
) -> Option<AnyElement> {
    let (icon_name, background, foreground, tooltip) = badge_visual(state, design)?;
    Some(
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(design.px(16.0))
            .px(design.px(4.0))
            .rounded(design.px(4.0))
            .bg(background)
            .child(
                Icon::new(icon_name)
                    .with_size(design.px(11.0))
                    .text_color(foreground),
            )
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .into_any_element(),
    )
}

/// Commit details section showing `"Signed by <signer> (<key>)"`, verification
/// status, missing-config hint for `E`, and git's verification message on
/// hover or expand.
pub fn signature_details(
    state: &CommitSignatureState,
    expanded: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    design: &Design,
) -> Option<AnyElement> {
    let (icon_name, background, foreground, _) = badge_visual(state, design)?;

    let (status_label, format_label, summary_line, missing_hint, raw_message) = match state {
        CommitSignatureState::Detected(format) => (
            t!("signature.checking").to_string(),
            format.label(),
            t!("signature.detected", format = format.label()).to_string(),
            None,
            None,
        ),
        CommitSignatureState::Verified(sig) => {
            if sig.validity == SignatureValidity::None {
                return None;
            }
            let summary_line = signed_by_line(sig);
            let raw_message = (!sig.message.trim().is_empty()).then(|| sig.message.clone());
            (
                validity_label(sig.validity),
                sig.format.label(),
                summary_line,
                sig.missing_hint.clone(),
                raw_message,
            )
        }
    };

    let has_message = raw_message.is_some();
    let mut header = div()
        .id("commit-signature-details-toggle")
        .flex()
        .items_center()
        .gap(design.px(6.0))
        .min_w_0()
        .rounded(design.px(5.0))
        .px(design.px(6.0))
        .py(design.px(4.0))
        .bg(design.field)
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(design.px(4.0))
                .px(design.px(6.0))
                .h(design.px(18.0))
                .rounded_full()
                .bg(background)
                .text_color(foreground)
                .text_size(px(design.tiny()))
                .font_weight(FontWeight::SEMIBOLD)
                .child(
                    Icon::new(icon_name)
                        .with_size(design.px(11.0))
                        .text_color(foreground),
                )
                .child(format!("{status_label} ({format_label})")),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(design.px(11.5))
                .text_color(design.text)
                .child(summary_line),
        );

    if let Some(message) = raw_message.clone() {
        let chevron = if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };
        header = header
            .cursor_pointer()
            .hover(|style| style.bg(design.selected))
            .child(
                Icon::new(chevron)
                    .with_size(design.px(12.0))
                    .text_color(design.muted),
            )
            .tooltip(move |window, cx| Tooltip::new(message.clone()).build(window, cx))
            .on_click(move |_event, window, cx| on_toggle(window, cx));
    }

    let mut container = div().flex().flex_col().gap(design.px(4.0)).child(header);

    if let Some(hint) = missing_hint {
        container = container.child(
            div()
                .px(design.px(6.0))
                .text_size(design.px(11.0))
                .text_color(design.muted)
                .child(hint),
        );
    }

    if expanded
        && has_message
        && let Some(message) = raw_message
    {
        container = container.child(
            div()
                .px(design.px(8.0))
                .py(design.px(6.0))
                .rounded(design.px(5.0))
                .bg(design.field)
                .border_1()
                .border_color(design.border)
                .font_family(design.mono.clone())
                .text_size(px(design.tiny()))
                .text_color(design.muted)
                .line_height(gpui::relative(1.35))
                .child(message),
        );
    }

    Some(container.into_any_element())
}

/// Formats `"Signed by <signer> (<key>)"` with graceful fallbacks when git
/// did not report a signer name or key id.
pub fn signed_by_line(sig: &CommitSignature) -> String {
    match (&sig.signer, &sig.key) {
        (Some(signer), Some(key)) => {
            t!("signature.signed_by_key", signer = signer, key = key).to_string()
        }
        (Some(signer), None) => t!("signature.signed_by", signer = signer).to_string(),
        (None, Some(key)) => t!("signature.signed_with_key", key = key).to_string(),
        (None, None) => t!("signature.format", format = sig.format.label()).to_string(),
    }
}

/// The verification outcome in the interface language. The core crate names
/// the states; the app owns the wording.
fn validity_label(validity: SignatureValidity) -> String {
    use SignatureValidity as Validity;

    match validity {
        Validity::Good => t!("signature.validity.good"),
        Validity::Bad => t!("signature.validity.bad"),
        Validity::UnknownValidity => t!("signature.validity.unknown"),
        Validity::Expired => t!("signature.validity.expired"),
        Validity::ExpiredKey => t!("signature.validity.expired_key"),
        Validity::RevokedKey => t!("signature.validity.revoked_key"),
        Validity::CannotCheck => t!("signature.validity.cannot_check"),
        Validity::None => t!("signature.validity.none"),
    }
    .to_string()
}

fn badge_visual(
    state: &CommitSignatureState,
    design: &Design,
) -> Option<(IconName, Hsla, Hsla, SharedString)> {
    match state {
        CommitSignatureState::Detected(format) => Some((
            IconName::Shield,
            design.field,
            design.muted,
            SharedString::from(
                t!("signature.checking_format", format = format.label()).to_string(),
            ),
        )),
        CommitSignatureState::Verified(sig) => {
            let fmt = sig.format.label();
            let who = signed_by_line(sig);
            let visual = match sig.validity {
                SignatureValidity::Good => (
                    IconName::ShieldCheck,
                    design.add_background,
                    design.add_text,
                    t!("signature.verified", format = fmt, who = who).to_string(),
                ),
                SignatureValidity::UnknownValidity
                | SignatureValidity::Expired
                | SignatureValidity::ExpiredKey
                | SignatureValidity::RevokedKey => (
                    IconName::ShieldAlert,
                    design.tag_background,
                    design.tag_text,
                    t!(
                        "signature.validity_line",
                        validity = validity_label(sig.validity),
                        format = fmt,
                        who = who
                    )
                    .to_string(),
                ),
                SignatureValidity::Bad => (
                    IconName::ShieldX,
                    design.remove_background,
                    design.remove_text,
                    t!("signature.bad", format = fmt, who = who).to_string(),
                ),
                SignatureValidity::CannotCheck => {
                    let reason = sig
                        .missing_hint
                        .clone()
                        .unwrap_or_else(|| t!("signature.unable").to_string());
                    (
                        IconName::ShieldQuestionMark,
                        design.field,
                        design.muted,
                        t!("signature.cannot_verify", format = fmt, reason = reason).to_string(),
                    )
                }
                SignatureValidity::None => return None,
            };
            Some((visual.0, visual.1, visual.2, SharedString::from(visual.3)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(format: SignatureFormat, signer: Option<&str>, key: Option<&str>) -> CommitSignature {
        CommitSignature {
            format,
            validity: SignatureValidity::Good,
            signer: signer.map(str::to_owned),
            key: key.map(str::to_owned),
            fingerprint: None,
            primary_key_fingerprint: None,
            trust_level: None,
            message: String::new(),
            missing_hint: None,
        }
    }

    #[test]
    fn signed_by_line_formats_signer_and_key_with_fallbacks() {
        assert_eq!(
            signed_by_line(&sig(
                SignatureFormat::Ssh,
                Some("Alice <alice@example.com>"),
                Some("SHA256:abc")
            )),
            "Signed by Alice <alice@example.com> (SHA256:abc)"
        );
        assert_eq!(
            signed_by_line(&sig(SignatureFormat::OpenPgp, Some("Alice"), None)),
            "Signed by Alice"
        );
        assert_eq!(
            signed_by_line(&sig(SignatureFormat::X509, None, Some("DEADBEAF"))),
            "Signed with key DEADBEAF"
        );
        assert_eq!(
            signed_by_line(&sig(SignatureFormat::OpenPgp, None, None)),
            "OpenPGP signature"
        );
    }
}
