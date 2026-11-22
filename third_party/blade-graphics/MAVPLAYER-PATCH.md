# blade-graphics 0.7.1, patched for MavPlayer

This is the crates.io release of [blade-graphics](https://github.com/kvark/blade)
0.7.1 (MIT), GPUI's renderer on Linux, applied through `[patch.crates-io]` in
MavPlayer's `Cargo.toml`.

The patch:

- `src/vulkan/init.rs`: ray tracing is never enabled. Upstream turns on
  `VK_KHR_acceleration_structure` and `VK_KHR_ray_query` whenever the GPU has
  them. GPUI never uses ray tracing, and on NVIDIA enabling it loads and sets
  up the driver's ray-tracing core. MavPlayer measured 50 MB of memory (PSS) for
  that, in every process, including one waiting in the system tray.
- `src/vulkan/resource.rs` (`BIG_BLOCK`, `expire_spare`), `mod.rs`, `init.rs`:
  device-local textures of 4 MB or more get a memory object of their own, of
  exactly their size, and a freed one is kept for up to 2 s (at most 3) and
  reused by the next texture of the same size. GPUI gives every image bigger
  than its 1024² atlas a texture of its own, so a video frame (8 MB at 1080p)
  used to take a 16 MB buddy-allocator block, rounded up to a power of two,
  and each frame allocated and freed one: two kernel calls on the UI thread,
  60 times a second.
- `src/vulkan/resource.rs` (`BIG_UPLOAD`): upload buffers of 1 MB or more
  (GPUI's staging chunks for video frames) get exact-size memory as well, so a
  33 MB 4K frame no longer takes a 64 MB block. They aren't kept as spares:
  the patched blade-util belt reuses them itself.
- `src/lib.rs`: allows the `deprecated` and `mismatched_lifetime_syntaxes`
  warnings that current compilers raise in this release.

Nothing else is changed. The `etc/` folder (docs and pictures for the README)
is left out. `LICENSE` is the upstream repository's, which the crates.io
package doesn't include.
