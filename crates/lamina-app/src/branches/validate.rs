//! The names the branch and remote dialogs accept: git's own ref-name rules,
//! plus the few the CLI enforces by accident.

/// `git check-ref-format` rules that apply to `refs/heads/<name>`, plus the one
/// rule the CLI enforces by accident: a leading `-` would parse as an option.
pub(crate) fn validate_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(t!("branch.validation.name_required").to_string());
    }
    if name == "@" {
        return Err(t!("branch.validation.name_shorthand").to_string());
    }
    if name.starts_with('-') {
        return Err(t!("branch.validation.leading_dash").to_string());
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err(t!("branch.validation.slash_edges").to_string());
    }
    if name.contains("//") {
        return Err(t!("branch.validation.double_slash").to_string());
    }
    if name.contains("..") {
        return Err(t!("branch.validation.dot_dot").to_string());
    }
    if name.contains("@{") {
        return Err(t!("branch.validation.at_brace").to_string());
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(t!("branch.validation.control").to_string());
    }
    if name.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err(t!("branch.validation.specials").to_string());
    }
    if name.ends_with('.') {
        return Err(t!("branch.validation.trailing_dot").to_string());
    }
    if name.split('/').any(|part| {
        part.is_empty() || part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock")
    }) {
        return Err(t!("branch.validation.remote_part").to_string());
    }
    Ok(())
}

/// Names git accepts for a remote.
///
/// A remote's branches live under `refs/remotes/<name>/`, so git applies the
/// ref-name rules to the name; these are the same rules as for a branch, with
/// the message naming a remote instead. Unlike a branch, a remote name may
/// contain `/` (git stores the tracking refs below it), so parts are only
/// checked for the forbidden forms.
pub(crate) fn validate_remote_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(t!("branch.validation.remote_required").to_string());
    }
    if name.starts_with('-') {
        return Err(t!("branch.validation.remote_leading_dash").to_string());
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err(t!("branch.validation.remote_slash_edges").to_string());
    }
    if name.contains("//") {
        return Err(t!("branch.validation.remote_double_slash").to_string());
    }
    if name.contains("..") {
        return Err(t!("branch.validation.remote_dot_dot").to_string());
    }
    if name.contains("@{") {
        return Err(t!("branch.validation.remote_at_brace").to_string());
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(t!("branch.validation.remote_control").to_string());
    }
    if name.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err(t!("branch.validation.remote_specials").to_string());
    }
    if name.ends_with('.') {
        return Err(t!("branch.validation.remote_trailing_dot").to_string());
    }
    if name.split('/').any(|part| {
        part.is_empty() || part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock")
    }) {
        return Err(t!("branch.validation.remote_part").to_string());
    }
    Ok(())
}

/// The URL a remote command receives: non-empty, and not something git would
/// read as an option.
pub(crate) fn validate_remote_url(url: &str) -> Result<&str, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err(t!("branch.validation.url_required").to_string());
    }
    if url.starts_with('-') {
        return Err(t!("branch.validation.url_leading_dash").to_string());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_follow_git_check_ref_format() {
        for name in [
            "main",
            "feat/branch-sidebar",
            "fix-123",
            "v1.0.0",
            "a/b/c",
            "release.locked",
        ] {
            assert_eq!(validate_branch_name(name), Ok(()), "{name} should be valid");
        }

        for name in [
            "",
            "@",
            "-foo",
            "/foo",
            "foo/",
            "foo bar",
            "foo\tbar",
            "foo..bar",
            "foo/../bar",
            "foo//bar",
            "foo@{1}",
            "foo~1",
            "foo^",
            "foo:bar",
            "foo?",
            "foo*",
            "foo[bar]",
            "foo\\bar",
            "foo.",
            "foo./bar",
            "foo/.bar",
            ".foo",
            "foo.lock",
            "foo.lock/bar",
        ] {
            assert!(
                validate_branch_name(name).is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn remote_names_follow_git_s_ref_rules() {
        for name in ["origin", "upstream", "team/one", "backup-2", "a.b"] {
            assert_eq!(validate_remote_name(name), Ok(()), "{name} should be valid");
        }

        for name in [
            "",
            "  ",
            "-origin",
            "/origin",
            "origin/",
            "ori//gin",
            "ori..gin",
            "ori@{1}gin",
            "ori gin",
            "ori\ngin",
            "ori~gin",
            "ori:gin",
            "ori?gin",
            "ori*gin",
            "ori[gin",
            "ori\\gin",
            "origin.",
            ".origin",
            "origin.lock",
            "origin.lock/main",
        ] {
            assert!(
                validate_remote_name(name).is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn a_remote_url_must_be_present_and_not_an_option() {
        assert_eq!(
            validate_remote_url("  https://example.invalid/repo.git "),
            Ok("https://example.invalid/repo.git")
        );
        assert_eq!(validate_remote_url("/tmp/repo.git"), Ok("/tmp/repo.git"));
        assert!(validate_remote_url("   ").is_err());
        assert!(validate_remote_url("--upload-pack=evil").is_err());
    }
}
