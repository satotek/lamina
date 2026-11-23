//! On-demand commit signature verification via `git log`.
//!
//! Running `git` ensures the user's `gpg.format`, `gpg.program`,
//! `gpg.x509.program`, `gpg.ssh.allowedSignersFile`, and keyring/trust
//! configuration apply verbatim without re-implementing cryptographic trust in
//! core.

use std::collections::HashMap;

use crate::repo::{CommitSignature, SignatureConfigHints, SignatureFormat, SignatureValidity};
use crate::{Error, Oid, Result};

use super::{Git, command_error, to_args};

const LOG_FORMAT: &str = "%H%x1f%G?%x1f%GS%x1f%GK%x1f%GF%x1f%GP%x1f%GT%x1f%GG";

/// Parsed row from `git log --format=%H%x1f%G?... -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSignatureRow {
    pub id: Oid,
    pub validity: SignatureValidity,
    pub signer: Option<String>,
    pub key: Option<String>,
    pub fingerprint: Option<String>,
    pub primary_key_fingerprint: Option<String>,
    pub trust_level: Option<String>,
    pub message: String,
}

impl Git {
    /// Reads the settings that shape the "cannot check" hint, through `git`
    /// so they match what `verify_commit_signatures` sees: the same
    /// environment, includes, and any change made since the repository was
    /// opened. `--path` expands `~` the way git does.
    pub fn signature_config_hints(&self) -> Result<SignatureConfigHints> {
        let args = to_args(&["config", "--path", "--get", "gpg.ssh.allowedSignersFile"]);
        let output = self.run_output(&args, None, false)?;
        // Exit 1 means the key is unset; anything else is a real failure.
        if output.status.code() == Some(1) {
            return Ok(SignatureConfigHints::default());
        }
        if !output.status.success() {
            return Err(command_error(&args, &output));
        }
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if raw.is_empty() {
            return Ok(SignatureConfigHints::default());
        }
        let resolved = std::path::PathBuf::from(raw);
        Ok(SignatureConfigHints {
            ssh_allowed_signers_configured: true,
            ssh_allowed_signers_exists: resolved.is_file(),
            ssh_allowed_signers_path: Some(resolved),
        })
    }

    /// Verifies a batch of already-detected signed commits using `git log`.
    ///
    /// Only signed commits (from `Repo::commit_signature_formats`) should be
    /// passed here so unsigned commits never spawn `git`, and so a `%G? = N`
    /// emitted when a verifier (e.g. `gpg` or `gpg.ssh.allowedSignersFile`) is
    /// missing is accurately reported as [`SignatureValidity::CannotCheck`] (`E`)
    /// rather than unsigned.
    pub fn verify_commit_signatures(
        &self,
        commits: &[(Oid, SignatureFormat)],
        hints: &SignatureConfigHints,
        cancelled: &(impl Fn() -> bool + Send + Sync),
    ) -> Result<Option<HashMap<Oid, CommitSignature>>> {
        if commits.is_empty() {
            return Ok(Some(HashMap::new()));
        }
        if cancelled() {
            return Ok(None);
        }

        let mut format_by_id = HashMap::with_capacity(commits.len());
        let mut stdin = String::with_capacity(commits.len() * 41);
        for &(id, format) in commits {
            if format_by_id.insert(id, format).is_none() {
                stdin.push_str(&id.to_string());
                stdin.push('\n');
            }
        }

        let format_arg = format!("--format={LOG_FORMAT}");
        let args = ["log", "--no-walk=unsorted", "--stdin", "-z", &format_arg];
        let Some(output) =
            self.run_output_cancellable(&args, Some(stdin.as_bytes()), false, cancelled)?
        else {
            return Ok(None);
        };
        if !output.status.success() {
            let owned = to_args(&args);
            return Err(command_error(&owned, &output));
        }

        let stderr_text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let parsed = parse_git_log_signatures(&output.stdout)?;
        let mut results = HashMap::with_capacity(format_by_id.len());

        for row in parsed {
            let Some(&format) = format_by_id.get(&row.id) else {
                continue;
            };
            let id = row.id;
            let signature = finalize_row(row, format, &stderr_text, hints);
            results.insert(id, signature);
        }

        // If git skipped a commit entirely despite a detected signature header,
        // still report it as CannotCheck rather than silently dropping it.
        for (id, format) in format_by_id {
            results.entry(id).or_insert_with(|| {
                let message = stderr_text.clone();
                let missing_hint = Some(missing_hint_for(format, None, &message, hints));
                CommitSignature {
                    format,
                    validity: SignatureValidity::CannotCheck,
                    signer: None,
                    key: None,
                    fingerprint: None,
                    primary_key_fingerprint: None,
                    trust_level: None,
                    message,
                    missing_hint,
                }
            });
        }

        Ok(Some(results))
    }
}

/// Parses NUL-delimited records emitted by `git log -z --format=...`.
pub fn parse_git_log_signatures(stdout: &[u8]) -> Result<Vec<ParsedSignatureRow>> {
    let mut rows = Vec::new();
    for raw_record in stdout.split(|&byte| byte == 0) {
        if raw_record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(raw_record);
        let mut fields = text.splitn(8, '\x1f');
        let oid_str = fields.next().unwrap_or_default().trim();
        let id: Oid = oid_str.parse().map_err(|error| {
            Error::other(format!(
                "invalid commit id in signature verification output ({oid_str:?}): {error}"
            ))
        })?;
        let code = fields.next().unwrap_or_default();
        let validity = SignatureValidity::from_git_code(code);
        let signer = non_empty(fields.next());
        let key = non_empty(fields.next());
        let fingerprint = non_empty(fields.next());
        let primary_key_fingerprint = non_empty(fields.next());
        let trust_level = non_empty(fields.next());
        let message = fields
            .next()
            .unwrap_or_default()
            .trim_matches(['\r', '\n'])
            .to_owned();

        rows.push(ParsedSignatureRow {
            id,
            validity,
            signer,
            key,
            fingerprint,
            primary_key_fingerprint,
            trust_level,
            message,
        });
    }
    Ok(rows)
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn finalize_row(
    row: ParsedSignatureRow,
    format: SignatureFormat,
    stderr_text: &str,
    hints: &SignatureConfigHints,
) -> CommitSignature {
    // Because we only pass commits where `gix` already found a `gpgsig` or
    // `gpgsig-sha256` header, git emitting `N` means git could not invoke or
    // configure the verifier (e.g. missing `gpg.ssh.allowedSignersFile` or
    // missing `gpg` binary), which is `CannotCheck` (`E`), never unsigned.
    let validity = if matches!(
        row.validity,
        SignatureValidity::CannotCheck | SignatureValidity::None
    ) {
        SignatureValidity::CannotCheck
    } else {
        row.validity
    };

    let message = if row.message.is_empty() && validity == SignatureValidity::CannotCheck {
        stderr_text.to_owned()
    } else {
        row.message
    };

    let missing_hint = (validity == SignatureValidity::CannotCheck).then(|| {
        missing_hint_for(
            format,
            row.key.as_deref().or(row.fingerprint.as_deref()),
            if message.is_empty() {
                stderr_text
            } else {
                &message
            },
            hints,
        )
    });

    CommitSignature {
        format,
        validity,
        signer: row.signer,
        key: row.key,
        fingerprint: row.fingerprint,
        primary_key_fingerprint: row.primary_key_fingerprint,
        trust_level: row.trust_level,
        message,
        missing_hint,
    }
}

fn missing_hint_for(
    format: SignatureFormat,
    key: Option<&str>,
    diagnostic: &str,
    hints: &SignatureConfigHints,
) -> String {
    let lower = diagnostic.to_ascii_lowercase();
    match format {
        SignatureFormat::Ssh => {
            if lower.contains("allowedsignersfile") && lower.contains("need")
                || !hints.ssh_allowed_signers_configured
            {
                "Set gpg.ssh.allowedSignersFile to verify SSH signatures.".to_owned()
            } else if !hints.ssh_allowed_signers_exists
                || lower.contains("no such file")
                || lower.contains("does not exist")
                || lower.contains("failed reading")
            {
                match &hints.ssh_allowed_signers_path {
                    Some(path) => format!(
                        "Configured gpg.ssh.allowedSignersFile does not exist: {}",
                        path.display()
                    ),
                    None => "Configured gpg.ssh.allowedSignersFile does not exist.".to_owned(),
                }
            } else if lower.contains("ssh-keygen") {
                "Ensure ssh-keygen is installed and on PATH to verify SSH signatures.".to_owned()
            } else {
                "Check gpg.ssh.allowedSignersFile and your SSH signing configuration.".to_owned()
            }
        }
        SignatureFormat::OpenPgp => {
            if lower.contains("cannot run gpg")
                || lower.contains("no such file")
                || lower.contains("failed to execute")
            {
                "Install GnuPG (gpg) or set gpg.program to verify OpenPGP signatures.".to_owned()
            } else if lower.contains("no public key")
                || lower.contains("no_pubkey")
                || lower.contains("public key not found")
            {
                match key {
                    Some(key) => {
                        format!("Missing OpenPGP public key ({key}) in your GnuPG keyring.")
                    }
                    None => "Missing OpenPGP public key in your GnuPG keyring.".to_owned(),
                }
            } else {
                "Ensure gpg and the signer's public key are available to verify OpenPGP signatures."
                    .to_owned()
            }
        }
        SignatureFormat::X509 => {
            if lower.contains("cannot run")
                || lower.contains("gpgsm")
                || lower.contains("no such file")
            {
                "Install GnuPG S/MIME (gpgsm) or set gpg.x509.program to verify X.509 signatures."
                    .to_owned()
            } else {
                "Ensure the signer's X.509 certificate and CA trust store are available to gpgsm."
                    .to_owned()
            }
        }
        SignatureFormat::Unknown => {
            "Unrecognized signature format or missing verification program.".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> Oid {
        format!("{n:040x}").parse().expect("valid oid")
    }

    #[test]
    fn parses_all_git_signature_status_codes_with_multiline_messages() {
        let mut raw = Vec::new();
        let cases = [
            (
                oid(1),
                "G",
                "Alice <alice@example.com>",
                "SHA256:abc",
                "FP1",
                "",
                "fully",
                "Good \"git\" signature for alice@example.com\nwith ED25519 key SHA256:abc\n",
                SignatureValidity::Good,
            ),
            (
                oid(2),
                "B",
                "Mallory <mallory@example.com>",
                "DEAD1234",
                "FP2",
                "PFP2",
                "never",
                "gpg: BAD signature from \"Mallory\"\n",
                SignatureValidity::Bad,
            ),
            (
                oid(3),
                "U",
                "Bob <bob@example.com>",
                "BEEF5678",
                "FP3",
                "",
                "undefined",
                "gpg: Good signature from \"Bob\" [unknown]\n",
                SignatureValidity::UnknownValidity,
            ),
            (
                oid(4),
                "X",
                "Carol",
                "1111",
                "",
                "",
                "marginal",
                "Signature expired",
                SignatureValidity::Expired,
            ),
            (
                oid(5),
                "Y",
                "Dave",
                "2222",
                "",
                "",
                "marginal",
                "Key expired",
                SignatureValidity::ExpiredKey,
            ),
            (
                oid(6),
                "R",
                "Eve",
                "3333",
                "",
                "",
                "never",
                "Key revoked",
                SignatureValidity::RevokedKey,
            ),
            (
                oid(7),
                "E",
                "",
                "4444",
                "",
                "",
                "undefined",
                "gpg: Can't check signature: No public key\n",
                SignatureValidity::CannotCheck,
            ),
            (oid(8), "N", "", "", "", "", "", "", SignatureValidity::None),
        ];

        for (id, code, signer, key, fp, pfp, trust, msg, _) in &cases {
            let record =
                format!("{id}\x1f{code}\x1f{signer}\x1f{key}\x1f{fp}\x1f{pfp}\x1f{trust}\x1f{msg}");
            raw.extend_from_slice(record.as_bytes());
            raw.push(0);
        }

        let parsed = parse_git_log_signatures(&raw).expect("parse records");
        assert_eq!(parsed.len(), cases.len());
        for (row, (id, _, _, _, _, _, _, _, expected_validity)) in parsed.iter().zip(cases.iter()) {
            assert_eq!(row.id, *id);
            assert_eq!(row.validity, *expected_validity);
        }
        assert_eq!(
            parsed[0].signer.as_deref(),
            Some("Alice <alice@example.com>")
        );
        assert_eq!(parsed[0].key.as_deref(), Some("SHA256:abc"));
        assert_eq!(parsed[0].fingerprint.as_deref(), Some("FP1"));
        assert_eq!(parsed[0].primary_key_fingerprint, None);
        assert_eq!(parsed[0].trust_level.as_deref(), Some("fully"));
        assert_eq!(
            parsed[0].message,
            "Good \"git\" signature for alice@example.com\nwith ED25519 key SHA256:abc"
        );
        assert_eq!(parsed[1].primary_key_fingerprint.as_deref(), Some("PFP2"));
    }

    #[test]
    fn signed_commit_with_n_becomes_cannot_check_with_actionable_hint() {
        let row = ParsedSignatureRow {
            id: oid(1),
            validity: SignatureValidity::None,
            signer: None,
            key: None,
            fingerprint: None,
            primary_key_fingerprint: None,
            trust_level: None,
            message: String::new(),
        };
        let sig = finalize_row(
            row,
            SignatureFormat::Ssh,
            "error: gpg.ssh.allowedSignersFile needs to be configured and exist for ssh signature verification",
            &SignatureConfigHints::default(),
        );
        assert_eq!(sig.validity, SignatureValidity::CannotCheck);
        assert_ne!(sig.validity, SignatureValidity::Bad);
        assert_eq!(
            sig.missing_hint.as_deref(),
            Some("Set gpg.ssh.allowedSignersFile to verify SSH signatures.")
        );
        assert!(sig.message.contains("allowedSignersFile"));

        let gpg_row = ParsedSignatureRow {
            id: oid(2),
            validity: SignatureValidity::CannotCheck,
            signer: None,
            key: Some("ABCD1234".to_owned()),
            fingerprint: None,
            primary_key_fingerprint: None,
            trust_level: None,
            message: "gpg: Can't check signature: No public key".to_owned(),
        };
        let gpg_sig = finalize_row(
            gpg_row,
            SignatureFormat::OpenPgp,
            "",
            &SignatureConfigHints::default(),
        );
        assert_eq!(gpg_sig.validity, SignatureValidity::CannotCheck);
        assert!(
            gpg_sig
                .missing_hint
                .as_deref()
                .unwrap()
                .contains("ABCD1234")
        );
    }
}
