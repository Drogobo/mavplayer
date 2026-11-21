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
- `src/lib.rs`: allows the `deprecated` and `mismatched_lifetime_syntaxes`
  warnings that current compilers raise in this release.

Nothing else is changed. The `etc/` folder (docs and pictures for the README)
is left out. `LICENSE` is the upstream repository's, which the crates.io
package doesn't include.
