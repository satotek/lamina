//! `lamina-remote`: serves one repository over stdin/stdout.
//!
//! The app starts this binary through `ssh -T host -- lamina-remote --stdio`.
//! In that mode stdout must carry framed protocol messages only, so every
//! diagnostic below writes to stderr.

use std::process::ExitCode;

fn main() -> ExitCode {
    // git starts this binary as its askpass program while a fetch or push is
    // waiting on a credential. That mode has to answer before `--stdio`
    // parsing, and it must not open a repository or write the secret anywhere
    // but the helper's own stdout, which git reads.
    //
    // The server process is started by ssh, which already carries an askpass
    // channel for ssh's own prompts. Those variables are inherited, so a
    // `--stdio` invocation must not be read as a credential question.
    if !server_arguments()
        && let Some(code) = lamina_core::askpass::helper_main(std::env::args_os().skip(1))
    {
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lamina-remote: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), lamina_remote::BoxError> {
    let mut stdio = false;
    let mut self_check = false;
    let mut browse = false;
    let mut root = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--stdio" => stdio = true,
            "--self-check" => self_check = true,
            "--browse" => browse = true,
            "--root" => {
                let Some(value) = args.next() else {
                    return Err("--root requires a path".into());
                };
                root = Some(value);
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    if self_check {
        println!(
            "protocol: {}\nbuild: {}",
            lamina_remote_protocol::PROTOCOL_VERSION,
            lamina_remote::build_id()
        );
        return Ok(());
    }

    if !stdio {
        print_usage();
        return Err("--stdio is required to serve a repository".into());
    }

    if browse {
        return lamina_remote::serve_browse(std::io::stdin().lock(), std::io::stdout().lock());
    }

    let root = lamina_remote::parse_root(root.as_deref())?;
    lamina_remote::serve(std::io::stdin(), std::io::stdout(), root)
}

fn print_usage() {
    eprintln!(
        "usage: lamina-remote --stdio [--root <path>]\n       lamina-remote --stdio --browse\n       lamina-remote --self-check"
    );
}

/// True when argv is the repository server, not an askpass prompt.
///
/// `--root` may come first (`--root <path> --stdio`). A prompt git passes
/// never starts with one of these flags.
fn server_arguments() -> bool {
    matches!(
        std::env::args().nth(1).as_deref(),
        Some("--stdio" | "--browse" | "--self-check" | "--root" | "--help" | "-h")
    )
}
