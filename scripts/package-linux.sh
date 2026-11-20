#!/usr/bin/env bash
# Builds the Linux release packages into ./dist:
#   mavplayer_<ver>_amd64.deb, mavplayer-<ver>.x86_64.rpm, MavPlayer-<ver>-x86_64.AppImage
#
# Usage: scripts/package-linux.sh [deb] [rpm] [appimage]     (default: all three)
#
# Expects FFmpeg to be available for static linking (scripts/build-ffmpeg.sh)
# with PKG_CONFIG_PATH pointing at it. Set MAVPLAYER_DYNAMIC_FFMPEG=1 to link
# the system FFmpeg instead (the packages then need FFmpeg 9 on the target).
#
# Tools: cargo-deb, cargo-generate-rpm (cargo install …) and linuxdeploy
# (downloaded automatically if missing).
set -euo pipefail
cd "$(dirname "$0")/.."

ROOT=$(pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)
DIST=$ROOT/dist
mkdir -p "$DIST"
WANT=("$@")
[ ${#WANT[@]} -eq 0 ] && WANT=(deb rpm appimage)
want() { [[ " ${WANT[*]} " == *" $1 "* ]]; }

FEATURES=(--features static-ffmpeg)
[ "${MAVPLAYER_DYNAMIC_FFMPEG:-0}" = 1 ] && FEATURES=()

echo ">> building mavplayer $VERSION"
cargo build --release --locked "${FEATURES[@]}"
BIN=target/release/mavplayer

if want deb; then
    echo ">> .deb"
    cargo deb --no-build --no-strip --output "$DIST/"
fi

if want rpm; then
    echo ">> .rpm"
    cargo generate-rpm --output "$DIST/"
fi

if want appimage; then
    echo ">> AppImage"
    TOOLS=$ROOT/target/appimage-tools
    mkdir -p "$TOOLS"
    LINUXDEPLOY=${LINUXDEPLOY:-$(command -v linuxdeploy || true)}
    if [ -z "$LINUXDEPLOY" ]; then
        LINUXDEPLOY=$TOOLS/linuxdeploy-$ARCH.AppImage
        [ -x "$LINUXDEPLOY" ] || {
            curl -fL -o "$LINUXDEPLOY" "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$ARCH.AppImage"
            chmod +x "$LINUXDEPLOY"
        }
    fi
    APPDIR=$ROOT/target/AppDir
    rm -rf "$APPDIR"
    install -Dm755 "$BIN" "$APPDIR/usr/bin/mavplayer"
    install -Dm644 packaging/linux/io.github.drogobo.MavPlayer.metainfo.xml \
        "$APPDIR/usr/share/metainfo/io.github.drogobo.MavPlayer.metainfo.xml"
    for s in 16 32 48 64 128 256 512; do
        install -Dm644 "packaging/icons/mavplayer-$s.png" \
            "$APPDIR/usr/share/icons/hicolor/${s}x${s}/apps/io.github.drogobo.MavPlayer.png"
    done
    install -Dm644 packaging/linux/io.github.drogobo.MavPlayer.svg \
        "$APPDIR/usr/share/icons/hicolor/scalable/apps/io.github.drogobo.MavPlayer.svg"
    # AppRun: run the bundled binary. Desktop integration is built into the app
    # (first-run prompt, Media ▸ Desktop Integration, or `--integrate`).
    install -Dm755 packaging/appimage/AppRun "$APPDIR/AppRun"

    export APPIMAGE_EXTRACT_AND_RUN=1 # linuxdeploy without FUSE (CI containers)
    export ARCH VERSION
    export LDAI_OUTPUT="$DIST/MavPlayer-$VERSION-$ARCH.AppImage"
    export LDAI_UPDATE_INFORMATION="gh-releases-zsync|Drogobo|mavplayer|latest|MavPlayer-*-$ARCH.AppImage.zsync"
    # libpipewire must come from the host: a bundled copy looks for its
    # modules in the build machine's paths and fails to reach the server.
    "$LINUXDEPLOY" --appdir "$APPDIR" \
        --executable "$APPDIR/usr/bin/mavplayer" \
        --desktop-file packaging/linux/io.github.drogobo.MavPlayer.desktop \
        --icon-file packaging/linux/io.github.drogobo.MavPlayer.svg \
        --custom-apprun packaging/appimage/AppRun \
        --exclude-library 'libpipewire-0.3.so*' \
        --output appimage
fi

echo ">> packages in $DIST:"
ls -1 "$DIST"
