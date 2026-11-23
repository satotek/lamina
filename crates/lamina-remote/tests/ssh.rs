//! `ssh` is a script on `PATH`'s behalf: it ignores the host and runs the
//! remote command locally. Nothing here contacts a real SSH server.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lamina_core::HeadInfo;
use lamina_core::askpass::{Answer, Prompt};
use lamina_core::session::RepositorySession;
use lamina_core::test_support::TestRepo;
use lamina_remote_client::{
    InstallOptions, RemoteBrowser, RemoteSession, RemoteTarget, SshOptions,
};

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("lamina-ssh-{}-{n}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch dir");
        Self { root }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Copy)]
enum Lie {
    Never,
    Once,
    Always,
}

struct Fake {
    os: &'static str,
    arch: &'static str,
    lie: Lie,
    /// Exported on the "remote" side as `LAMINA_REMOTE_HELPER`.
    override_path: Option<PathBuf>,
    /// Printed to stdout before every remote command, the way a login shell
    /// or its startup files might.
    banner: Option<&'static str>,
    /// Rewrites the upload's `cat` into `false`, so the transfer fails after
    /// creating the temp file.
    fail_upload: bool,
    /// Holds each install at its self-check and at its rename, so a test can
    /// make two installs overlap exactly.
    barrier: Option<Barrier>,
}

/// Release files the test touches to let a held command continue.
#[derive(Clone)]
struct Barrier {
    /// Released once every install has run its first self-check.
    checks: PathBuf,
    /// Released once every upload has finished writing its temp file.
    uploads: PathBuf,
}

#[derive(Clone)]
struct LaidOut {
    ssh: PathBuf,
    log: PathBuf,
    home: PathBuf,
    helpers: PathBuf,
}

fn layout(scratch: &Scratch, fake: &Fake) -> LaidOut {
    let bin = scratch.root.join("bin");
    let home = scratch.root.join("home");
    let helpers = scratch.root.join("helpers");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&helpers).unwrap();
    let log = scratch.root.join("ssh.log");
    let mark = scratch.root.join("lie-mark");

    let uname = format!(
        "#!/bin/sh\nif [ \"$1\" = \"-s\" ]; then\n  echo {os}\n  exit 0\nfi\nif [ \"$1\" = \"-m\" ]; then\n  echo {arch}\n  exit 0\nfi\necho unknown\nexit 0\n",
        os = fake.os,
        arch = fake.arch,
    );
    write_exec(&bin.join("uname"), &uname);

    let lie = match fake.lie {
        Lie::Never => String::new(),
        // The lie is a stale build of the current protocol. A protocol mismatch
        // is a different failure and would hide the build-id check these tests cover.
        Lie::Once => format!(
            "if [ ! -f \"$MARK\" ]; then\n  printf '%s\\n' 'protocol: {}' 'build: not-this-build'\n  touch \"$MARK\"\n  exit 0\nfi\n",
            lamina_remote_protocol::PROTOCOL_VERSION
        ),
        Lie::Always => format!(
            "printf '%s\\n' 'protocol: {}' 'build: not-this-build'\nexit 0\n",
            lamina_remote_protocol::PROTOCOL_VERSION
        ),
    };
    let override_line = match &fake.override_path {
        Some(path) => format!(
            "export LAMINA_REMOTE_HELPER={}",
            sh_quote(&path.display().to_string())
        ),
        None => String::new(),
    };
    let banner_line = match fake.banner {
        Some(text) => format!("printf '%s\\n' {}\n", sh_quote(text)),
        None => String::new(),
    };
    // The self-check case arm doubles as the arrival point for installs that
    // the test wants to hold back until every instance has decided to upload.
    let check_barrier = match &fake.barrier {
        Some(barrier) => format!(
            "printf 'x-check\\n' >> \"$LOG\"\nwhile [ ! -e {} ]; do sleep 0.1; done\n",
            sh_quote(&barrier.checks.display().to_string())
        ),
        None => String::new(),
    };
    // The upload script's `cat >` and `chmod 700` are rewritten textually:
    // the script runs in a fresh inner `sh`, so only in-script changes are
    // seen by it.
    let (fail_sed, upload_barrier, upload_exports) = match &fake.barrier {
        Some(barrier) => (
            if fake.fail_upload {
                "cmd=$(printf '%s\\n' \"$cmd\" | sed 's@cat >@false >@')\n".to_owned()
            } else {
                String::new()
            },
            // `$$` and quoting stay intact: only `chmod 700` is replaced, by
            // the marker write and the wait that precede it.
            "cmd=$(printf '%s\\n' \"$cmd\" | sed 's@chmod 700@printf x-cat >> $LAMINA_TEST_LOG; while [ ! -e $LAMINA_TEST_RELEASE ]; do sleep 0.1; done; chmod 700@')\n"
                .to_owned(),
            format!(
                "export LAMINA_TEST_LOG=$LOG\nexport LAMINA_TEST_RELEASE={}\n",
                sh_quote(&barrier.uploads.display().to_string())
            ),
        ),
        None => (
            if fake.fail_upload {
                "cmd=$(printf '%s\\n' \"$cmd\" | sed 's@cat >@false >@')\n".to_owned()
            } else {
                String::new()
            },
            String::new(),
            String::new(),
        ),
    };
    let upload_case = if fail_sed.is_empty() && upload_barrier.is_empty() {
        String::new()
    } else {
        format!("case \"$cmd\" in\n  *umask\\ 077*)\n{fail_sed}{upload_barrier}    ;;\nesac\n")
    };
    let script = format!(
        r#"#!/bin/sh
export HOME={home}
unset XDG_CACHE_HOME
export PATH={bin}:$PATH
{override_line}
LOG={log}
{upload_exports}MARK={mark}
{{
  printf 'SSH_ASKPASS=%s\n' "${{SSH_ASKPASS-}}"
  printf 'SSH_ASKPASS_REQUIRE=%s\n' "${{SSH_ASKPASS_REQUIRE-}}"
  printf 'LAMINA_ASKPASS_ENDPOINT=%s\n' "${{LAMINA_ASKPASS_ENDPOINT-}}"
  printf 'CMD'
  for arg in "$@"; do
    printf ' <%s>' "$arg"
  done
  printf '\n'
}} >> "$LOG"
after=0
cmd=
for arg in "$@"; do
  if [ "$after" = 1 ]; then
    cmd="$cmd $arg"
    continue
  fi
  if [ "$arg" = "--" ]; then
    after=1
  fi
done
cmd=${{cmd# }}
case "$cmd" in
  *--self-check*)
    {lie}
{check_barrier}    ;;
esac
{upload_case}if [ -z "$cmd" ]; then
  echo "fake ssh: no remote command" >&2
  exit 2
fi
{banner_line}exec sh -c "$cmd"
"#,
        home = sh_quote(&home.display().to_string()),
        bin = sh_quote(&bin.display().to_string()),
        log = sh_quote(&log.display().to_string()),
        mark = sh_quote(&mark.display().to_string()),
    );
    let ssh = bin.join("ssh");
    write_exec(&ssh, &script);
    LaidOut {
        ssh,
        log,
        home,
        helpers,
    }
}

fn write_exec(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

fn host_helper(dir: &Path) {
    let source = PathBuf::from(env!("CARGO_BIN_EXE_lamina-remote"));
    let dest = dir.join("lamina-remote-x86_64-unknown-linux-musl");
    fs::copy(&source, &dest).expect("copy helper");
    let mut perms = fs::metadata(&dest).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dest, perms).unwrap();
}

fn copy_exec(source: &Path, dest: &Path) {
    fs::copy(source, dest).unwrap();
    let mut perms = fs::metadata(dest).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(dest, perms).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn options(laid: &LaidOut) -> SshOptions {
    SshOptions {
        program: laid.ssh.clone(),
        helper_dir: Some(laid.helpers.clone()),
        askpass_program: Some(PathBuf::from("lamina-askpass-test")),
        askpass: Some(Arc::new(|_prompt: Prompt| -> Option<Answer> { None })),
    }
}

fn install(laid: &LaidOut, port: Option<u16>) -> Result<String, lamina_core::Error> {
    install_on(laid, "ignored.example", port)
}

fn install_on(laid: &LaidOut, host: &str, port: Option<u16>) -> Result<String, lamina_core::Error> {
    let mut opts = options(laid);
    lamina_remote_client::install_helper(&InstallOptions {
        program: opts.program.clone(),
        host: host.into(),
        port,
        helper_dir: opts.helper_dir.take(),
        askpass_program: opts.askpass_program.clone(),
        askpass: opts.askpass.clone(),
    })
}

/// Waits until the ssh log has grown `count` lines containing `needle`.
fn wait_for_lines(path: &Path, needle: &str, count: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && text.matches(needle).count() >= count
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {count} {needle:?} lines in {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn ssh_transport_reads_a_repository_and_installs_mode_0700() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    host_helper(&laid.helpers);

    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.commit("initial import");

    let (tx, rx) = std::sync::mpsc::channel();
    let session = RemoteSession::connect_ssh(
        RemoteTarget::new("ignored.example").with_port(2222),
        repo.path().display().to_string(),
        move |event| {
            let _ = tx.send(event);
        },
        options(&laid),
    )
    .expect("connect over fake ssh");
    let head = session.reader().head().expect("remote head");
    match head {
        HeadInfo::Branch { short_name, target } => {
            assert_eq!(short_name, "main");
            assert!(target.is_some());
        }
        other => panic!("head: {other:?}"),
    }
    drop(rx);
    drop(session);

    let log = fs::read_to_string(&laid.log).expect("ssh log");
    for needle in [
        "<-T>",
        "<ConnectTimeout=15>",
        "<ServerAliveInterval=15>",
        "<ServerAliveCountMax=3>",
        "<NumberOfPasswordPrompts=1>",
        "<-p>",
        "<2222>",
        "<--stdio>",
        "<--root>",
        "SSH_ASKPASS_REQUIRE=force",
    ] {
        assert!(log.contains(needle), "missing {needle} in {log}");
    }
    assert!(
        !log.contains("StrictHostKeyChecking"),
        "host-key checking must not be disabled: {log}"
    );
    assert!(
        log.contains("LAMINA_ASKPASS_ENDPOINT=") && !log.contains("LAMINA_ASKPASS_ENDPOINT=\n"),
        "askpass endpoint was not passed to ssh: {log}"
    );

    let installed = laid
        .home
        .join(".cache/lamina/remote")
        .join(env!("CARGO_PKG_VERSION"));
    let binary = installed.join("lamina-remote-x86_64-unknown-linux-musl");
    assert!(
        binary.is_file(),
        "helper was not installed at {}",
        binary.display()
    );
    assert_eq!(mode(&binary), 0o700, "helper file mode");
    assert_eq!(mode(&installed), 0o700, "version directory mode");
    assert_eq!(
        mode(&laid.home.join(".cache")),
        0o700,
        "cache directory mode"
    );
}

#[test]
fn a_build_id_mismatch_reinstalls_once() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Once,
            override_path: None,
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    host_helper(&laid.helpers);
    let path = install(&laid, None).expect("reinstall succeeds");
    assert!(Path::new(&path).is_file(), "{path}");
    let log = fs::read_to_string(&laid.log).unwrap();
    assert_eq!(
        log.matches("cat >").count(),
        1,
        "a helper answering for another build is replaced once: {log}"
    );
    let check = Command::new(&path)
        .arg("--self-check")
        .output()
        .expect("self-check the installed helper");
    assert!(check.status.success());
    let stdout = String::from_utf8(check.stdout).unwrap();
    assert!(!stdout.contains("not-this-build"), "{stdout}");
}

#[test]
fn a_current_helper_is_not_uploaded_again() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    host_helper(&laid.helpers);
    let first = install(&laid, None).expect("first install");
    let second = install(&laid, None).expect("reconnect");
    assert_eq!(first, second);
    let log = fs::read_to_string(&laid.log).unwrap();
    assert_eq!(
        log.matches("cat >").count(),
        1,
        "a reconnect reuses the installed helper: {log}"
    );
}

#[test]
fn a_repeated_build_id_mismatch_is_reported() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Always,
            override_path: None,
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    host_helper(&laid.helpers);
    let error = install(&laid, None).expect_err("second mismatch");
    let text = error.to_string();
    assert!(text.contains("not-this-build"), "{text}");
    assert!(text.contains("still failed after reinstall"), "{text}");
    let log = fs::read_to_string(&laid.log).unwrap();
    assert_eq!(
        log.matches("cat >").count(),
        2,
        "do not retry forever: {log}"
    );
}

#[test]
fn an_override_path_skips_the_upload() {
    let scratch = Scratch::new();
    let override_path = scratch.root.join("custom-helper");
    copy_exec(
        Path::new(env!("CARGO_BIN_EXE_lamina-remote")),
        &override_path,
    );
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: Some(override_path.clone()),
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    let installed = install(&laid, None).expect("override");
    assert_eq!(installed, override_path.display().to_string());
    let log = fs::read_to_string(&laid.log).unwrap();
    assert!(!log.contains("cat >"), "override must not upload: {log}");
}

#[test]
fn an_override_mismatch_does_not_delete_the_file() {
    let scratch = Scratch::new();
    let override_path = scratch.root.join("custom-helper");
    copy_exec(
        Path::new(env!("CARGO_BIN_EXE_lamina-remote")),
        &override_path,
    );
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Always,
            override_path: Some(override_path.clone()),
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    let error = install(&laid, None).expect_err("mismatch");
    let text = error.to_string();
    assert!(text.contains("not-this-build"), "{text}");
    assert!(override_path.is_file(), "the user's helper was deleted");
    let log = fs::read_to_string(&laid.log).unwrap();
    assert!(!log.contains("<rm>"), "override must not be removed: {log}");
}

#[test]
fn an_unwritable_cache_is_a_clear_error() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    host_helper(&laid.helpers);
    fs::create_dir_all(&laid.home).unwrap();
    fs::write(laid.home.join(".cache"), b"not a directory").unwrap();
    let error = install(&laid, None).expect_err("unwritable cache");
    let text = error.to_string();
    assert!(
        text.contains("could not install the remote helper"),
        "{text}"
    );
}

#[test]
fn a_missing_local_binary_names_the_file() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    let error = install(&laid, None).expect_err("missing binary");
    let text = error.to_string();
    assert!(
        text.contains("lamina-remote-x86_64-unknown-linux-musl"),
        "{text}"
    );
    assert!(text.contains("LAMINA_REMOTE_HELPER_DIR"), "{text}");
}

#[test]
fn an_unsupported_remote_is_refused() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Darwin",
            arch: "arm64",
            lie: Lie::Never,
            override_path: None,
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    let error = install(&laid, None).expect_err("darwin");
    let text = error.to_string();
    assert!(text.contains("not available"), "{text}");
    assert!(text.contains("Darwin"), "{text}");
}

#[test]
fn a_browse_helper_walks_folders_on_the_host() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    host_helper(&laid.helpers);
    fs::create_dir_all(laid.home.join("src/app/.git")).unwrap();
    fs::create_dir_all(laid.home.join("src/notes")).unwrap();
    fs::write(laid.home.join("src/readme.txt"), "not a folder\n").unwrap();

    let helper = install(&laid, None).expect("install helper");
    let target = RemoteTarget::new("ignored.example");
    let browser = RemoteBrowser::open(&target, helper, &options(&laid)).expect("open browser");
    let home = fs::canonicalize(&laid.home).unwrap();
    assert_eq!(browser.home(), laid.home.display().to_string());

    let start = browser.list(None).expect("list home");
    assert_eq!(start.path, home.display().to_string());
    assert!(
        start.entries.iter().any(|entry| entry.name == "src"),
        "{start:?}"
    );

    let src = browser.list(Some("~/src")).expect("list src");
    let names: Vec<_> = src
        .entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.repository))
        .collect();
    assert_eq!(names, [("app", true), ("notes", false)]);
    assert_eq!(src.parent, Some(home.display().to_string()));

    let app = browser
        .list(Some(&format!("{}/app", src.path)))
        .expect("list app");
    assert!(app.repository);

    let missing = browser.list(Some("~/nope")).expect_err("missing folder");
    assert!(missing.to_string().contains("nope"), "{missing}");
    // The connection survives a failed listing.
    browser.list(None).expect("list after an error");

    let log = fs::read_to_string(&laid.log).expect("ssh log");
    assert!(log.contains("<--browse>"), "{log}");
}

#[test]
fn a_host_beginning_with_a_dash_is_refused_before_ssh_runs() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
            banner: None,
            fail_upload: false,
            barrier: None,
        },
    );
    host_helper(&laid.helpers);
    let error = install_on(&laid, "-oProxyCommand=touch /tmp/pwned", None)
        .expect_err("a dash host is not a destination");
    let text = error.to_string();
    assert!(text.contains("may not begin with '-'"), "{text}");
    assert!(
        !laid.log.exists(),
        "ssh ran anyway and option-parsed the host: {}",
        fs::read_to_string(&laid.log).unwrap_or_default()
    );
}

#[test]
fn a_login_shell_banner_does_not_confuse_the_probe() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
            banner: Some("Welcome to the shell of the day"),
            fail_upload: false,
            barrier: None,
        },
    );
    host_helper(&laid.helpers);
    // A positional probe would read the banner as the os and shift every
    // later value, turning `x86_64` into the helper override path.
    let installed = install(&laid, None).expect("install past the banner");
    assert!(Path::new(&installed).is_file(), "{installed}");
}
