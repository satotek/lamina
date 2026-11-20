use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

const LIBGHOSTTY_VT_VERSION: &str = "1.0.1";
const REQUIRED_ZIG_VERSION: &str = "0.17.1";

const SRC_URL: &str =
    "https://release.files.ghostty.org/libghostty-vt/1.0.1/libghostty-vt-1.0.1.tar.gz";
const SRC_SHA256: &str = "333dfce97641ef9b536bb9e08907ad86abaaada50e8f7417b70d9284adef37aa";

const DEPS_URL: &str =
    "https://release.files.ghostty.org/libghostty-vt/1.0.1/libghostty-vt-1.0.1-deps.tar.gz";
const DEPS_SHA256: &str = "91a4e10960a52bf5bf4b15aa2a2cddaff8a89d6cd903d3055bda4fd17de3356d";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_LIB_DIR");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_ZIG_SYSTEM_DIR");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=ZIG");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let lib_filename = static_lib_filename(&target_os);

    // Allow Nix or offline builds to supply a prebuilt static archive directly.
    if let Ok(lib_dir) = env::var("LIBGHOSTTY_VT_LIB_DIR") {
        let lib_path = PathBuf::from(&lib_dir);
        if !lib_path.is_dir() {
            panic!(
                "LIBGHOSTTY_VT_LIB_DIR does not exist or is not a directory: {}",
                lib_path.display()
            );
        }
        let archive = lib_path.join(lib_filename);
        if !archive.is_file() {
            panic!(
                "LIBGHOSTTY_VT_LIB_DIR ({}) does not contain expected static archive `{lib_filename}`",
                lib_path.display()
            );
        }
        emit_link_directives(&lib_path, &target_os);
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let prefix_dir = out_dir.join("ghostty-vt-prefix");
    let lib_dir = prefix_dir.join("lib");
    let optimize = env::var("LIBGHOSTTY_VT_OPTIMIZE").unwrap_or_else(|_| "ReleaseFast".to_string());
    let target = env::var("TARGET").unwrap_or_default();
    let host = env::var("HOST").unwrap_or_default();

    let expected_lib = lib_dir.join(lib_filename);
    let stamp_path = out_dir.join("ghostty-vt-build.stamp");
    let expected_stamp = format!("{LIBGHOSTTY_VT_VERSION}:{optimize}:{target}");

    if expected_lib.is_file()
        && fs::read_to_string(&stamp_path).ok().as_deref() == Some(expected_stamp.as_str())
    {
        emit_link_directives(&lib_dir, &target_os);
        return;
    }

    let source_override = env::var("LIBGHOSTTY_VT_SOURCE_DIR").ok();
    let deps_override = env::var("LIBGHOSTTY_VT_ZIG_SYSTEM_DIR").ok();
    let cache_root = cargo_target_dir(&out_dir)
        .map(|target_dir| target_dir.join("ghostty-vt-cache"))
        .unwrap_or_else(|| out_dir.clone());
    let prebuilt_lib = cache_root
        .join("prebuilt")
        .join(format!("{LIBGHOSTTY_VT_VERSION}-{optimize}-{target}"))
        .join(lib_filename);

    if source_override.is_none() && deps_override.is_none() && prebuilt_lib.is_file() {
        fs::create_dir_all(&lib_dir).expect("create lib dir");
        fs::copy(&prebuilt_lib, &expected_lib).expect("copy cached libghostty-vt archive");
        fs::write(&stamp_path, &expected_stamp).expect("write build stamp");
        emit_link_directives(&lib_dir, &target_os);
        return;
    }

    // Verify Zig before downloading/unpacking so missing or mismatched toolchains fail fast.
    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".to_string());
    verify_zig_version(&zig);

    let downloads_dir = cache_root.join("downloads");
    let work_dir = out_dir.join("ghostty-vt-work");
    fs::create_dir_all(&downloads_dir).expect("create downloads dir");
    fs::create_dir_all(&work_dir).expect("create work dir");

    let source_dir = match source_override.as_deref() {
        Some(dir) => {
            let path = PathBuf::from(dir);
            assert!(
                path.join("build.zig").is_file(),
                "LIBGHOSTTY_VT_SOURCE_DIR ({}) does not contain build.zig",
                path.display()
            );
            path
        }
        None => prepare_tarball(
            SRC_URL,
            SRC_SHA256,
            &downloads_dir.join("libghostty-vt-1.0.1.tar.gz"),
            &work_dir.join("src"),
            "libghostty-vt-1.0.1",
        ),
    };

    let zig_system_dir = match deps_override.as_deref() {
        Some(dir) => {
            let path = PathBuf::from(dir);
            assert!(
                path.is_dir(),
                "LIBGHOSTTY_VT_ZIG_SYSTEM_DIR ({}) does not exist or is not a directory",
                path.display()
            );
            path
        }
        None => prepare_zig_deps(
            DEPS_URL,
            DEPS_SHA256,
            &downloads_dir.join("libghostty-vt-1.0.1-deps.tar.gz"),
            &work_dir.join("zig-deps"),
        ),
    };

    // Use an isolated Zig cache under OUT_DIR so we never collide with a stale
    // user-global Zig cache or trigger Zig 0.17's archive-cache bug.
    let zig_global_cache = out_dir.join("zig-global-cache");
    let zig_local_cache = out_dir.join("zig-local-cache");
    fs::create_dir_all(&zig_global_cache).expect("create zig global cache dir");
    fs::create_dir_all(&zig_local_cache).expect("create zig local cache dir");

    let mut cmd = Command::new(&zig);
    cmd.current_dir(&source_dir)
        .env("ZIG_GLOBAL_CACHE_DIR", &zig_global_cache)
        .env("ZIG_LOCAL_CACHE_DIR", &zig_local_cache)
        // The extracted tarball has no `.git`, so without a ceiling git walks up
        // into whatever repository holds the target directory (Lamina's own).
        // A tag there, such as a release tag on CI, makes the version check in
        // libghostty's build.zig panic because it does not match 1.0.1.
        .env(
            "GIT_CEILING_DIRECTORIES",
            source_dir.parent().unwrap_or(&source_dir),
        )
        .arg("build")
        .arg("-Demit-lib-vt")
        .arg(format!("-Dlib-version-string={LIBGHOSTTY_VT_VERSION}"))
        .arg("--prefix")
        .arg(&prefix_dir)
        .arg("--system")
        .arg(&zig_system_dir)
        .arg(format!("-Doptimize={optimize}"))
        .arg("-Dcpu=baseline");

    if host != target
        && let Some(zig_target) = cargo_target_to_zig(&target)
    {
        cmd.arg(format!("-Dtarget={zig_target}"));
    }

    let output = cmd.output().unwrap_or_else(|err| {
        panic!(
            "failed to invoke `{zig}` to build libghostty-vt {LIBGHOSTTY_VT_VERSION}: {err}\n\
             Required Zig version: {REQUIRED_ZIG_VERSION}.\n\
             Overrides: ZIG=/path/to/zig, LIBGHOSTTY_VT_LIB_DIR, LIBGHOSTTY_VT_SOURCE_DIR, \
             LIBGHOSTTY_VT_ZIG_SYSTEM_DIR."
        )
    });
    if !output.status.success() {
        panic!(
            "`zig build -Demit-lib-vt` failed ({}):\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    if !expected_lib.is_file() {
        panic!(
            "libghostty-vt static archive was not produced at {}",
            expected_lib.display()
        );
    }

    if source_override.is_none()
        && deps_override.is_none()
        && let Some(prebuilt_parent) = prebuilt_lib.parent()
    {
        fs::create_dir_all(prebuilt_parent).expect("create prebuilt cache dir");
        if !prebuilt_lib.is_file() {
            let tmp_prebuilt =
                prebuilt_lib.with_file_name(format!("{lib_filename}.{}.part", std::process::id()));
            fs::copy(&expected_lib, &tmp_prebuilt).expect("populate prebuilt cache");
            if let Err(err) = fs::rename(&tmp_prebuilt, &prebuilt_lib) {
                if !prebuilt_lib.is_file() {
                    panic!("commit prebuilt cache failed: {err}");
                }
                let _ = fs::remove_file(&tmp_prebuilt);
            }
        }
    }

    fs::write(&stamp_path, expected_stamp).expect("write build stamp");
    emit_link_directives(&lib_dir, &target_os);
}

fn verify_zig_version(zig: &str) {
    let output = Command::new(zig)
        .arg("version")
        .output()
        .unwrap_or_else(|err| {
            panic!(
                "Zig {REQUIRED_ZIG_VERSION} is required to build libghostty-vt {LIBGHOSTTY_VT_VERSION}, \
                 but `{zig}` could not be executed: {err}\n\
                 Install Zig {REQUIRED_ZIG_VERSION} (or enter the Nix dev shell via `direnv exec .`), \
                 set `ZIG=/path/to/zig`, or provide a prebuilt library via `LIBGHOSTTY_VT_LIB_DIR` \
                 (offline source/deps overrides: `LIBGHOSTTY_VT_SOURCE_DIR`, `LIBGHOSTTY_VT_ZIG_SYSTEM_DIR`)."
            )
        });

    if !output.status.success() {
        panic!(
            "Zig {REQUIRED_ZIG_VERSION} is required to build libghostty-vt {LIBGHOSTTY_VT_VERSION}, \
             but `{zig} version` exited with {}.\n\
             stderr: {}\n\
             Set `ZIG=/path/to/zig` or provide a prebuilt library via `LIBGHOSTTY_VT_LIB_DIR`.",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let matches = found == REQUIRED_ZIG_VERSION
        || found.starts_with(&format!("{REQUIRED_ZIG_VERSION}+"))
        || found.starts_with(&format!("{REQUIRED_ZIG_VERSION}-"));

    if !matches {
        panic!(
            "Zig {REQUIRED_ZIG_VERSION} is required to build libghostty-vt {LIBGHOSTTY_VT_VERSION}, \
             but `{zig} version` reported `{found}`.\n\
             Install Zig {REQUIRED_ZIG_VERSION} (or enter the Nix dev shell), set `ZIG=/path/to/zig`, \
             or provide a prebuilt library via `LIBGHOSTTY_VT_LIB_DIR` \
             (offline source/deps overrides: `LIBGHOSTTY_VT_SOURCE_DIR`, `LIBGHOSTTY_VT_ZIG_SYSTEM_DIR`)."
        );
    }
}

fn static_lib_filename(target_os: &str) -> &'static str {
    if target_os == "windows" {
        "ghostty-vt-static.lib"
    } else {
        "libghostty-vt.a"
    }
}

fn emit_link_directives(lib_dir: &Path, target_os: &str) {
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=static=ghostty-vt-static");
        println!("cargo:rustc-link-lib=dylib=ntdll");
        println!("cargo:rustc-link-lib=dylib=kernel32");
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
}

fn prepare_tarball(
    url: &str,
    expected_sha256: &str,
    archive_path: &Path,
    extract_root: &Path,
    inner_dir_name: &str,
) -> PathBuf {
    let target_dir = extract_root.join(inner_dir_name);
    if target_dir.join("build.zig").is_file() {
        return target_dir;
    }
    ensure_downloaded(url, expected_sha256, archive_path);
    if extract_root.exists() {
        fs::remove_dir_all(extract_root).expect("clean stale source dir");
    }
    fs::create_dir_all(extract_root).expect("create source extract dir");
    unpack_targz(archive_path, extract_root);
    assert!(
        target_dir.join("build.zig").is_file(),
        "extracted archive did not contain {}/build.zig",
        target_dir.display()
    );
    target_dir
}

fn prepare_zig_deps(
    url: &str,
    expected_sha256: &str,
    archive_path: &Path,
    extract_root: &Path,
) -> PathBuf {
    let deps_dir = extract_root.join("libghostty-vt-1.0.1-deps");
    if deps_dir.is_dir() {
        return deps_dir;
    }
    ensure_downloaded(url, expected_sha256, archive_path);
    if extract_root.exists() {
        fs::remove_dir_all(extract_root).expect("clean stale deps dir");
    }
    fs::create_dir_all(extract_root).expect("create deps extract dir");
    unpack_targz(archive_path, extract_root);
    assert!(
        deps_dir.is_dir(),
        "extracted deps archive did not contain {}",
        deps_dir.display()
    );
    deps_dir
}

fn cargo_target_dir(out_dir: &Path) -> Option<PathBuf> {
    if let Some(dir) = env::var_os("CARGO_TARGET_DIR") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    let pkg_dir = out_dir.parent()?;
    let build_dir = pkg_dir.parent()?;
    if build_dir.file_name()?.to_str()? != "build" {
        return None;
    }
    let profile_dir = build_dir.parent()?;
    profile_dir.parent().map(Path::to_path_buf)
}

fn ensure_downloaded(url: &str, expected_sha256: &str, dest: &Path) {
    if dest.is_file() {
        if sha256_file(dest).ok().as_deref() == Some(expected_sha256) {
            return;
        }
        // Remove corrupted or stale archive before re-downloading (required on Windows before rename).
        let _ = fs::remove_file(dest);
    }

    let curl_bin = if cfg!(windows) { "curl.exe" } else { "curl" };
    let dest_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive.tar.gz");
    let tmp = dest.with_file_name(format!("{dest_name}.{}.part", std::process::id()));

    let status = Command::new(curl_bin)
        .args([
            "-fsSL",
            "--retry",
            "5",
            "--retry-delay",
            "2",
            "--connect-timeout",
            "30",
            "-o",
        ])
        .arg(&tmp)
        .arg(url)
        .status()
        .unwrap_or_else(|err| {
            panic!(
                "failed to run `{curl_bin}` to fetch {url}: {err}\n\
                 For offline builds, set LIBGHOSTTY_VT_LIB_DIR or both \
                 LIBGHOSTTY_VT_SOURCE_DIR and LIBGHOSTTY_VT_ZIG_SYSTEM_DIR."
            )
        });
    if !status.success() {
        let _ = fs::remove_file(&tmp);
        panic!("`{curl_bin}` failed downloading {url} with status {status}");
    }

    let actual = sha256_file(&tmp).expect("hash downloaded tarball");
    if actual != expected_sha256 {
        let _ = fs::remove_file(&tmp);
        panic!("SHA-256 mismatch for {url}: expected {expected_sha256}, got {actual}");
    }

    if dest.is_file() && sha256_file(dest).ok().as_deref() == Some(expected_sha256) {
        let _ = fs::remove_file(&tmp);
        return;
    }
    let _ = fs::remove_file(dest);
    fs::rename(&tmp, dest).expect("move verified tarball into place");
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn unpack_targz(archive_path: &Path, dest: &Path) {
    let file = File::open(archive_path).expect("open archive");
    let gz = GzDecoder::new(file);
    let mut archive = Archive::new(gz);
    archive.unpack(dest).expect("unpack tar.gz archive");
}

fn cargo_target_to_zig(target: &str) -> Option<&'static str> {
    match target {
        "aarch64-apple-darwin" => Some("aarch64-macos"),
        "x86_64-apple-darwin" => Some("x86_64-macos"),
        "x86_64-unknown-linux-gnu" => Some("x86_64-linux-gnu"),
        "aarch64-unknown-linux-gnu" => Some("aarch64-linux-gnu"),
        "x86_64-pc-windows-msvc" => Some("x86_64-windows-msvc"),
        "aarch64-pc-windows-msvc" => Some("aarch64-windows-msvc"),
        _ => None,
    }
}
