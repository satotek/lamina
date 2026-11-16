#!/bin/sh
# Builds dist/Lamina.app: the release binary, a bundle Info.plist, the
# checked-in Lamina.icns and (when they were staged) the remote helpers, then
# signs it so it launches on this Mac. Without an identity the signature is ad
# hoc; the release workflow imports a self-signed certificate and passes its
# name so the app keeps one identity across releases. Nothing here notarises
# the app; dmg.sh turns the result into a disk image.
#
# Run it from the dev shell so cargo and the release target directory match a
# normal build:
#
#     direnv exec . packaging/macos/bundle.sh
#     packaging/macos/bundle.sh --target x86_64-apple-darwin
#     packaging/macos/bundle.sh --sign "Strata Code Signing"
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
. "$root/packaging/lib/common.sh"

usage() {
    cat <<'EOF'
Builds dist/Lamina.app: the release binary, a bundle Info.plist and the
checked-in Lamina.icns, then signs it so it launches on this Mac. Nothing here
notarises the app; dmg.sh turns the result into the disk image the release
publishes.

    direnv exec . packaging/macos/bundle.sh
    packaging/macos/bundle.sh --target x86_64-apple-darwin
    packaging/macos/bundle.sh --sign "Strata Code Signing"

Options:
    --target <triple>  build and bundle that target instead of the host
    --no-build         bundle a release binary that already exists
    --sign <identity>  sign with that identity instead of ad hoc

Environment:
    NOTICES              notices file to embed (default: dist/THIRD-PARTY-NOTICES.md)
    REMOTE_HELPERS       directory of lamina-remote-<target> binaries to embed.
                         When unset, remote-helpers/ at the repository root is
                         used if it exists. The Linux binaries cannot sit directly
                         under Contents/MacOS (codesign treats that as nested
                         code), so the bytes go in Contents/Resources and
                         Contents/MacOS/remote-helpers is a relative symlink.
    LAMINA_SIGNING_IDENTITY  identity to sign with when --sign is absent
EOF
}

target=
build=1
identity=
while [ $# -gt 0 ]; do
    case $1 in
    --target)
        target=${2:?--target needs a triple}
        shift 2
        ;;
    --no-build) build=0; shift ;;
    --sign)
        identity=${2:?--sign needs an identity}
        shift 2
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        echo "bundle.sh: unknown argument: $1" >&2
        exit 2
        ;;
    esac
done

# The flag wins so a caller can override an exported default.
identity=${identity:-${LAMINA_SIGNING_IDENTITY:-}}

cd "$root"

app="$root/dist/Lamina.app"
icon=crates/lamina-app/resources/icon/Lamina.icns
notices=${NOTICES:-$root/dist/THIRD-PARTY-NOTICES.md}
# Resolve the binary through CARGO_TARGET_DIR and the optional --target, because
# the dev shell points cargo at one target directory shared by every worktree
# and cross builds add a triple directory.
release=$(lamina_release_dir "$root" "$target")
version=$(lamina_version "$root")

if [ -z "$version" ]; then
    echo "bundle.sh: could not read [workspace.package] version from Cargo.toml" >&2
    exit 1
fi
if [ ! -f "$notices" ]; then
    echo "bundle.sh: $notices is missing; run packaging/third-party/generate.sh first" >&2
    exit 1
fi

if [ "$build" = 1 ]; then
    command -v cargo >/dev/null 2>&1 || {
        echo "bundle.sh: cargo not found; run through the dev shell: direnv exec . $0" >&2
        exit 1
    }
    set -- --release --locked -p lamina-app
    if [ -n "$target" ]; then
        set -- "$@" --target "$target"
    fi
    cargo build "$@"
fi

command -v codesign >/dev/null 2>&1 || {
    echo "bundle.sh: codesign not found; install the Xcode command line tools" >&2
    exit 1
}

if [ ! -f "$release/lamina" ]; then
    echo "bundle.sh: no binary at $release/lamina; build it or drop --no-build" >&2
    exit 1
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$release/lamina" "$app/Contents/MacOS/lamina"
cp "$icon" "$app/Contents/Resources/Lamina.icns"
# Licences travel inside the bundle; dmg.sh also copies them to the image root.
cp LICENSE-MIT LICENSE-APACHE "$app/Contents/Resources/"
cp "$notices" "$app/Contents/Resources/THIRD-PARTY-NOTICES.md"

# The app looks for remote-helpers/ beside its own executable. codesign treats
# every regular file under Contents/MacOS as nested code, and a Linux binary
# cannot take an ad-hoc signature, so the bytes live in Resources and the
# directory next to the executable is a relative symlink. Sign after this:
# the symlink has to be part of the bundle seal.
helpers=$(lamina_remote_helper_dir "$root")
if [ -n "$helpers" ]; then
    lamina_install_remote_helpers "$helpers" "$app/Contents/Resources/remote-helpers"
    ln -s ../Resources/remote-helpers "$app/Contents/MacOS/remote-helpers"
fi

# 14.0 matches the deployment target the Rust toolchain compiles for
# (`LC_BUILD_VERSION minos`); GPUI's Metal renderer needs a current SDK too.
cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>Lamina</string>
    <key>CFBundleExecutable</key>
    <string>lamina</string>
    <key>CFBundleIconFile</key>
    <string>Lamina.icns</string>
    <key>CFBundleIdentifier</key>
    <string>dev.lamina.Lamina</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Lamina</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
PLIST

# The bundle carries no nested code (no frameworks or helper apps), so one
# signature over the app seals everything.
#
# An ad-hoc signature is tied to the binary's hash, so macOS treats every
# release as a different app and the user's Keychain "Always Allow" choices and
# privacy grants reset on each update. A certificate identity produces a
# designated requirement naming the certificate instead, which stays the same
# between builds. Gatekeeper still treats either as unnotarised.
if [ -n "$identity" ]; then
    codesign --force --sign "$identity" --timestamp=none "$app"
    echo "signed $app with $identity"
else
    codesign --force --sign - --timestamp=none "$app"
fi

echo "built $app ($version)"
