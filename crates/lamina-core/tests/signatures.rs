//! End-to-end tests for commit signature detection, `git log` verification,
//! `Session` caching/cancellation/invalidation, SSH signing, and optional OpenPGP.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lamina_core::Oid;
use lamina_core::git_cli::Git;
use lamina_core::repo::{CommitSignature, Repo, SignatureFormat, SignatureValidity};
use lamina_core::session::{Session, SessionEvent};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(20);
const SIGNER_EMAIL: &str = "signer@example.invalid";

/// `GNUPGHOME` as the gpg this test finds reads it.
///
/// The gpg Git for Windows ships runs under an MSYS runtime: it resolves the
/// value with its own POSIX layer, so a native `C:\...` home is read as a path
/// relative to the working directory. The `/c/...` spelling it mounts instead
/// is understood by gpg and passed through untouched by git.
fn gnupg_env(path: &Path) -> std::ffi::OsString {
    msys_path(&path.to_string_lossy().replace('\\', "/")).into()
}

fn msys_path(text: &str) -> String {
    match text.as_bytes() {
        [drive, b':', rest @ ..] if drive.is_ascii_alphabetic() => format!(
            "/{}{}",
            (*drive as char).to_ascii_lowercase(),
            String::from_utf8_lossy(rest)
        ),
        _ => text.to_owned(),
    }
}

struct IsolatedRepo {
    workdir: TempDir,
    env_dir: TempDir,
}

impl IsolatedRepo {
    fn new() -> Self {
        let repo = Self {
            workdir: TempDir::new().expect("workdir tempdir"),
            env_dir: TempDir::new().expect("env tempdir"),
        };
        fs::write(repo.global_config(), "").expect("write global config");
        fs::create_dir_all(repo.gnupg_home()).expect("create gnupg home");
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Lamina Test"]);
        repo.git(&["config", "user.email", SIGNER_EMAIL]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.git(&["config", "core.autocrlf", "false"]);
        repo
    }

    fn path(&self) -> &Path {
        self.workdir.path()
    }

    fn env_path(&self) -> &Path {
        self.env_dir.path()
    }

    fn global_config(&self) -> PathBuf {
        self.env_dir.path().join("gitconfig")
    }

    fn gnupg_home(&self) -> PathBuf {
        self.env_dir.path().join("gnupg")
    }

    /// `GNUPGHOME` for this fixture's home, in the spelling gpg expects.
    fn gnupg_home_env(&self) -> std::ffi::OsString {
        gnupg_env(&self.gnupg_home())
    }

    fn repo(&self) -> Repo {
        Repo::open(self.path()).expect("open repo")
    }

    fn git_handle(&self) -> Git {
        Git::new(self.path().to_path_buf())
            .with_env("GIT_CONFIG_GLOBAL", self.global_config())
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("XDG_CONFIG_HOME", self.env_path())
            .with_env("GNUPGHOME", self.gnupg_home_env())
            .with_env("GIT_AUTHOR_NAME", "Lamina Test")
            .with_env("GIT_AUTHOR_EMAIL", SIGNER_EMAIL)
            .with_env("GIT_COMMITTER_NAME", "Lamina Test")
            .with_env("GIT_COMMITTER_EMAIL", SIGNER_EMAIL)
            .with_env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .with_env("GIT_COMMITTER_DATE", "1700000000 +0000")
    }

    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(self.path())
            .env("GIT_CONFIG_GLOBAL", self.global_config())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("XDG_CONFIG_HOME", self.env_path())
            .env("GNUPGHOME", self.gnupg_home_env())
            .env("GIT_AUTHOR_NAME", "Lamina Test")
            .env("GIT_AUTHOR_EMAIL", SIGNER_EMAIL)
            .env("GIT_COMMITTER_NAME", "Lamina Test")
            .env("GIT_COMMITTER_EMAIL", SIGNER_EMAIL)
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        cmd
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.command("git").args(args).output().expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn write(&self, rel: &str, contents: &str) {
        let full = self.path().join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(full, contents).expect("write file");
    }

    fn head(&self) -> Oid {
        self.git(&["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("head oid")
    }

    /// Generates an unencrypted Ed25519 SSH signing key inside the temp dir.
    fn create_ssh_key(&self, name: &str) -> (PathBuf, String) {
        let key_path = self.env_dir.path().join(name);
        let output = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                SIGNER_EMAIL,
                "-f",
                key_path.to_str().expect("utf8 key path"),
            ])
            .output()
            .expect("run ssh-keygen");
        assert!(
            output.status.success(),
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pub_key = fs::read_to_string(key_path.with_extension("pub"))
            .expect("read public key")
            .trim()
            .to_owned();
        (key_path, pub_key)
    }

    fn tamper_commit(&self, commit: Oid, old_summary: &str, new_summary: &str) -> Oid {
        let raw = self.git(&["cat-file", "commit", &commit.to_string()]);
        let tampered = raw.replacen(old_summary, new_summary, 1);
        assert_ne!(raw, tampered, "tampering must change commit text");

        let mut child = self
            .command("git")
            .args(["hash-object", "-t", "commit", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn hash-object");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(tampered.as_bytes())
            .expect("write tampered commit");
        let output = child.wait_with_output().expect("wait hash-object");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("tampered oid")
    }
}

fn open_session(path: &Path) -> (Session, Receiver<SessionEvent>) {
    let (tx, rx) = mpsc::channel();
    let session = Session::open(path, move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");
    (session, rx)
}

fn wait_for_snapshot(session: &Session, rx: &Receiver<SessionEvent>) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let snapshot = session.snapshot();
        if snapshot.commits.complete && snapshot.head.is_some() {
            return;
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(rem) {
            Ok(SessionEvent::LoadFailed { error }) => panic!("load failed: {error}"),
            Ok(_) => {}
            Err(err) => panic!("timed out waiting for snapshot: {err}"),
        }
    }
}

fn wait_for_signatures(
    rx: &Receiver<SessionEvent>,
    ticket: lamina_core::session::QueryTicket,
) -> (
    Option<HashMap<Oid, SignatureFormat>>,
    HashMap<Oid, CommitSignature>,
) {
    let deadline = Instant::now() + TIMEOUT;
    let mut detected = None;
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(rem) {
            Ok(SessionEvent::CommitSignaturesDetected { ticket: t, formats }) if t == ticket => {
                detected = Some((*formats).clone());
            }
            Ok(SessionEvent::CommitSignaturesReady {
                ticket: t,
                signatures,
            }) if t == ticket => {
                return (detected, (*signatures).clone());
            }
            Ok(SessionEvent::QueryFailed { ticket: t, error }) if t == ticket => {
                panic!("signature query failed: {error}");
            }
            Ok(_) => {}
            Err(err) => panic!("timed out waiting for CommitSignaturesReady: {err}"),
        }
    }
}

#[test]
fn ssh_signed_commits_verify_g_u_e_and_bad_in_isolated_temp_repo() {
    let test = IsolatedRepo::new();
    let (key1, pub1) = test.create_ssh_key("id_ed25519_primary");
    let (key2, _pub2) = test.create_ssh_key("id_ed25519_unknown");

    // 1. Unsigned commit.
    test.write("a.txt", "unsigned\n");
    test.git(&["add", "-A"]);
    test.git(&["commit", "-q", "-m", "unsigned commit"]);
    let unsigned_oid = test.head();

    // 2. Signed commit with key1 (will be G once allowedSignersFile has key1).
    test.git(&["config", "gpg.format", "ssh"]);
    test.git(&[
        "config",
        "user.signingkey",
        key1.to_str().expect("key1 path"),
    ]);
    test.write("a.txt", "signed good\n");
    test.git(&["add", "-A"]);
    test.git(&["commit", "-q", "-S", "-m", "signed good commit"]);
    let good_oid = test.head();

    // 3. Signed commit with key2 (unknown key -> U when allowedSignersFile exists without key2).
    test.git(&[
        "config",
        "user.signingkey",
        key2.to_str().expect("key2 path"),
    ]);
    test.write("a.txt", "signed unknown\n");
    test.git(&["add", "-A"]);
    test.git(&["commit", "-q", "-S", "-m", "signed unknown commit"]);
    let unknown_oid = test.head();

    // 4. Tampered commit (B) using good_oid's signature on altered commit text.
    let bad_oid = test.tamper_commit(good_oid, "signed good commit", "tampered commit");

    let repo = test.repo();
    let formats = repo
        .commit_signature_formats(&[unsigned_oid, good_oid, unknown_oid, bad_oid])
        .expect("detect formats");
    assert_eq!(
        formats,
        vec![
            (unsigned_oid, None),
            (good_oid, Some(SignatureFormat::Ssh)),
            (unknown_oid, Some(SignatureFormat::Ssh)),
            (bad_oid, Some(SignatureFormat::Ssh)),
        ]
    );

    let signed: Vec<(Oid, SignatureFormat)> = formats
        .into_iter()
        .filter_map(|(id, fmt)| fmt.map(|f| (id, f)))
        .collect();

    // First verify WITHOUT gpg.ssh.allowedSignersFile configured -> E (CannotCheck), distinct from B.
    let git = test.git_handle();
    let hints_missing = git.signature_config_hints().expect("hints");
    let unconfigured = git
        .verify_commit_signatures(&signed, &hints_missing, &|| false)
        .expect("verify without allowedSignersFile")
        .expect("not cancelled");
    assert_eq!(
        unconfigured.get(&good_oid).expect("good entry").validity,
        SignatureValidity::CannotCheck
    );
    assert_ne!(
        unconfigured.get(&good_oid).expect("good entry").validity,
        SignatureValidity::Bad
    );
    assert_eq!(
        unconfigured
            .get(&good_oid)
            .expect("good entry")
            .missing_hint
            .as_deref(),
        Some("Set gpg.ssh.allowedSignersFile to verify SSH signatures.")
    );

    // Now configure gpg.ssh.allowedSignersFile containing only key1.
    let allowed_signers = test.env_path().join("allowed_signers");
    fs::write(&allowed_signers, format!("{SIGNER_EMAIL} {pub1}\n")).expect("write allowedSigners");
    test.git(&[
        "config",
        "gpg.ssh.allowedSignersFile",
        allowed_signers.to_str().expect("allowed signers path"),
    ]);

    let hints_configured = git.signature_config_hints().expect("hints");
    assert!(hints_configured.ssh_allowed_signers_configured);
    assert!(hints_configured.ssh_allowed_signers_exists);

    let verified = git
        .verify_commit_signatures(&signed, &hints_configured, &|| false)
        .expect("verify with allowedSignersFile")
        .expect("not cancelled");

    let good_sig = verified.get(&good_oid).expect("good signature");
    assert_eq!(good_sig.format, SignatureFormat::Ssh);
    assert_eq!(good_sig.validity, SignatureValidity::Good);
    assert_eq!(good_sig.signer.as_deref(), Some(SIGNER_EMAIL));
    assert!(
        good_sig
            .key
            .as_deref()
            .is_some_and(|k| k.starts_with("SHA256:"))
    );
    assert!(good_sig.missing_hint.is_none());

    let unknown_sig = verified.get(&unknown_oid).expect("unknown key signature");
    assert_eq!(unknown_sig.format, SignatureFormat::Ssh);
    assert_eq!(unknown_sig.validity, SignatureValidity::UnknownValidity);
    assert!(unknown_sig.key.is_some());

    let bad_sig = verified.get(&bad_oid).expect("bad signature");
    assert_eq!(bad_sig.format, SignatureFormat::Ssh);
    assert_eq!(bad_sig.validity, SignatureValidity::Bad);
}

#[test]
fn session_caches_signatures_cancels_tickets_and_invalidates_on_reload() {
    let test = IsolatedRepo::new();
    let (key1, pub1) = test.create_ssh_key("id_ed25519");
    let allowed_signers = test.env_path().join("allowed_signers");
    fs::write(&allowed_signers, format!("{SIGNER_EMAIL} {pub1}\n")).expect("write allowedSigners");

    test.git(&["config", "gpg.format", "ssh"]);
    test.git(&[
        "config",
        "user.signingkey",
        key1.to_str().expect("key1 path"),
    ]);
    test.git(&[
        "config",
        "gpg.ssh.allowedSignersFile",
        allowed_signers.to_str().expect("allowed signers path"),
    ]);

    test.write("file.txt", "1\n");
    test.git(&["add", "-A"]);
    test.git(&["commit", "-q", "-m", "unsigned"]);
    let unsigned_oid = test.head();

    test.write("file.txt", "2\n");
    test.git(&["add", "-A"]);
    test.git(&["commit", "-q", "-S", "-m", "signed"]);
    let signed_oid = test.head();

    let (session, rx) = open_session(test.path());
    wait_for_snapshot(&session, &rx);

    let git_log_calls = Arc::new(AtomicUsize::new(0));
    let git_log_calls_hook = Arc::clone(&git_log_calls);
    session.git().set_log_subscriber(move |record| {
        if record.args.first().map(String::as_str) == Some("log") {
            git_log_calls_hook.fetch_add(1, Ordering::SeqCst);
        }
    });

    // First request runs gix detection + git log once.
    let ticket1 = session.request_commit_signatures(vec![unsigned_oid, signed_oid]);
    let (detected1, sigs1) = wait_for_signatures(&rx, ticket1);
    let detected1 = detected1.expect("CommitSignaturesDetected emitted for signed commit");
    assert_eq!(detected1.get(&signed_oid), Some(&SignatureFormat::Ssh));
    assert!(!detected1.contains_key(&unsigned_oid));
    assert_eq!(
        sigs1.get(&signed_oid).expect("signed sig").validity,
        SignatureValidity::Good
    );
    assert!(!sigs1.contains_key(&unsigned_oid));
    assert_eq!(git_log_calls.load(Ordering::SeqCst), 1);

    // Second request hits the session cache and does NOT spawn git log again.
    let ticket2 = session.request_commit_signatures(vec![unsigned_oid, signed_oid]);
    let (detected2, sigs2) = wait_for_signatures(&rx, ticket2);
    assert!(detected2.is_none(), "cached result needs no detected phase");
    assert_eq!(sigs2, sigs1);
    assert_eq!(git_log_calls.load(Ordering::SeqCst), 1);

    // Reload invalidates the signature cache and emits SignaturesInvalidated.
    session.reload();
    let deadline = Instant::now() + TIMEOUT;
    let mut saw_invalidated = false;
    while Instant::now() < deadline {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(500))
            && matches!(event, SessionEvent::SignaturesInvalidated)
        {
            saw_invalidated = true;
            break;
        }
    }
    assert!(saw_invalidated, "reload must emit SignaturesInvalidated");

    // Cancelling a ticket suppresses delivery of CommitSignaturesReady for it.
    let cancelled_ticket = session.request_commit_signatures(vec![signed_oid]);
    session.cancel_query(cancelled_ticket);
    let follow_up_ticket = session.request_commit_signatures(vec![signed_oid]);

    let received_tickets = Arc::new(Mutex::new(Vec::new()));
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(Instant::now());
        if let Ok(event) = rx.recv_timeout(rem)
            && let SessionEvent::CommitSignaturesReady { ticket, .. } = event
        {
            received_tickets.lock().expect("lock").push(ticket);
            if ticket == follow_up_ticket {
                break;
            }
        }
    }
    let received = received_tickets.lock().expect("lock");
    assert!(!received.contains(&cancelled_ticket));
    assert!(received.contains(&follow_up_ticket));
}

#[test]
fn openpgp_end_to_end_when_gpg_is_installed() {
    let Ok(gpg_check) = Command::new("gpg").arg("--version").output() else {
        eprintln!("gpg not available on PATH; skipping OpenPGP end-to-end test");
        return;
    };
    if !gpg_check.status.success() {
        eprintln!("gpg --version failed; skipping OpenPGP end-to-end test");
        return;
    }

    let test = IsolatedRepo::new();
    // gpg refuses a home directory others can read; Windows has no such mode bits.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let gnupg = test.gnupg_home();
        let mut perms = fs::metadata(&gnupg).expect("meta").permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&gnupg, perms).expect("chmod 700");
    }

    let batch = "\
        %echo Generating test key\n\
        Key-Type: eddsa\n\
        Key-Curve: ed25519\n\
        Name-Real: Lamina Test\n\
        Name-Email: signer@example.invalid\n\
        %no-protection\n\
        %commit\n";
    let mut keygen = test
        .command("gpg")
        .current_dir(test.gnupg_home())
        // The gpg Git for Windows ships runs under an MSYS runtime: it reads a
        // native `C:\...` home from the environment as a path relative to the
        // working directory. `--homedir .` is resolved from the real one.
        .args(["--homedir", ".", "--batch", "--gen-key"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn gpg");
    keygen
        .stdin
        .as_mut()
        .unwrap()
        .write_all(batch.as_bytes())
        .unwrap();
    let status = keygen.wait().expect("wait gpg");
    if !status.success() {
        // The gpg Git for Windows ships runs under an MSYS runtime and may be
        // unable to start its agent inside a test temp directory; the OpenPGP
        // path is still covered wherever key generation works.
        eprintln!("gpg --gen-key failed; skipping OpenPGP end-to-end test");
        return;
    }

    test.git(&["config", "gpg.format", "openpgp"]);
    test.git(&["config", "user.signingkey", SIGNER_EMAIL]);
    test.write("pgp.txt", "hello\n");
    test.git(&["add", "-A"]);
    test.git(&["commit", "-q", "-S", "-m", "pgp signed commit"]);
    let pgp_oid = test.head();

    let repo = test.repo();
    assert_eq!(
        repo.commit_signature_format(pgp_oid).expect("format"),
        Some(SignatureFormat::OpenPgp)
    );

    let git = test.git_handle();
    let res = git
        .verify_commit_signatures(
            &[(pgp_oid, SignatureFormat::OpenPgp)],
            &git.signature_config_hints().expect("hints"),
            &|| false,
        )
        .expect("verify")
        .expect("not cancelled");
    let sig = res.get(&pgp_oid).expect("pgp entry");
    assert_eq!(sig.format, SignatureFormat::OpenPgp);
    assert!(matches!(
        sig.validity,
        SignatureValidity::Good | SignatureValidity::UnknownValidity
    ));

    // Verify with an empty GNUPGHOME -> CannotCheck (E) because public key is missing.
    let empty_gnupg = test.env_path().join("empty_gnupg");
    fs::create_dir_all(&empty_gnupg).unwrap();
    let missing_key_git = test
        .git_handle()
        .with_env("GNUPGHOME", gnupg_env(&empty_gnupg));
    let res_missing = missing_key_git
        .verify_commit_signatures(
            &[(pgp_oid, SignatureFormat::OpenPgp)],
            &git.signature_config_hints().expect("hints"),
            &|| false,
        )
        .expect("verify missing key")
        .expect("not cancelled");
    assert_eq!(
        res_missing.get(&pgp_oid).expect("entry").validity,
        SignatureValidity::CannotCheck
    );
}

#[test]
fn gnupg_home_is_spelled_the_way_an_msys_gpg_mounts_it() {
    assert_eq!(msys_path("C:/Users/t/gnupg"), "/c/Users/t/gnupg");
    assert_eq!(msys_path("D:/Temp/gnupg"), "/d/Temp/gnupg");
    // Already a POSIX path: nothing to mount.
    assert_eq!(msys_path("/tmp/gnupg"), "/tmp/gnupg");
}
