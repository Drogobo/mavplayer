# gpui 0.2.2, patched for MavPlayer

This is the crates.io release of [gpui](https://github.com/zed-industries/zed)
0.2.2 (Apache-2.0), applied through `[patch.crates-io]` in MavPlayer's
`Cargo.toml`. The package's `examples/` folder (and its `[[example]]` entries
in `Cargo.toml`) is left out.

The patch adds `Window::set_window_visible(bool)` (`PlatformWindow::set_visible`,
a no-op by default). "Play in the background" uses it to hide the player
window to the tray while playback goes on, and to bring it back: GPUI ends its
event loop when the last window *closes* on Linux and Windows, so the window
must stay open, only unshown.

- Wayland (`platform/linux/wayland/window.rs`): hiding attaches no buffer,
  which unmaps the toplevel; `frame` then draws nothing (a buffer attached
  while unmapped would map it without a configure). Showing sets the title,
  app ID, minimum size and decoration mode again (an unmapped toplevel forgets
  them), commits without a buffer and draws once the configure is
  acknowledged, as for a new window. The state keeps the title and minimum
  size for this.
- X11: `UnmapWindow` / `MapWindow` (an unmapped window is withdrawn).
- Windows: `ShowWindowAsync` with `SW_HIDE` / `SW_SHOW`.
- macOS: `orderOut:` / `makeKeyAndOrderFront:`.

It also adds `Window::activate_window_with_token(&str)`
(`PlatformWindow::activate_with_token`, `activate` by default). A tray host
hands its item an XDG activation token with each click; GPUI's own
`activate_window` asks for one with the window's last input serial, which
KWin refuses when the click was on the panel, so a window shown from the tray
could come up behind others. On Wayland the token is used with
`xdg_activation_v1.activate`; a window that is hidden or not yet configured
keeps it and is activated right after its first frame once mapped.

Besides that, two float literals in `src/taffy.rs` are typed `f32` (a
future-incompatibility warning that path dependencies show). Nothing else is
changed.
