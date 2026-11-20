#!/usr/bin/env bash
# Installs newer SPA (PipeWire's Simple Plugin API) headers for building on
# distributions with an old PipeWire, such as Ubuntu 22.04 (0.3.48), which the
# portable Linux packages are built on for the oldest glibc.
#
# The `libspa` crate (0.10) uses SPA functions and fields newer than 0.3.48.
# SPA is header-only: its inline functions are compiled into MavPlayer, and the
# audio structures it uses are unchanged since 0.3.x, so the binary still runs
# against any PipeWire 0.3.44 or later. Only libspa-0.2 is replaced; the system
# libpipewire-0.3 is still what gets linked.
#
# Usage:  scripts/spa-headers.sh            (installs into spa-headers/)
#         export PKG_CONFIG_PATH=$PWD/spa-headers/lib/pkgconfig:$PKG_CONFIG_PATH
set -euo pipefail

VERSION=1.2.7
COMMIT=cc7439187f61dd73b81ca69f5dbccbb52ce970b2   # the 1.2.7 tag
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PREFIX=${1:-$ROOT/spa-headers}

if [ -f "$PREFIX/lib/pkgconfig/libspa-0.2.pc" ] && grep -q "Version: $VERSION" "$PREFIX/lib/pkgconfig/libspa-0.2.pc"; then
    echo "SPA $VERSION headers already in $PREFIX"
    exit 0
fi

SRC=$(mktemp -d)
trap 'rm -rf "$SRC"' EXIT
git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$VERSION" \
    https://gitlab.freedesktop.org/pipewire/pipewire.git "$SRC/pipewire"
got=$(git -C "$SRC/pipewire" rev-parse HEAD)
if [ "$got" != "$COMMIT" ]; then
    echo "error: PipeWire tag $VERSION is $got, expected $COMMIT" >&2
    exit 1
fi

rm -rf "$PREFIX"
mkdir -p "$PREFIX/include/spa-0.2" "$PREFIX/lib/pkgconfig"
cp -r "$SRC/pipewire/spa/include/spa" "$PREFIX/include/spa-0.2/"
cat > "$PREFIX/lib/pkgconfig/libspa-0.2.pc" <<EOF
prefix=$PREFIX
includedir=\${prefix}/include

Name: libspa
Description: Simple Plugin API (headers from PipeWire $VERSION)
Version: $VERSION
Cflags: -I\${includedir}/spa-0.2 -D_REENTRANT
EOF
echo "SPA $VERSION headers installed in $PREFIX"
