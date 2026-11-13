#!/bin/sh
# Builds dist/Strata.app: the release binary, a bundle Info.plist and the
# checked-in Strata.icns, then signs it ad hoc so it launches on this Mac.
# Nothing here notarises or installs the app; copy it to /Applications if you
# want it in the Dock permanently.
#
# Run it from the dev shell so cargo and the release target directory match a
# normal build:
#
#     direnv exec . packaging/macos/bundle.sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
cd "$root"

app="$root/dist/Strata.app"
icon=crates/strata-app/resources/icon/Strata.icns
# The dev shell points cargo at one target directory shared by every worktree,
# so the binary is not necessarily under ./target. `cargo run` would find it,
# but this script copies the file and so has to resolve the path itself.
target_dir=${CARGO_TARGET_DIR:-$root/target}

command -v cargo >/dev/null 2>&1 || {
    echo "bundle.sh: cargo not found; run through the dev shell: direnv exec . $0" >&2
    exit 1
}
command -v codesign >/dev/null 2>&1 || {
    echo "bundle.sh: codesign not found; install the Xcode command line tools" >&2
    exit 1
}

cargo build --release -p strata-app

version=$(cargo metadata --no-deps --format-version 1 --quiet | python3 -c '
import json, sys
packages = json.load(sys.stdin)["packages"]
print(next(package["version"] for package in packages if package["name"] == "strata-app"))
')

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$target_dir/release/strata" "$app/Contents/MacOS/strata"
cp "$icon" "$app/Contents/Resources/Strata.icns"

# 14.0 matches the deployment target the Rust toolchain compiles for
# (`LC_BUILD_VERSION minos`); GPUI's Metal renderer needs a current SDK too.
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>Strata</string>
    <key>CFBundleExecutable</key>
    <string>strata</string>
    <key>CFBundleIconFile</key>
    <string>Strata.icns</string>
    <key>CFBundleIdentifier</key>
    <string>dev.strata.Strata</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Strata</string>
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

# Ad-hoc signature: Gatekeeper still treats the app as unsigned for downloads,
# but a signed bundle gets a stable identity, so TCC prompts and the Dock
# recognise it between launches.
codesign --force --sign - --timestamp=none "$app"

echo "built $app ($version)"
