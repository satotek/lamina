//! `strata-remote`: serves one repository over stdin/stdout.
//!
//! The app starts this binary through `ssh -T host -- strata-remote --stdio`.
//! In that mode stdout must carry framed protocol messages only, so every
//! diagnostic below writes to stderr.

use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("strata-remote: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), strata_remote::BoxError> {
    let mut stdio = false;
    let mut self_check = false;
    let mut root = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--stdio" => stdio = true,
            "--self-check" => self_check = true,
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
            strata_remote_protocol::PROTOCOL_VERSION,
            strata_remote::build_id()
        );
        return Ok(());
    }

    if !stdio {
        print_usage();
        return Err("--stdio is required to serve a repository".into());
    }

    let root = strata_remote::parse_root(root.as_deref())?;
    strata_remote::serve(std::io::stdin(), std::io::stdout(), root)
}

fn print_usage() {
    eprintln!("usage: strata-remote --stdio [--root <path>]\n       strata-remote --self-check");
}
