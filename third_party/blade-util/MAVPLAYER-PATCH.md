# blade-util 0.3.0, patched for MavPlayer

This is the crates.io release of [blade-util](https://github.com/kvark/blade)
0.3.0 (MIT), whose `BufferBelt` GPUI uses to upload textures, applied through
`[patch.crates-io]` in MavPlayer's `Cargo.toml`.

The patch, all in `src/belt.rs`:

- A chunk is reused by best fit (the smallest that is big enough), not the
  first one that fits, so a small upload doesn't take a frame-sized chunk.
- Chunks of 1 MB or more (`BIG_CHUNK`) unused for 2 s (`IDLE_FOR`) are
  destroyed once the GPU is done with them (`expire`, run from `alloc`).
  Upstream keeps every chunk for good, and GPUI uploads each video frame
  through a chunk of its own size: every window size played at left its
  frame-sized chunks behind (three 33 MB ones after a 4K window).

Nothing else is changed. `LICENSE` is the upstream repository's, which the
crates.io package doesn't include.
