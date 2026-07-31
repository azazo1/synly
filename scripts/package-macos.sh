#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 0 ]]; then
    version="$(cargo metadata --locked --no-deps --format-version 1 | jq -er '.packages[] | select(.name == "synly") | .version')"
    target="$(rustc -vV | sed -n 's/^host: //p')"
    output_dir="dist"
    if [[ -x "target/$target/release/synly" ]]; then
        binary="target/$target/release/synly"
    else
        binary="target/release/synly"
    fi
elif [[ $# -eq 3 ]]; then
    version="$1"
    target="$2"
    output_dir="$3"
    binary="target/$target/release/synly"
else
    printf 'usage: %s [VERSION TARGET OUTPUT_DIR]\n' "$0" >&2
    exit 2
fi
app_bundle="$output_dir/Synly.app"
icon="assets/macos/synly.icns"
dmg_root="$output_dir/dmg-root"
case "$target" in
    x86_64-apple-darwin) arch="x86_64" ;;
    aarch64-apple-darwin) arch="aarch64" ;;
    *)
        printf 'unsupported macOS target: %s\n' "$target" >&2
        exit 1
        ;;
esac
dmg_path="$output_dir/synly-$version-macos-$arch.dmg"

if [[ ! -x "$binary" ]]; then
    printf 'missing executable: %s\n' "$binary" >&2
    exit 1
fi
if [[ ! -f "$icon" ]]; then
    printf 'missing macOS icon: %s\n' "$icon" >&2
    exit 1
fi

printf '[package] checking macOS binary for %s\n' "$target"
if ! file "$binary" | grep -q 'Mach-O'; then
    printf 'unexpected binary format: %s\n' "$binary" >&2
    exit 1
fi

printf '[package] assembling Synly.app\n'
mkdir -p "$output_dir"
rm -rf "$app_bundle"
mkdir -p "$app_bundle/Contents/MacOS" "$app_bundle/Contents/Resources"
cp "$binary" "$app_bundle/Contents/MacOS/synly"
chmod 755 "$app_bundle/Contents/MacOS/synly"
cp "$icon" "$app_bundle/Contents/Resources/synly.icns"

printf '%s\n' \
    '<?xml version="1.0" encoding="UTF-8"?>' \
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
    '<plist version="1.0">' \
    '<dict>' \
    '  <key>CFBundleDisplayName</key>' \
    '  <string>Synly</string>' \
    '  <key>CFBundleExecutable</key>' \
    '  <string>synly</string>' \
    '  <key>CFBundleIdentifier</key>' \
    '  <string>dev.azazo.synly</string>' \
    '  <key>CFBundleIconFile</key>' \
    '  <string>synly.icns</string>' \
    '  <key>CFBundleInfoDictionaryVersion</key>' \
    '  <string>6.0</string>' \
    '  <key>CFBundleName</key>' \
    '  <string>Synly</string>' \
    '  <key>CFBundlePackageType</key>' \
    '  <string>APPL</string>' \
    '  <key>CFBundleShortVersionString</key>' \
    "  <string>$version</string>" \
    '  <key>CFBundleVersion</key>' \
    "  <string>$version</string>" \
    '  <key>LSMinimumSystemVersion</key>' \
    '  <string>14.0</string>' \
    '  <key>NSHighResolutionCapable</key>' \
    '  <true/>' \
    '</dict>' \
    '</plist>' > "$app_bundle/Contents/Info.plist"

printf '[package] preparing DMG contents\n'
rm -rf "$dmg_root"
mkdir -p "$dmg_root"
cp -R "$app_bundle" "$dmg_root/Synly.app"
ln -s /Applications "$dmg_root/Applications"

printf '[package] creating %s\n' "$dmg_path"
rm -f "$dmg_path"
hdiutil create \
    -volname "Synly $version" \
    -srcfolder "$dmg_root" \
    -ov \
    -format UDZO \
    "$dmg_path" >/dev/null

test -f "$app_bundle/Contents/MacOS/synly"
test -f "$app_bundle/Contents/Info.plist"
test -f "$app_bundle/Contents/Resources/synly.icns"
test -L "$dmg_root/Applications"
test -f "$dmg_path"
printf '[package] completed %s\n' "$dmg_path"
