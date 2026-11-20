# Bundled mpv-style shaders

Unmodified GPU shaders (mpv `.glsl`/`.hook` user-shader format), compiled into
the binary by `src/engine/upscale.rs` and run through FFmpeg's `libplacebo`
filter. Every license here is compatible with MavPlayer's GPL-3.0-or-later:
the MIT ones are permissive, and LGPL-3.0-or-later code may be used in a
GPL-3.0 program. Each file keeps its original copyright header.

| Folder | Files | Author / source | License |
|---|---|---|---|
| `anime4k/` | Anime4K v4 presets (modes A, B, C, A+A, B+B, C+A, HQ and Fast) | bloc97, <https://github.com/bloc97/Anime4K> @ `7684e9586f8d` | MIT |
| `artcnn/` | `ArtCNN_C4F16.glsl`, `ArtCNN_C4F16_DS.glsl` | Artoriuz, <https://github.com/Artoriuz/ArtCNN> @ `0837e5326e71` | MIT |
| `fsrcnnx/` | `FSRCNNX_x2_8-0-4-1.glsl`, `FSRCNNX_x2_16-0-4-1.glsl` | igv, <https://github.com/igv/FSRCNN-TensorFlow> release 1.1 | LGPL-3.0-or-later |
| `ravu/` | `ravu-lite-ar-r4.hook` | bjin, <https://github.com/bjin/mpv-prescalers> @ `b3f0a59d68f3` | LGPL-3.0-or-later |
| `amd/` | `FSR.glsl` (FidelityFX Super Resolution 1.0.2), `CAS.glsl` (Contrast Adaptive Sharpening 1.2) | AMD, ported to mpv by agyild: gists `82219c545228d70c5604f865ce0b0ce5` @ `2623d743`, `bbb4e58298b2f86aa24da3032a0d2ee6` @ `99d7dbeb` | MIT |
| `nvidia/` | `NVScaler.glsl` (NVIDIA Image Scaling 1.0.3) | NVIDIA, ported to mpv by agyild: gist `7e8951915b2bf24526a9343d951db214` @ `9d270a44` | MIT |
| `igv/` | `KrigBilateral.glsl` | Shiandow / igv: gist `a015fc885d5c22e6891820ad89555637` @ `03806482` | LGPL-3.0-or-later |

The full license texts are in `LICENSES.txt`, which the packages install
next to MavPlayer's own `LICENSE` (the LGPL-3.0 builds on the GPL-3.0 text
there). Keep it in step when shaders are added or replaced.

Gists are at `https://gist.github.com/<id>`. To update a shader, replace the
file with the newer upstream version (same name), keep its header, and update
the commit in this table. Before adding a new shader, check that its license
is GPL-3.0-compatible (MIT, BSD, LGPL, GPL-3.0; not "non-commercial" or
GPL-2.0-only).
