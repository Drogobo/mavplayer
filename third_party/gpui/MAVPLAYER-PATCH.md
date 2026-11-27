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

On Wayland, an idle window no longer wakes at the display's refresh rate.
Upstream asks for a frame callback and commits on every frame, whether it drew
anything or not, so a window with nothing to draw still ran its frame loop 60
times a second (or more on faster displays). Now (`platform/linux/wayland/window.rs`,
`FrameLoop`):

- `completed_frame` asks for the next frame callback and commits only when the
  frame drew (presented) a scene, so the loop stops once nothing changes.
- `PlatformWindow::frame_waker` (`None` elsewhere: other platforms poll) gives
  the window's `WindowInvalidator` a function it calls when the window becomes
  dirty (`invalidate_view`, `set_dirty(true)`), and `Window::on_next_frame` calls
  it too. If no frame callback is pending, it runs a frame from the event loop
  (`insert_idle`), never from inside the caller.
- A wake that comes while a frame is running keeps the loop going at the
  compositor's pace for one more frame, even if this one draws nothing, so
  next-frame callbacks that ask for another frame cannot spin.
- Hiding or showing the window forgets a pending callback (one asked for
  before hiding may never come), so showing it can start the loop again.

Besides that, two float literals in `src/taffy.rs` are typed `f32` (a
future-incompatibility warning that path dependencies show). Nothing else is
changed.

`PathPromptOptions` has a `filters` list (`PathFilter`: a name, extensions and,
for the Linux portal, MIME types), so open dialogs can show only the files
MavPlayer can open. Upstream offers no filters at all. The first filter is
selected. The Linux portal gets each as a `FileFilter` (MIME types plus
`*.ext` globs; GTK matches globs case-sensitively but guesses MIME types from
names in any case), Windows as `SetFileTypes`, and macOS, whose open panel has
no list of types, shows only the first filter's extensions
(`setAllowedFileTypes:`). Directory pickers ignore it.
