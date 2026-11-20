# Helpers shared by the packaging scripts. This file is sourced, never run.
# shellcheck shell=sh
#
# The scripts read the version from Cargo.toml instead of `cargo metadata`
# because the release workflow compares a tag against the same field, and a
# packaging script should not need a resolved dependency graph (or network)
# just to learn the version.

# Prints the [workspace.package] version of the repository rooted at $1.
lamina_version() {
    sed -n '/^\[workspace\.package\]/,/^\[[^[]/p' "$1/Cargo.toml" |
        sed -n 's/^version *= *"\([^"]*\)".*/\1/p' |
        head -n 1
}

# Prints the directory holding the release binaries for the repository rooted
# at $1 and the target triple in $2 (which may be empty). CARGO_TARGET_DIR wins
# because the Nix dev shell points every worktree at one shared target
# directory, so `./target` is usually not where the binary is.
lamina_release_dir() {
    base=${CARGO_TARGET_DIR:-$1/target}
    if [ -n "${2:-}" ]; then
        printf '%s/%s/release\n' "$base" "$2"
    else
        printf '%s/release\n' "$base"
    fi
}

# Prints the short architecture name used in artifact file names.
lamina_arch() {
    case ${1:-$(uname -m)} in
    *aarch64* | *arm64*) echo arm64 ;;
    *x86_64* | *amd64* | *x64*) echo x64 ;;
    *) echo "${1:-$(uname -m)}" ;;
    esac
}
