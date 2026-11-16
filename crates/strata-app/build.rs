use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=resources/icon/strata.ico");

    emit_build_info();

    // GPUI's Windows backend gives every window the executable's icon
    // resource with ID 1, which is also what Explorer and the taskbar show.
    // The resource compiler only exists on a Windows host, and cross-compiling
    // the app to Windows is not supported, so the host check is enough.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("resources/icon/strata.ico")
            .compile()
            .expect("embed the application icon");
    }
}

/// Names the revision and the date the binary was built, for the About window.
///
/// CI can pass the revision it checked out because a release build may run
/// without a `.git` directory, and `SOURCE_DATE_EPOCH` makes the date
/// reproducible when a packager sets it.
fn emit_build_info() {
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let repository = manifest
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(manifest.as_path());

    watch_git(repository);
    println!(
        "cargo:rustc-env=STRATA_BUILD_COMMIT={}",
        build_commit(repository)
    );
    println!("cargo:rustc-env=STRATA_BUILD_DATE={}", build_date());
    println!("cargo:rerun-if-env-changed=STRATA_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

/// The commit the binary is built from: ten characters, `unknown` when there
/// is nothing to ask.
fn build_commit(repository: &Path) -> String {
    if let Ok(value) = std::env::var("STRATA_BUILD_COMMIT") {
        let short: String = value.trim().chars().take(10).collect();
        if !short.is_empty() {
            return short;
        }
    }
    Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
        .current_dir(repository)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|hash| hash.trim().to_owned())
        .filter(|hash| !hash.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The UTC build date as `YYYY-MM-DD`.
fn build_date() -> String {
    let seconds = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs() as i64)
                .unwrap_or(0)
        });
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Converts days since the Unix epoch to a Gregorian date (Howard Hinnant's
/// `civil_from_days`), so the build script needs no date crate.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Asks Cargo to rerun when HEAD or the ref it names changes, so a checkout or
/// a commit refreshes the embedded revision without a clean build.
fn watch_git(repository: &Path) {
    let Some(git_dir) = git_dir(&repository.join(".git")) else {
        return;
    };
    let head = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head.display());

    let common = common_dir(&git_dir);
    if let Ok(text) = std::fs::read_to_string(&head)
        && let Some(reference) = text.strip_prefix("ref: ").map(str::trim)
    {
        // A linked worktree keeps HEAD in its own git dir and the branch in
        // the common dir, so try both before falling back to packed refs.
        let loose = [git_dir.join(reference), common.join(reference)]
            .into_iter()
            .find(|path| path.exists());
        if let Some(loose) = loose {
            println!("cargo:rerun-if-changed={}", loose.display());
        }
    }
    let packed = common.join("packed-refs");
    if packed.exists() {
        println!("cargo:rerun-if-changed={}", packed.display());
    }
}

/// The git directory behind `.git`, which is a file in a linked worktree.
fn git_dir(dot_git: &Path) -> Option<PathBuf> {
    if dot_git.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    let text = std::fs::read_to_string(dot_git).ok()?;
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        dot_git.parent()?.join(target)
    })
}

/// The shared git directory of a worktree, named by its `commondir` file.
fn common_dir(git_dir: &Path) -> PathBuf {
    let Ok(text) = std::fs::read_to_string(git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    let target = Path::new(text.trim());
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        git_dir.join(target)
    }
}
