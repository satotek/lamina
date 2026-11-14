//! Cheap commit signature detection via `gix`.
//!
//! Reads only the raw commit headers (`gpgsig` / `gpgsig-sha256`) and classifies
//! the signature armor without spawning any external process. Cryptographic
//! verification runs on demand through `git_cli::signature`.

use std::path::PathBuf;

use gix::objs::FindExt as _;
use gix::objs::commit::ref_iter::Token as CommitToken;

use super::Repo;
use crate::{Error, Oid, Result};

/// Cryptographic format detected from a commit's `gpgsig` / `gpgsig-sha256` armor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignatureFormat {
    OpenPgp,
    Ssh,
    X509,
    Unknown,
}

impl SignatureFormat {
    /// Human-readable name for UI labels and tooltips.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenPgp => "OpenPGP",
            Self::Ssh => "SSH",
            Self::X509 => "X.509",
            Self::Unknown => "Signature",
        }
    }
}

/// Verification outcome mapped from git's `%G?` format specifier.
///
/// [`CannotCheck`](Self::CannotCheck) (`E`) is kept distinct from
/// [`Bad`](Self::Bad) (`B`): missing public keys, missing
/// `gpg.ssh.allowedSignersFile`, or a missing `gpg` executable mean the
/// signature could not be checked rather than that verification failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignatureValidity {
    /// `%G? = G`: good (valid) signature.
    Good,
    /// `%G? = B`: bad signature.
    Bad,
    /// `%G? = U`: good signature with unknown validity / untrusted key.
    UnknownValidity,
    /// `%G? = X`: good signature that has expired.
    Expired,
    /// `%G? = Y`: good signature made by an expired key.
    ExpiredKey,
    /// `%G? = R`: good signature made by a revoked key.
    RevokedKey,
    /// `%G? = E`: signature cannot be checked (missing key, missing config, missing program).
    CannotCheck,
    /// `%G? = N`: no signature.
    None,
}

impl SignatureValidity {
    /// Maps git's `%G?` single-letter code to a [`SignatureValidity`].
    pub fn from_git_code(code: &str) -> Self {
        match code.trim() {
            "G" => Self::Good,
            "B" => Self::Bad,
            "U" => Self::UnknownValidity,
            "X" => Self::Expired,
            "Y" => Self::ExpiredKey,
            "R" => Self::RevokedKey,
            "E" => Self::CannotCheck,
            _ => Self::None,
        }
    }

    pub fn is_signed(self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn is_verified(self) -> bool {
        matches!(self, Self::Good)
    }

    /// Short status description suitable for a UI label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Good => "Verified",
            Self::Bad => "Bad signature",
            Self::UnknownValidity => "Signed (untrusted key)",
            Self::Expired => "Signed (signature expired)",
            Self::ExpiredKey => "Signed (key expired)",
            Self::RevokedKey => "Signed (key revoked)",
            Self::CannotCheck => "Cannot verify",
            Self::None => "Unsigned",
        }
    }
}

/// Verification result for one signed commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSignature {
    pub format: SignatureFormat,
    pub validity: SignatureValidity,
    /// `%GS`: signer identity, when known.
    pub signer: Option<String>,
    /// `%GK`: signing key id or SSH fingerprint.
    pub key: Option<String>,
    /// `%GF`: fingerprint of the signing key.
    pub fingerprint: Option<String>,
    /// `%GP`: fingerprint of the primary key when signed with a subkey.
    pub primary_key_fingerprint: Option<String>,
    /// `%GT`: trust level (`ultimate`, `fully`, `marginal`, `never`, `undefined`).
    pub trust_level: Option<String>,
    /// `%GG` (or git's stderr when verification could not run): raw diagnostic message.
    pub message: String,
    /// Actionable hint when `validity == SignatureValidity::CannotCheck`.
    pub missing_hint: Option<String>,
}

/// Configuration hints read via `gix` to explain `CannotCheck` (`E`) states.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureConfigHints {
    pub ssh_allowed_signers_configured: bool,
    pub ssh_allowed_signers_exists: bool,
    pub ssh_allowed_signers_path: Option<PathBuf>,
}

impl Repo {
    /// Detects cheaply whether `commit` carries a `gpgsig` or `gpgsig-sha256`
    /// header and classifies its armor format without running `git`.
    pub fn commit_signature_format(&self, commit: Oid) -> Result<Option<SignatureFormat>> {
        let repo = self.local();
        let mut object_buf = Vec::new();
        detect_commit_signature_format(&repo, commit, &mut object_buf)
    }

    /// Batch version of [`commit_signature_format`](Self::commit_signature_format)
    /// that reuses a single object buffer across all commits.
    pub fn commit_signature_formats(
        &self,
        commits: &[Oid],
    ) -> Result<Vec<(Oid, Option<SignatureFormat>)>> {
        let repo = self.local();
        let mut object_buf = Vec::new();
        let mut results = Vec::with_capacity(commits.len());
        for &commit in commits {
            let format = detect_commit_signature_format(&repo, commit, &mut object_buf)?;
            results.push((commit, format));
        }
        Ok(results)
    }
}

fn detect_commit_signature_format(
    repo: &gix::Repository,
    id: Oid,
    object_buf: &mut Vec<u8>,
) -> Result<Option<SignatureFormat>> {
    let data = match repo.objects.find(&id, object_buf) {
        Ok(data) => data,
        Err(err) if err.is_not_found() => return Ok(None),
        Err(err) => return Err(Error::gix(err)),
    };
    if data.kind != gix::objs::Kind::Commit {
        return Ok(None);
    }
    for token in gix::objs::CommitRefIter::from_bytes(data.data, repo.object_hash()) {
        match token.map_err(Error::gix)? {
            CommitToken::ExtraHeader((name, value))
                if name == b"gpgsig" || name == b"gpgsig-sha256" =>
            {
                return Ok(Some(classify_signature_armor(value.as_ref())));
            }
            CommitToken::Message(_) => break,
            _ => {}
        }
    }
    Ok(None)
}

/// Classifies a `gpgsig` / `gpgsig-sha256` payload from its PEM/ASCII armor header.
pub fn classify_signature_armor(payload: &[u8]) -> SignatureFormat {
    let trimmed = payload.trim_ascii_start();
    if trimmed.starts_with(b"-----BEGIN PGP SIGNATURE-----")
        || trimmed.starts_with(b"-----BEGIN PGP MESSAGE-----")
    {
        SignatureFormat::OpenPgp
    } else if trimmed.starts_with(b"-----BEGIN SSH SIGNATURE-----") {
        SignatureFormat::Ssh
    } else if trimmed.starts_with(b"-----BEGIN SIGNED MESSAGE-----") {
        SignatureFormat::X509
    } else {
        SignatureFormat::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;

    #[test]
    fn armor_classification_covers_all_supported_formats() {
        assert_eq!(
            classify_signature_armor(b"-----BEGIN PGP SIGNATURE-----\n..."),
            SignatureFormat::OpenPgp
        );
        assert_eq!(
            classify_signature_armor(b"\n  -----BEGIN PGP MESSAGE-----\n..."),
            SignatureFormat::OpenPgp
        );
        assert_eq!(
            classify_signature_armor(b"-----BEGIN SSH SIGNATURE-----\n..."),
            SignatureFormat::Ssh
        );
        assert_eq!(
            classify_signature_armor(b"-----BEGIN SIGNED MESSAGE-----\n..."),
            SignatureFormat::X509
        );
        assert_eq!(
            classify_signature_armor(b"-----BEGIN SOMETHING ELSE-----\n"),
            SignatureFormat::Unknown
        );
        assert_eq!(classify_signature_armor(b""), SignatureFormat::Unknown);
    }

    #[test]
    fn git_validity_codes_map_cleanly_with_e_distinct_from_b() {
        assert_eq!(
            SignatureValidity::from_git_code("G"),
            SignatureValidity::Good
        );
        assert_eq!(
            SignatureValidity::from_git_code("B"),
            SignatureValidity::Bad
        );
        assert_eq!(
            SignatureValidity::from_git_code("U"),
            SignatureValidity::UnknownValidity
        );
        assert_eq!(
            SignatureValidity::from_git_code("X"),
            SignatureValidity::Expired
        );
        assert_eq!(
            SignatureValidity::from_git_code("Y"),
            SignatureValidity::ExpiredKey
        );
        assert_eq!(
            SignatureValidity::from_git_code("R"),
            SignatureValidity::RevokedKey
        );
        assert_eq!(
            SignatureValidity::from_git_code("E"),
            SignatureValidity::CannotCheck
        );
        assert_eq!(
            SignatureValidity::from_git_code("N"),
            SignatureValidity::None
        );
        assert_ne!(SignatureValidity::CannotCheck, SignatureValidity::Bad);
    }

    #[test]
    fn detects_gpgsig_and_gpgsig_sha256_headers_for_all_kinds_and_unsigned() {
        let test = TestRepo::new();
        test.write("a.txt", "one\n");
        let unsigned = test.commit("unsigned commit");

        let make_commit_with_header = |header_name: &str, armor: &str, msg: &str| -> Oid {
            let tree = test.git(&["rev-parse", "HEAD^{tree}"]).trim().to_owned();
            let indented = armor
                .lines()
                .map(|line| format!(" {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            let raw = format!(
                "tree {tree}\n\
                 author Strata Test <strata@example.invalid> 1700000000 +0000\n\
                 committer Strata Test <strata@example.invalid> 1700000000 +0000\n\
                 {header_name} {}\n\n\
                 {msg}\n",
                indented.trim_start()
            );
            let mut cmd = test.command("git");
            cmd.args(["hash-object", "-t", "commit", "-w", "--stdin"]);
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            let mut child = cmd.spawn().expect("spawn hash-object");
            {
                use std::io::Write as _;
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(raw.as_bytes())
                    .unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .expect("oid")
        };

        let pgp_commit = make_commit_with_header(
            "gpgsig",
            "-----BEGIN PGP SIGNATURE-----\niQIzBAEBCAAd\n-----END PGP SIGNATURE-----",
            "pgp commit",
        );
        let ssh_commit = make_commit_with_header(
            "gpgsig",
            "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH\n-----END SSH SIGNATURE-----",
            "ssh commit",
        );
        let x509_commit = make_commit_with_header(
            "gpgsig-sha256",
            "-----BEGIN SIGNED MESSAGE-----\nMII=\n-----END SIGNED MESSAGE-----",
            "x509 commit",
        );
        let unknown_commit = make_commit_with_header(
            "gpgsig",
            "-----BEGIN CUSTOM SIGNATURE-----\nAAAA\n-----END CUSTOM SIGNATURE-----",
            "unknown commit",
        );

        let repo = test.open();
        assert_eq!(repo.commit_signature_format(unsigned).unwrap(), None);
        assert_eq!(
            repo.commit_signature_format(pgp_commit).unwrap(),
            Some(SignatureFormat::OpenPgp)
        );
        assert_eq!(
            repo.commit_signature_format(ssh_commit).unwrap(),
            Some(SignatureFormat::Ssh)
        );
        assert_eq!(
            repo.commit_signature_format(x509_commit).unwrap(),
            Some(SignatureFormat::X509)
        );
        assert_eq!(
            repo.commit_signature_format(unknown_commit).unwrap(),
            Some(SignatureFormat::Unknown)
        );

        let batch = repo
            .commit_signature_formats(&[
                unsigned,
                pgp_commit,
                ssh_commit,
                x509_commit,
                unknown_commit,
            ])
            .unwrap();
        assert_eq!(
            batch,
            vec![
                (unsigned, None),
                (pgp_commit, Some(SignatureFormat::OpenPgp)),
                (ssh_commit, Some(SignatureFormat::Ssh)),
                (x509_commit, Some(SignatureFormat::X509)),
                (unknown_commit, Some(SignatureFormat::Unknown)),
            ]
        );
    }
}
