#!/usr/bin/env bash
# Builds dist/MavPlayer-<ver>-macos-<arch>.dmg containing MavPlayer.app.
#
# FFmpeg is linked statically (run scripts/build-ffmpeg.sh first and point
# PKG_CONFIG_PATH at it), so the app has no dependencies beyond macOS itself.
# Set CODESIGN_IDENTITY to sign with a Developer ID; otherwise the app is
# ad-hoc signed (users then right-click ▸ Open the first time).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)
DIST=dist
APP=$DIST/MavPlayer.app
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}

cargo build --release --locked --features static-ffmpeg

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/mavplayer "$APP/Contents/MacOS/mavplayer"
cp packaging/macos/MavPlayer.icns "$APP/Contents/Resources/MavPlayer.icns"
sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
cp LICENSE README.md "$APP/Contents/Resources/"
cp third_party/shaders/LICENSES.txt "$APP/Contents/Resources/SHADER-LICENSES.txt"

# Anything not from the OS must have been linked statically.
if otool -L "$APP/Contents/MacOS/mavplayer" | tail -n +2 | grep -Ev '/System/Library/|/usr/lib/'; then
    echo "error: the binary links non-system libraries (see above)" >&2
    exit 1
fi

codesign --force --deep --options runtime --timestamp=none \
    --sign "${CODESIGN_IDENTITY:--}" "$APP"

DMG=$DIST/MavPlayer-$VERSION-macos-$ARCH.dmg
rm -f "$DMG"
STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "MavPlayer $VERSION" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"
echo ">> $DMG"
