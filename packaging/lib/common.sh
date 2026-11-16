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

# Linux musl triples the app knows how to upload. Keep this in step with
# `target_for` in crates/lamina-remote-client/src/deploy.rs.
lamina_remote_targets() {
    printf '%s\n' x86_64-unknown-linux-musl aarch64-unknown-linux-musl
}

# Prints the directory of prebuilt `lamina-remote-<target>` binaries, or
# nothing when this package should ship without them. `REMOTE_HELPERS` forces
# a directory. Otherwise an existing `remote-helpers/` at the repository root
# is used, which is where the release workflow downloads the helper artifact.
lamina_remote_helper_dir() {
    root=$1
    if [ -n "${REMOTE_HELPERS:-}" ]; then
        case $REMOTE_HELPERS in
        /*) printf '%s\n' "$REMOTE_HELPERS" ;;
        *) printf '%s\n' "$root/$REMOTE_HELPERS" ;;
        esac
        return 0
    fi
    if [ -d "$root/remote-helpers" ]; then
        printf '%s\n' "$root/remote-helpers"
    fi
}

# Copies both musl helpers into $2 and marks them executable. Fails when the
# directory is missing a target, so a release archive cannot ship half a pair.
lamina_install_remote_helpers() {
    src=$1
    dest=$2
    if [ ! -d "$src" ]; then
        echo "remote helper directory does not exist: $src" >&2
        return 1
    fi
    mkdir -p "$dest"
    chmod 755 "$dest"
    missing=0
    for target in $(lamina_remote_targets); do
        helper=$src/lamina-remote-$target
        if [ ! -f "$helper" ]; then
            echo "missing remote helper $helper" >&2
            missing=1
            continue
        fi
        cp "$helper" "$dest/lamina-remote-$target"
        chmod 755 "$dest/lamina-remote-$target"
    done
    if [ "$missing" = 1 ]; then
        return 1
    fi
}
