#!/bin/sh
# Builds scribecut and wraps it in a macOS .app bundle.
#
# The bundle is not about distribution: macOS hands out the Screen Recording
# and Microphone permissions to an *application*, so a bare binary run from a
# terminal gets the terminal's permissions (or silently records nothing). With
# a bundle, scribecut asks for its own and keeps them.
#
# Usage: scripts/bundle-macos.sh [--debug]
set -eu

profile=release
cargo_args="--release"
if [ "${1:-}" = "--debug" ]; then
    profile=debug
    cargo_args=""
fi

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

# shellcheck disable=SC2086 # cargo_args is intentionally word-split
cargo build $cargo_args

app="$root/target/scribecut.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>                 <string>scribecut</string>
    <key>CFBundleDisplayName</key>          <string>scribecut</string>
    <key>CFBundleIdentifier</key>           <string>org.scribecut.scribecut</string>
    <key>CFBundleExecutable</key>           <string>scribecut</string>
    <key>CFBundlePackageType</key>          <string>APPL</string>
    <key>CFBundleShortVersionString</key>   <string>0.1.0</string>
    <key>CFBundleVersion</key>              <string>0.1.0</string>
    <key>LSMinimumSystemVersion</key>       <string>11.0</string>
    <key>NSHighResolutionCapable</key>      <true/>
    <!-- Shown in the prompt when a recording first opens the microphone. -->
    <key>NSMicrophoneUsageDescription</key>
    <string>scribecut records your microphone alongside the screen so the video has narration to transcribe.</string>
</dict>
</plist>
PLIST

cp "$root/target/$profile/scribecut" "$app/Contents/MacOS/scribecut"

# Ad-hoc signature: the permission prompts need a signed app. Note that this
# identity changes whenever the binary does, so macOS may ask again after a
# rebuild — re-approving in System Settings is expected.
codesign --force --sign - --timestamp=none "$app" >/dev/null 2>&1 ||
    echo "warning: could not sign $app; permission prompts may not appear" >&2

echo "built $app"
echo "run it with:  open $app        (or: $app/Contents/MacOS/scribecut <video>)"
