#!/bin/sh
# Writes the third-party notices file that ships in every release artifact:
# the Rust crate licences collected by cargo-about, followed by the Zig-native
# dependencies that crates/lamina-term/build.rs compiles into libghostty-vt.
#
#     packaging/third-party/generate.sh dist/THIRD-PARTY-NOTICES.md
#
# Needs cargo-about on PATH (`nix shell nixpkgs#cargo-about --command ...`, or
# `cargo install cargo-about`). Release workflows generate the file once per
# build job so the notices always match the lockfile that was compiled.
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)

usage() {
    cat <<'EOF'
Writes the third-party notices file that ships in every release artifact: the
Rust crate licences collected by cargo-about, followed by the Zig-native
dependencies that crates/lamina-term/build.rs compiles into libghostty-vt.

    packaging/third-party/generate.sh dist/THIRD-PARTY-NOTICES.md

Needs cargo-about on PATH (`nix shell nixpkgs#cargo-about`, or
`cargo install cargo-about`); CARGO_ABOUT overrides the binary name.
EOF
}

case ${1:-} in
-h | --help)
    usage
    exit 0
    ;;
'')
    usage >&2
    exit 2
    ;;
esac
out=$1

cargo_about=${CARGO_ABOUT:-cargo-about}
if ! command -v "$cargo_about" >/dev/null 2>&1; then
    echo "generate.sh: cargo-about not found on PATH." >&2
    echo "generate.sh: try 'nix shell nixpkgs#cargo-about --command $0 $out'," >&2
    echo "generate.sh: or 'cargo install cargo-about'." >&2
    exit 1
fi

case $out in
/*) ;;
*) out=$root/$out ;;
esac
mkdir -p "$(dirname "$out")"

# cargo-about resolves the workspace from the working directory, so run it from
# the repository root. --locked keeps the notices in step with Cargo.lock.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

cd "$root"
"$cargo_about" generate --locked --config about.toml about.hbs -o "$tmp/rust.md"

cat "$tmp/rust.md" packaging/third-party/libghostty-vt.md packaging/third-party/catppuccin.md > "$tmp/notices.md"
mv "$tmp/notices.md" "$out"

echo "wrote $out"
