//! Pure update logic: manifest verification, version comparison, platform
//! asset selection, and install-kind detection.
//!
//! Kept free of network I/O and GPUI state so every security check and decision
//! rule runs in ordinary unit tests on every platform.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Embedded minisign public key used to verify `latest.json` and downloaded assets.
///
/// The placeholder ships until a release keypair is generated; while it is
/// present, verification and installation always fail closed.
pub const UPDATE_PUBLIC_KEY: &str = include_str!("update_key.pub");

pub const MANIFEST_URL: &str =
    "https://github.com/satotek/strata/releases/latest/download/latest.json";
pub const MANIFEST_SIG_URL: &str =
    "https://github.com/satotek/strata/releases/latest/download/latest.json.minisig";
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/satotek/strata/releases/download";

/// Minimum interval between automatic startup update checks (24 hours).
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Empty marker file created next to `strata.exe` by the Windows installer.
pub const WINDOWS_SETUP_MARKER: &str = "installed-by-setup";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestAsset {
    pub name: String,
    pub sha256: String,
    pub minisig: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub tag: String,
    pub notes_url: String,
    pub assets: BTreeMap<String, ManifestAsset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Windows,
    Linux,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
    Other,
}

impl Arch {
    pub fn current() -> Self {
        if cfg!(target_arch = "aarch64") {
            Self::Aarch64
        } else if cfg!(target_arch = "x86_64") {
            Self::X86_64
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    MacosBundle { bundle_path: PathBuf },
    WindowsSetup,
    NotifyOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckTrigger {
    Automatic,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMode {
    MacosDmg {
        bundle_path: PathBuf,
        asset: ManifestAsset,
    },
    WindowsSetup {
        asset: ManifestAsset,
    },
    NotifyOnly {
        asset: Option<ManifestAsset>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: String,
    pub tag: String,
    pub notes_url: String,
    pub mode: InstallMode,
}

impl AvailableUpdate {
    pub fn can_install(&self) -> bool {
        !matches!(self.mode, InstallMode::NotifyOnly { .. })
    }

    #[cfg(test)]
    pub fn asset(&self) -> Option<&ManifestAsset> {
        match &self.mode {
            InstallMode::MacosDmg { asset, .. } | InstallMode::WindowsSetup { asset } => {
                Some(asset)
            }
            InstallMode::NotifyOnly { asset } => asset.as_ref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    UpToDate,
    Skipped(String),
    Available(AvailableUpdate),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    PlaceholderKey,
    InvalidPublicKey(String),
    SignatureDecode(String),
    SignatureVerify(String),
    ManifestParse(String),
    InvalidManifest(String),
    AssetSignatureVerify(String),
    Sha256Mismatch { expected: String, actual: String },
    Io(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlaceholderKey => {
                write!(
                    f,
                    "embedded update public key is still the placeholder; refusing update"
                )
            }
            Self::InvalidPublicKey(err) => write!(f, "invalid update public key: {err}"),
            Self::SignatureDecode(err) => write!(f, "invalid minisign signature: {err}"),
            Self::SignatureVerify(err) => {
                write!(f, "manifest signature verification failed: {err}")
            }
            Self::ManifestParse(err) => write!(f, "could not parse update manifest: {err}"),
            Self::InvalidManifest(err) => write!(f, "invalid update manifest: {err}"),
            Self::AssetSignatureVerify(err) => {
                write!(f, "downloaded asset signature verification failed: {err}")
            }
            Self::Sha256Mismatch { expected, actual } => {
                write!(
                    f,
                    "downloaded asset SHA-256 mismatch: expected {expected}, got {actual}"
                )
            }
            Self::Io(err) => write!(f, "I/O error during update verification: {err}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Returns `true` when `key_text` is empty or still holds the placeholder token.
pub fn is_placeholder_key(key_text: &str) -> bool {
    let trimmed = key_text.trim();
    trimmed.is_empty() || trimmed.contains("PLACEHOLDER")
}

/// Decodes `key_text` as a minisign public key, failing closed on the placeholder.
pub fn parse_public_key(key_text: &str) -> Result<PublicKey, UpdateError> {
    if is_placeholder_key(key_text) {
        log::warn!(
            "refusing update operation: embedded minisign public key is still the placeholder"
        );
        return Err(UpdateError::PlaceholderKey);
    }
    PublicKey::decode(key_text).map_err(|err| UpdateError::InvalidPublicKey(err.to_string()))
}

/// Returns `true` if the running binary carries a release version suitable for
/// background auto-checks (never true in debug/dev builds).
pub fn has_release_version(is_debug: bool, current_version: &str) -> bool {
    if is_debug {
        return false;
    }
    let Ok(parsed) = Version::parse(current_version.trim()) else {
        return false;
    };
    parsed != Version::new(0, 0, 0) && !parsed.pre.as_str().contains("dev")
}

/// Returns `true` when enough time has elapsed since `last_checked_at` to run
/// another automatic startup check.
pub fn is_check_due(last_checked_at: Option<u64>, now_secs: u64) -> bool {
    match last_checked_at {
        None => true,
        Some(last) => now_secs.saturating_sub(last) >= CHECK_INTERVAL_SECS,
    }
}

/// Extracts the `.app` bundle path when `exe` lives at `<App>.app/Contents/MacOS/<bin>`.
pub fn macos_bundle_path(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents_dir = macos_dir.parent()?;
    if contents_dir.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let app_dir = contents_dir.parent()?;
    if app_dir.extension()?.to_str()? != "app" {
        return None;
    }
    Some(app_dir.to_path_buf())
}

/// Determines how updates can be installed for the running binary.
pub fn detect_install_kind(platform: Platform, exe: Option<&Path>, is_debug: bool) -> InstallKind {
    if is_debug {
        return InstallKind::NotifyOnly;
    }
    match platform {
        Platform::Macos => match exe.and_then(macos_bundle_path) {
            Some(bundle_path) => InstallKind::MacosBundle { bundle_path },
            None => InstallKind::NotifyOnly,
        },
        Platform::Windows => {
            let has_marker = exe
                .and_then(Path::parent)
                .is_some_and(|dir| dir.join(WINDOWS_SETUP_MARKER).is_file());
            if has_marker {
                InstallKind::WindowsSetup
            } else {
                InstallKind::NotifyOnly
            }
        }
        Platform::Linux | Platform::Other => InstallKind::NotifyOnly,
    }
}

/// Chooses the `latest.json` `assets` key for `(platform, arch, install_kind)`.
pub fn asset_key_for(
    platform: Platform,
    arch: Arch,
    install_kind: &InstallKind,
) -> Option<&'static str> {
    match (platform, arch) {
        (Platform::Macos, Arch::Aarch64) => Some("macos-aarch64"),
        (Platform::Windows, Arch::X86_64) => match install_kind {
            InstallKind::WindowsSetup => Some("windows-x86_64"),
            InstallKind::NotifyOnly | InstallKind::MacosBundle { .. } => {
                Some("windows-x86_64-portable")
            }
        },
        (Platform::Linux, Arch::X86_64) => Some("linux-x86_64"),
        _ => None,
    }
}

/// Builds the download URL for a validated release tag and asset filename.
pub fn asset_download_url(tag: &str, name: &str) -> Result<String, UpdateError> {
    validate_tag(tag)?;
    validate_asset_name(name)?;
    Ok(format!("{RELEASE_DOWNLOAD_BASE}/{tag}/{name}"))
}

fn validate_tag(tag: &str) -> Result<(), UpdateError> {
    if tag.is_empty()
        || tag.contains("..")
        || tag.chars().any(|c| {
            c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#' | '%')
        })
    {
        return Err(UpdateError::InvalidManifest(format!(
            "invalid release tag {tag:?}"
        )));
    }
    Ok(())
}

fn validate_asset_name(name: &str) -> Result<(), UpdateError> {
    if name.is_empty()
        || name == "."
        || name.contains("..")
        || name
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#'))
    {
        return Err(UpdateError::InvalidManifest(format!(
            "invalid asset name {name:?}"
        )));
    }
    Ok(())
}

fn validate_notes_url(url: &str) -> Result<(), UpdateError> {
    if !url.starts_with("https://") || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(UpdateError::InvalidManifest(format!(
            "invalid notes_url {url:?}"
        )));
    }
    Ok(())
}

fn validate_sha256_hex(hex_str: &str) -> Result<(), UpdateError> {
    let trimmed = hex_str.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(UpdateError::InvalidManifest(format!(
            "invalid sha256 digest {hex_str:?}"
        )));
    }
    Ok(())
}

/// Verifies `manifest_bytes` against `minisig_text` using `key_text` BEFORE
/// deserializing JSON, then validates manifest fields.
pub fn verify_manifest_and_parse(
    key_text: &str,
    manifest_bytes: &[u8],
    minisig_text: &str,
) -> Result<Manifest, UpdateError> {
    let pk = parse_public_key(key_text)?;
    let sig = Signature::decode(minisig_text)
        .map_err(|err| UpdateError::SignatureDecode(err.to_string()))?;
    pk.verify(manifest_bytes, &sig, false)
        .map_err(|err| UpdateError::SignatureVerify(err.to_string()))?;

    let manifest: Manifest = serde_json::from_slice(manifest_bytes)
        .map_err(|err| UpdateError::ManifestParse(err.to_string()))?;

    Version::parse(manifest.version.trim()).map_err(|err| {
        UpdateError::InvalidManifest(format!("invalid version {:?}: {err}", manifest.version))
    })?;
    validate_tag(&manifest.tag)?;
    validate_notes_url(&manifest.notes_url)?;
    for (key, asset) in &manifest.assets {
        validate_asset_name(&asset.name)
            .map_err(|err| UpdateError::InvalidManifest(format!("asset {key:?}: {err}")))?;
        validate_sha256_hex(&asset.sha256)
            .map_err(|err| UpdateError::InvalidManifest(format!("asset {key:?}: {err}")))?;
    }

    Ok(manifest)
}

/// Verifies a downloaded asset stream against BOTH its minisign signature and
/// its expected SHA-256 hex digest in a single streaming pass.
pub fn verify_reader_against_asset<R: Read>(
    key_text: &str,
    reader: &mut R,
    expected_minisig: &str,
    expected_sha256: &str,
) -> Result<(), UpdateError> {
    let pk = parse_public_key(key_text)?;
    validate_sha256_hex(expected_sha256)?;
    let sig = Signature::decode(expected_minisig)
        .map_err(|err| UpdateError::SignatureDecode(err.to_string()))?;
    let mut verifier = pk
        .verify_stream(&sig)
        .map_err(|err| UpdateError::AssetSignatureVerify(err.to_string()))?;
    let mut hasher = Sha256::new();

    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|err| UpdateError::Io(err.to_string()))?;
        if read == 0 {
            break;
        }
        let chunk = &buf[..read];
        hasher.update(chunk);
        verifier.update(chunk);
    }

    verifier
        .finalize()
        .map_err(|err| UpdateError::AssetSignatureVerify(err.to_string()))?;

    let actual = hex::encode(hasher.finalize());
    let expected = expected_sha256.trim();
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(UpdateError::Sha256Mismatch {
            expected: expected.to_ascii_lowercase(),
            actual,
        });
    }

    Ok(())
}

/// Decides whether a verified manifest offers a newer version for this build.
pub fn evaluate_update(
    manifest: &Manifest,
    current_version: &str,
    skipped_version: Option<&str>,
    trigger: CheckTrigger,
    platform: Platform,
    arch: Arch,
    install_kind: InstallKind,
) -> Result<UpdateDecision, UpdateError> {
    let current = Version::parse(current_version.trim()).map_err(|err| {
        UpdateError::InvalidManifest(format!(
            "invalid current version {current_version:?}: {err}"
        ))
    })?;
    let latest = Version::parse(manifest.version.trim()).map_err(|err| {
        UpdateError::InvalidManifest(format!(
            "invalid manifest version {:?}: {err}",
            manifest.version
        ))
    })?;

    if latest <= current {
        return Ok(UpdateDecision::UpToDate);
    }

    if trigger == CheckTrigger::Automatic && skipped_version == Some(manifest.version.trim()) {
        return Ok(UpdateDecision::Skipped(manifest.version.clone()));
    }

    let asset = asset_key_for(platform, arch, &install_kind)
        .and_then(|key| manifest.assets.get(key).cloned());

    let mode = match (install_kind, asset) {
        (InstallKind::MacosBundle { bundle_path }, Some(asset)) => {
            InstallMode::MacosDmg { bundle_path, asset }
        }
        (InstallKind::WindowsSetup, Some(asset)) => InstallMode::WindowsSetup { asset },
        (_, asset) => InstallMode::NotifyOnly { asset },
    };

    Ok(UpdateDecision::Available(AvailableUpdate {
        version: manifest.version.clone(),
        tag: manifest.tag.clone(),
        notes_url: manifest.notes_url.clone(),
        mode,
    }))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use minisign::KeyPair;

    use super::*;

    struct TestKeys {
        public_key: String,
        secret_key: minisign::SecretKey,
        minisign_pk: minisign::PublicKey,
    }

    fn generate_test_keys() -> TestKeys {
        let kp = KeyPair::generate_unencrypted_keypair().expect("generate ephemeral keypair");
        let pk_box = kp.pk.to_box().expect("public key box");
        let public_key = pk_box.to_string();
        TestKeys {
            public_key,
            secret_key: kp.sk,
            minisign_pk: kp.pk,
        }
    }

    fn sign_bytes(keys: &TestKeys, bytes: &[u8]) -> String {
        let sig_box = minisign::sign(
            Some(&keys.minisign_pk),
            &keys.secret_key,
            Cursor::new(bytes),
            None,
            None,
        )
        .expect("sign test payload");
        sig_box.to_string()
    }

    fn sample_manifest(keys: &TestKeys, asset_bytes: &[u8]) -> (Vec<u8>, String) {
        let sha256 = hex::encode(Sha256::digest(asset_bytes));
        let asset_sig = sign_bytes(keys, asset_bytes);

        let mut assets = BTreeMap::new();
        for (key, name) in [
            ("macos-aarch64", "Strata-0.2.0-macos-arm64.dmg"),
            ("windows-x86_64", "Strata-0.2.0-windows-x64-setup.exe"),
            ("windows-x86_64-portable", "Strata-0.2.0-windows-x64.zip"),
            ("linux-x86_64", "strata-0.2.0-linux-x86_64.tar.gz"),
        ] {
            assets.insert(
                key.to_owned(),
                ManifestAsset {
                    name: name.to_owned(),
                    sha256: sha256.clone(),
                    minisig: asset_sig.clone(),
                },
            );
        }

        let manifest = Manifest {
            version: "0.2.0".to_owned(),
            tag: "v0.2.0".to_owned(),
            notes_url: "https://github.com/satotek/strata/releases/tag/v0.2.0".to_owned(),
            assets,
        };
        let manifest_bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
        let manifest_sig = sign_bytes(keys, &manifest_bytes);
        (manifest_bytes, manifest_sig)
    }

    #[test]
    fn placeholder_key_is_detected_and_refused() {
        assert!(is_placeholder_key(UPDATE_PUBLIC_KEY));
        assert!(is_placeholder_key(""));
        assert!(matches!(
            parse_public_key(UPDATE_PUBLIC_KEY),
            Err(UpdateError::PlaceholderKey)
        ));

        let keys = generate_test_keys();
        assert!(!is_placeholder_key(&keys.public_key));
        let payload = b"fake binary";
        let (manifest_bytes, manifest_sig) = sample_manifest(&keys, payload);
        assert!(matches!(
            verify_manifest_and_parse(UPDATE_PUBLIC_KEY, &manifest_bytes, &manifest_sig),
            Err(UpdateError::PlaceholderKey)
        ));

        let sha = hex::encode(Sha256::digest(payload));
        let sig = sign_bytes(&keys, payload);
        assert!(matches!(
            verify_reader_against_asset(UPDATE_PUBLIC_KEY, &mut Cursor::new(payload), &sig, &sha,),
            Err(UpdateError::PlaceholderKey)
        ));
    }

    #[test]
    fn verifies_signed_manifest_and_rejects_tampering_or_wrong_key() {
        let keys = generate_test_keys();
        let wrong_keys = generate_test_keys();
        let asset_bytes = b"release archive contents";
        let (manifest_bytes, manifest_sig) = sample_manifest(&keys, asset_bytes);

        let parsed = verify_manifest_and_parse(&keys.public_key, &manifest_bytes, &manifest_sig)
            .expect("valid signed manifest");
        assert_eq!(parsed.version, "0.2.0");
        assert_eq!(parsed.tag, "v0.2.0");

        let mut tampered = manifest_bytes.clone();
        tampered[0] ^= 0x01;
        assert!(matches!(
            verify_manifest_and_parse(&keys.public_key, &tampered, &manifest_sig),
            Err(UpdateError::SignatureVerify(_))
        ));

        assert!(matches!(
            verify_manifest_and_parse(&wrong_keys.public_key, &manifest_bytes, &manifest_sig),
            Err(UpdateError::SignatureVerify(_))
        ));
    }

    #[test]
    fn verifies_asset_against_both_minisig_and_sha256() {
        let keys = generate_test_keys();
        let payload = b"Strata installer bytes 1234567890";
        let sha = hex::encode(Sha256::digest(payload));
        let sig = sign_bytes(&keys, payload);

        verify_reader_against_asset(&keys.public_key, &mut Cursor::new(payload), &sig, &sha)
            .expect("valid asset passes both checks");

        // Correct signature over modified payload is caught; tampered bytes vs original sig is caught.
        let mut altered = payload.to_vec();
        altered[5] ^= 0xFF;
        assert!(matches!(
            verify_reader_against_asset(&keys.public_key, &mut Cursor::new(&altered), &sig, &sha),
            Err(UpdateError::AssetSignatureVerify(_))
        ));

        // Even if someone re-signed altered bytes with the wrong hash, SHA-256 mismatch rejects it.
        let altered_sig = sign_bytes(&keys, &altered);
        assert!(matches!(
            verify_reader_against_asset(
                &keys.public_key,
                &mut Cursor::new(&altered),
                &altered_sig,
                &sha,
            ),
            Err(UpdateError::Sha256Mismatch { .. })
        ));

        // Even with matching SHA-256, a wrong minisig rejects it.
        let other_keys = generate_test_keys();
        let other_sig = sign_bytes(&other_keys, payload);
        assert!(matches!(
            verify_reader_against_asset(
                &keys.public_key,
                &mut Cursor::new(payload),
                &other_sig,
                &sha,
            ),
            Err(UpdateError::AssetSignatureVerify(_))
        ));
    }

    #[test]
    fn rejects_manifest_with_path_traversal_or_non_https_notes_url() {
        let keys = generate_test_keys();
        let payload = b"bytes";
        let sha = hex::encode(Sha256::digest(payload));
        let sig = sign_bytes(&keys, payload);

        let mut assets = BTreeMap::new();
        assets.insert(
            "macos-aarch64".to_owned(),
            ManifestAsset {
                name: "../escape.dmg".to_owned(),
                sha256: sha,
                minisig: sig,
            },
        );
        let bad_manifest = Manifest {
            version: "0.2.0".to_owned(),
            tag: "v0.2.0".to_owned(),
            notes_url: "https://github.com/satotek/strata/releases/tag/v0.2.0".to_owned(),
            assets,
        };
        let bytes = serde_json::to_vec(&bad_manifest).unwrap();
        let msig = sign_bytes(&keys, &bytes);
        assert!(matches!(
            verify_manifest_and_parse(&keys.public_key, &bytes, &msig),
            Err(UpdateError::InvalidManifest(_))
        ));

        let http_manifest = Manifest {
            version: "0.2.0".to_owned(),
            tag: "v0.2.0".to_owned(),
            notes_url: "http://example.com".to_owned(),
            assets: BTreeMap::new(),
        };
        let bytes = serde_json::to_vec(&http_manifest).unwrap();
        let msig = sign_bytes(&keys, &bytes);
        assert!(matches!(
            verify_manifest_and_parse(&keys.public_key, &bytes, &msig),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn version_comparison_and_skip_behavior() {
        let keys = generate_test_keys();
        let (bytes, sig) = sample_manifest(&keys, b"dmg");
        let manifest = verify_manifest_and_parse(&keys.public_key, &bytes, &sig).unwrap();

        // Older or equal current version is up to date.
        assert_eq!(
            evaluate_update(
                &manifest,
                "0.2.0",
                None,
                CheckTrigger::Manual,
                Platform::Linux,
                Arch::X86_64,
                InstallKind::NotifyOnly,
            )
            .unwrap(),
            UpdateDecision::UpToDate
        );
        assert_eq!(
            evaluate_update(
                &manifest,
                "0.3.0",
                None,
                CheckTrigger::Manual,
                Platform::Linux,
                Arch::X86_64,
                InstallKind::NotifyOnly,
            )
            .unwrap(),
            UpdateDecision::UpToDate
        );

        // Newer version is skipped on automatic checks when matching skipped_version,
        // but still offered on manual checks.
        assert_eq!(
            evaluate_update(
                &manifest,
                "0.1.6",
                Some("0.2.0"),
                CheckTrigger::Automatic,
                Platform::Linux,
                Arch::X86_64,
                InstallKind::NotifyOnly,
            )
            .unwrap(),
            UpdateDecision::Skipped("0.2.0".to_owned())
        );
        assert!(matches!(
            evaluate_update(
                &manifest,
                "0.1.6",
                Some("0.2.0"),
                CheckTrigger::Manual,
                Platform::Linux,
                Arch::X86_64,
                InstallKind::NotifyOnly,
            )
            .unwrap(),
            UpdateDecision::Available(_)
        ));
    }

    #[test]
    fn selects_asset_and_mode_by_platform_and_install_kind() {
        let keys = generate_test_keys();
        let (bytes, sig) = sample_manifest(&keys, b"payload");
        let manifest = verify_manifest_and_parse(&keys.public_key, &bytes, &sig).unwrap();

        // macOS bundle
        let mac_bundle = detect_install_kind(
            Platform::Macos,
            Some(Path::new("/Applications/Strata.app/Contents/MacOS/strata")),
            false,
        );
        let mac_decision = evaluate_update(
            &manifest,
            "0.1.6",
            None,
            CheckTrigger::Automatic,
            Platform::Macos,
            Arch::Aarch64,
            mac_bundle,
        )
        .unwrap();
        match mac_decision {
            UpdateDecision::Available(avail) => {
                assert!(avail.can_install());
                assert_eq!(avail.asset().unwrap().name, "Strata-0.2.0-macos-arm64.dmg");
            }
            other => panic!("unexpected macOS bundle decision: {other:?}"),
        }

        // Dev build on macOS -> notify-only
        let mac_dev = detect_install_kind(
            Platform::Macos,
            Some(Path::new("/Applications/Strata.app/Contents/MacOS/strata")),
            true,
        );
        assert_eq!(mac_dev, InstallKind::NotifyOnly);
        match evaluate_update(
            &manifest,
            "0.1.6",
            None,
            CheckTrigger::Manual,
            Platform::Macos,
            Arch::Aarch64,
            mac_dev,
        )
        .unwrap()
        {
            UpdateDecision::Available(avail) => assert!(!avail.can_install()),
            other => panic!("unexpected macOS dev decision: {other:?}"),
        }

        // Windows setup marker vs portable
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("strata.exe");
        std::fs::write(&exe, b"exe").unwrap();
        assert_eq!(
            detect_install_kind(Platform::Windows, Some(&exe), false),
            InstallKind::NotifyOnly
        );
        let portable = evaluate_update(
            &manifest,
            "0.1.6",
            None,
            CheckTrigger::Automatic,
            Platform::Windows,
            Arch::X86_64,
            InstallKind::NotifyOnly,
        )
        .unwrap();
        match portable {
            UpdateDecision::Available(avail) => {
                assert!(!avail.can_install());
                assert_eq!(avail.asset().unwrap().name, "Strata-0.2.0-windows-x64.zip");
            }
            other => panic!("unexpected windows portable decision: {other:?}"),
        }

        std::fs::write(dir.path().join(WINDOWS_SETUP_MARKER), b"").unwrap();
        let setup_kind = detect_install_kind(Platform::Windows, Some(&exe), false);
        assert_eq!(setup_kind, InstallKind::WindowsSetup);
        match evaluate_update(
            &manifest,
            "0.1.6",
            None,
            CheckTrigger::Automatic,
            Platform::Windows,
            Arch::X86_64,
            setup_kind,
        )
        .unwrap()
        {
            UpdateDecision::Available(avail) => {
                assert!(avail.can_install());
                assert_eq!(
                    avail.asset().unwrap().name,
                    "Strata-0.2.0-windows-x64-setup.exe"
                );
            }
            other => panic!("unexpected windows setup decision: {other:?}"),
        }

        // Linux -> notify-only with tarball asset
        match evaluate_update(
            &manifest,
            "0.1.6",
            None,
            CheckTrigger::Automatic,
            Platform::Linux,
            Arch::X86_64,
            InstallKind::NotifyOnly,
        )
        .unwrap()
        {
            UpdateDecision::Available(avail) => {
                assert!(!avail.can_install());
                assert_eq!(
                    avail.asset().unwrap().name,
                    "strata-0.2.0-linux-x86_64.tar.gz"
                );
            }
            other => panic!("unexpected linux decision: {other:?}"),
        }
    }

    #[test]
    fn check_interval_and_release_version_gating() {
        assert!(is_check_due(None, 1_000));
        assert!(!is_check_due(Some(1_000), 1_000 + CHECK_INTERVAL_SECS - 1));
        assert!(is_check_due(Some(1_000), 1_000 + CHECK_INTERVAL_SECS));

        assert!(!has_release_version(true, "0.1.6"));
        assert!(!has_release_version(false, "0.0.0"));
        assert!(!has_release_version(false, "0.1.6-dev"));
        assert!(has_release_version(false, "0.1.6"));
    }

    #[test]
    fn asset_url_construction() {
        assert_eq!(
            asset_download_url("v0.1.7", "Strata-0.1.7-macos-arm64.dmg").unwrap(),
            "https://github.com/satotek/strata/releases/download/v0.1.7/Strata-0.1.7-macos-arm64.dmg"
        );
        assert!(asset_download_url("../evil", "file.dmg").is_err());
        assert!(asset_download_url("v0.1.7", "dir/file.dmg").is_err());
    }
}
