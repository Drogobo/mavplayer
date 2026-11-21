# ksni 0.3.7, patched for MavPlayer

This is the crates.io release of [ksni](https://github.com/iovxw/ksni) 0.3.7
(Unlicense), applied through `[patch.crates-io]` in MavPlayer's `Cargo.toml`.

The patch adds one builder option, `dbus_name(Option<String>)`, on both
`TrayServiceBuilder`s (`src/lib.rs`, `src/blocking.rs`). With it set,
`service::run` (`src/service.rs`) first requests that well-known name with
`AllowReplacement | ReplaceExisting | DoNotQueue`, and falls back to the usual
`org.kde.StatusNotifierItem-PID-N` name (or the unique name) if it can't have it.

MavPlayer hands its tray icon from one process to the next when the window
closes. With a fixed name, the new process takes the name straight from the old
one, and Plasma's watcher (which drops an item only once its name has no owner)
keeps the icon on screen throughout.

Nothing else is changed. The upstream `.github`, `.config` and `Cargo.lock`
files are left out.
