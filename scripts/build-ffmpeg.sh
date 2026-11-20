#!/usr/bin/env bash
# Builds the FFmpeg that release builds of MavPlayer link statically, plus the
# libraries it needs (dav1d, zimg, libdvdread, libdvdnav, nv-codec-headers, and
# on Linux libplacebo with glslang for the GPU upscalers).
#
# Usage: scripts/build-ffmpeg.sh [PREFIX]          (default: ./ffmpeg-build/prefix)
#
# Then build MavPlayer against it with:
#   export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
#   cargo build --release --features static-ffmpeg
#
# Works on Linux and macOS. Needs: a C compiler, make, nasm, meson, ninja,
# pkg-config, curl, autoconf/automake/libtool, and on Linux also a C++20
# compiler, git, cmake, python3, meson >= 1.3 and the OpenSSL / zlib / libva
# development headers (see HACKING.md).
set -euo pipefail

FFMPEG_VERSION=9.0.2
DAV1D_VERSION=1.5.1
ZIMG_VERSION=3.0.6
DVDREAD_VERSION=7.1.1
DVDNAV_VERSION=7.0.0
NVHEADERS_VERSION=n13.0.19.0
LIBPLACEBO_VERSION=7.387.0
GLSLANG_VERSION=vulkan-sdk-1.4.363.0

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${FFMPEG_WORK_DIR:-$ROOT/ffmpeg-build}
PREFIX=$(mkdir -p "${1:-$WORK/prefix}" && cd "${1:-$WORK/prefix}" && pwd)
SRC=$WORK/src
mkdir -p "$SRC"
OS=$(uname -s)
JOBS=$( (nproc || sysctl -n hw.ncpu) 2>/dev/null || echo 4)

export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:$PREFIX/lib64/pkgconfig:$PREFIX/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export CFLAGS="-O2 -fPIC ${CFLAGS:-}"
if [ "$OS" = Darwin ]; then
    # Everything here is built for this macOS version, so the app runs there too.
    export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
fi

fetch() { # url dir
    local url=$1 dir=$2 file
    file="$SRC/$(basename "$url")"
    [ -d "$SRC/$dir" ] && return
    echo ">> downloading $url"
    curl -fL --retry 3 -o "$file" "$url"
    tar -xf "$file" -C "$SRC"
}

git_fetch() { # url tag dir [submodule paths...]
    local url=$1 tag=$2 dir=$3; shift 3
    [ -d "$SRC/$dir" ] && return
    echo ">> cloning $url ($tag)"
    rm -rf "$SRC/$dir.tmp"
    git -c advice.detachedHead=false clone --depth 1 --branch "$tag" "$url" "$SRC/$dir.tmp"
    [ $# -gt 0 ] && git -C "$SRC/$dir.tmp" submodule update --init --depth 1 "$@"
    mv "$SRC/$dir.tmp" "$SRC/$dir"
}

meson_build() { # dir [meson options...]
    local dir=$1; shift
    (cd "$SRC/$dir" &&
        meson setup build --prefix="$PREFIX" --libdir=lib --buildtype=release \
            --default-library=static -Db_staticpic=true "$@" --wipe 2>/dev/null ||
        meson setup build --prefix="$PREFIX" --libdir=lib --buildtype=release \
            --default-library=static -Db_staticpic=true "$@")
    ninja -C "$SRC/$dir/build" install
}

# ── dav1d: fast AV1 software decoding ────────────────────────────────────────
fetch "https://code.videolan.org/videolan/dav1d/-/archive/$DAV1D_VERSION/dav1d-$DAV1D_VERSION.tar.bz2" "dav1d-$DAV1D_VERSION"
meson_build "dav1d-$DAV1D_VERSION" -Denable_tools=false -Denable_tests=false

# ── zimg: high quality scaling/colour conversion used for HDR tone mapping ───
fetch "https://github.com/sekrit-twc/zimg/archive/refs/tags/release-$ZIMG_VERSION.tar.gz" "zimg-release-$ZIMG_VERSION"
# Homebrew's libtool only installs g-prefixed tools.
[ "$OS" = Darwin ] && export LIBTOOLIZE=glibtoolize
# zimg is C++: its pkg-config file must name the C++ runtime (STL_LIBS) or
# static links against it fail (FFmpeg's configure reports "zimg not found").
CXX_RUNTIME=-lstdc++
[ "$OS" = Darwin ] && CXX_RUNTIME=-lc++
(cd "$SRC/zimg-release-$ZIMG_VERSION" && { [ -f configure ] || ./autogen.sh; } &&
    ./configure --prefix="$PREFIX" --libdir="$PREFIX/lib" --enable-static --disable-shared --with-pic \
        STL_LIBS="$CXX_RUNTIME" &&
    make -j"$JOBS" && make install)
# zimg also calls libm, which its .pc file omits; with `--as-needed` static
# links then fail ("DSO missing from command line"). Set both explicitly.
ZIMG_PC=$PREFIX/lib/pkgconfig/zimg.pc
if grep -q '^Libs.private:' "$ZIMG_PC"; then
    sed -i.bak "s|^Libs.private:.*|Libs.private: $CXX_RUNTIME -lm|" "$ZIMG_PC"
else
    echo "Libs.private: $CXX_RUNTIME -lm" >> "$ZIMG_PC"
fi

# ── libdvdread / libdvdnav: DVD-Video navigation (libdvdcss is loaded at
#    runtime if the user has it installed, for encrypted discs) ─────────────
fetch "https://download.videolan.org/pub/videolan/libdvdread/$DVDREAD_VERSION/libdvdread-$DVDREAD_VERSION.tar.xz" "libdvdread-$DVDREAD_VERSION"
meson_build "libdvdread-$DVDREAD_VERSION"
fetch "https://download.videolan.org/pub/videolan/libdvdnav/$DVDNAV_VERSION/libdvdnav-$DVDNAV_VERSION.tar.xz" "libdvdnav-$DVDNAV_VERSION"
meson_build "libdvdnav-$DVDNAV_VERSION"

# ── NVIDIA decode headers (header only; the driver is loaded at runtime) ────
HW_FLAGS=()
if [ "$OS" = Linux ]; then
    fetch "https://github.com/FFmpeg/nv-codec-headers/releases/download/$NVHEADERS_VERSION/nv-codec-headers-${NVHEADERS_VERSION#n}.tar.gz" "nv-codec-headers-${NVHEADERS_VERSION#n}"
    make -C "$SRC/nv-codec-headers-${NVHEADERS_VERSION#n}" PREFIX="$PREFIX" install
    # NVDEC through the dynamically loaded CUDA driver; VA-API through DRM
    # (VDPAU is left out: it needs Xlib, and VA-API covers the same GPUs).
    HW_FLAGS=(--enable-ffnvcodec --enable-cuda --enable-nvdec --enable-openssl)
    pkg-config --exists libva libva-drm && HW_FLAGS+=(--enable-vaapi)

    # ── glslang + libplacebo: FFmpeg's libplacebo filter (Vulkan), which runs
    #    the GPU upscalers. Both are linked statically; the Vulkan loader is
    #    not linked at all: FFmpeg dlopens the system's libvulkan.so.1 and
    #    hands libplacebo its vkGetInstanceProcAddr (hence vk-proc-addr off).
    #    macOS is left out: it would need MoltenVK bundled. ───────────────
    fetch "https://github.com/KhronosGroup/glslang/archive/refs/tags/$GLSLANG_VERSION.tar.gz" "glslang-$GLSLANG_VERSION"
    # The optimizer (SPIRV-Tools) is off: libplacebo doesn't need it.
    cmake -S "$SRC/glslang-$GLSLANG_VERSION" -B "$SRC/glslang-$GLSLANG_VERSION/build" -G Ninja \
        -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$PREFIX" -DCMAKE_INSTALL_LIBDIR=lib \
        -DBUILD_SHARED_LIBS=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
        -DENABLE_OPT=OFF -DENABLE_HLSL=OFF -DENABLE_GLSLANG_BINARIES=OFF -DGLSLANG_TESTS=OFF
    ninja -C "$SRC/glslang-$GLSLANG_VERSION/build" install

    # Its git submodules carry the Vulkan headers and the code generators.
    git_fetch https://code.videolan.org/videolan/libplacebo.git "v$LIBPLACEBO_VERSION" "libplacebo-$LIBPLACEBO_VERSION" \
        3rdparty/Vulkan-Headers 3rdparty/glad 3rdparty/jinja 3rdparty/markupsafe 3rdparty/fast_float
    # libplacebo finds glslang with find_library/has_header, not pkg-config.
    (export CPATH="$PREFIX/include" LIBRARY_PATH="$PREFIX/lib" &&
        meson_build "libplacebo-$LIBPLACEBO_VERSION" -Dprefer_static=true -Dvulkan-sdk="$PREFIX" \
            -Dvulkan=enabled -Dvk-proc-addr=disabled -Dglslang=enabled -Dshaderc=disabled \
            -Dopengl=disabled -Dd3d11=disabled -Dlcms=disabled -Dlibdovi=disabled \
            -Dxxhash=disabled -Dunwind=disabled -Ddemos=false -Dtests=false)
    # Its .pc file lists glslang as absolute .a paths, which FFmpeg's configure
    # moves in front of -lplacebo, and leaves out the C++ runtime. Rewrite it
    # in link order.
    PLACEBO_PC=$PREFIX/lib/pkgconfig/libplacebo.pc
    sed -i.bak -e 's|^Libs:.*|Libs: -L${libdir} -lplacebo|' -e '/^Libs.private:/d' "$PLACEBO_PC"
    echo "Libs.private: -lglslang-default-resource-limits -lSPIRV -lglslang -lMachineIndependent" \
        "-lOSDependent -lGenericCodeGen -lstdc++ -lm -pthread" >> "$PLACEBO_PC"
    # FFmpeg's Vulkan code needs headers newer than most distributions ship.
    # (C headers only; the C++ ones are 20 MB.)
    VK_INC=$SRC/libplacebo-$LIBPLACEBO_VERSION/3rdparty/Vulkan-Headers/include
    mkdir -p "$PREFIX/include/vulkan"
    cp "$VK_INC"/vulkan/*.h "$PREFIX/include/vulkan/"
    cp -R "$VK_INC/vk_video" "$PREFIX/include/"
    HW_FLAGS+=(--enable-vulkan --enable-libplacebo)
elif [ "$OS" = Darwin ]; then
    # TLS through the system's Security framework: no OpenSSL to build or
    # bundle (Homebrew's is built for the runner's macOS, not our minimum).
    HW_FLAGS=(--enable-videotoolbox --enable-securetransport)
fi

# ── FFmpeg: decoders, demuxers and filters only ─────────────────────────────
fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" "ffmpeg-$FFMPEG_VERSION"
cd "$SRC/ffmpeg-$FFMPEG_VERSION"
./configure \
    --prefix="$PREFIX" \
    --pkg-config-flags=--static \
    --extra-cflags="-I$PREFIX/include" \
    --extra-ldflags="-L$PREFIX/lib" \
    --enable-static --disable-shared --enable-pic \
    --enable-gpl --enable-version3 \
    --disable-autodetect \
    --disable-programs --disable-doc --disable-debug \
    --disable-avdevice --disable-encoders --disable-muxers --disable-outdevs --disable-indevs \
    --enable-encoder=png --enable-muxer=image2 \
    --enable-zlib --enable-iconv \
    --enable-libdav1d --enable-libzimg --enable-libdvdread --enable-libdvdnav \
    "${HW_FLAGS[@]}" ||
    { echo "---- ffbuild/config.log (last 80 lines) ----"; tail -80 ffbuild/config.log; exit 1; }
make -j"$JOBS"
make install
echo
echo "FFmpeg $FFMPEG_VERSION installed to $PREFIX"
echo "Build MavPlayer with:"
echo "  PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig cargo build --release --features static-ffmpeg"
