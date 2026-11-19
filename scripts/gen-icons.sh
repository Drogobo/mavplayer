#!/usr/bin/env bash
# Regenerates the raster app icons from assets/app-icon.svg.
# Needs: rsvg-convert, python3 with Pillow.
set -euo pipefail
cd "$(dirname "$0")/.."
SVG=assets/app-icon.svg
OUT=packaging/icons
mkdir -p "$OUT"
for s in 16 22 24 32 48 64 128 256 512 1024; do
  rsvg-convert -w $s -h $s "$SVG" -o "$OUT/mavplayer-$s.png"
done
python3 - <<'PY'
from PIL import Image
base = "packaging/icons/mavplayer-{}.png"
imgs = {s: Image.open(base.format(s)).convert("RGBA") for s in (16, 24, 32, 48, 64, 128, 256, 512, 1024)}
imgs[256].save("packaging/windows/mavplayer.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
imgs[1024].save("packaging/macos/MavPlayer.icns")
PY
echo "icons written to $OUT, packaging/windows/mavplayer.ico, packaging/macos/MavPlayer.icns"
