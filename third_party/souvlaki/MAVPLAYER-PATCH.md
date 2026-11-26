# souvlaki 0.9.3, patched for MavPlayer

This is the crates.io release of [souvlaki](https://github.com/Sinono3/souvlaki)
0.9.3 (MIT), applied through `[patch.crates-io]` in MavPlayer's `Cargo.toml`.
MavPlayer uses it for media keys and now-playing information (MPRIS over zbus
on Linux).

The patch changes one loop, in `run_service` (`src/platform/mpris/zbus.rs`).
Upstream waits for the next metadata/playback/volume update with
`recv_timeout(10 ms)` and loops, so the MPRIS thread wakes 100 times a second
for as long as the app runs, playing or not. zbus answers D-Bus calls on its own
executor thread, so nothing on this thread needs waking between updates: it now
blocks in `recv()`, and a closed channel ends the service like `Kill` does.

Nothing else is changed. The upstream `.github`, `docs` and `Cargo.lock` files
are left out.
