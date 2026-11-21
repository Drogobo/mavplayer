//! Everything around the player window that lasts as long as the process:
//! the window itself, the tray icon, and media handed over by later launches
//! or the OS.
//!
//! With "Keep running in the system tray" on, closing the window doesn't end
//! MavPlayer: a process waits in the tray with no window and no playback. The
//! next thing opened (from a file manager, the command line or the tray icon)
//! gets a window from that warm process in a few tens of milliseconds: its
//! libraries, fonts, GPU context and hardware decoder are already loaded.
//!
//! GPUI ends its event loop when the last window closes on Linux and Windows,
//! so there the closing process starts a fresh `mavplayer --background` and
//! quits. That also keeps the waiting process lean: it never played anything
//! (about 100 MB of private memory on NVIDIA, mostly the GPU driver and
//! GPUI's font database).
//! On macOS the app simply stays running without windows.

use super::player::{Boot, PlayerView};
use super::tray::{Tray, TrayEvent};
use crate::engine::MediaSource;
use crate::settings::Settings;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use gpui::{
    App, AppContext, Bounds, Global, TitlebarOptions, WindowBounds, WindowDecorations, WindowHandle, WindowOptions, px,
    size,
};

/// Something for the shell to act on, from outside the window.
pub enum Incoming {
    /// Media (or just "show yourself") from a later launch; see `instance`.
    Remote(crate::instance::Request),
    /// Files or URLs the OS asks us to open (macOS Finder and dock).
    Urls(Vec<String>),
    Tray(TrayEvent),
    /// The dock icon was clicked (macOS).
    Reopen,
}

pub type Sender = UnboundedSender<Incoming>;

pub fn channel() -> (Sender, UnboundedReceiver<Incoming>) {
    futures::channel::mpsc::unbounded()
}

/// How this process starts.
#[allow(clippy::large_enum_variant)] // made once, at startup
pub enum Start {
    /// With a window (a normal launch).
    Window { boot: Boot, fullscreen: bool },
    /// In the tray, without a window (`--background`).
    Background { settings: Settings },
}

impl Start {
    pub fn settings(&self) -> &Settings {
        match self {
            Start::Window { boot, .. } => &boot.settings,
            Start::Background { settings } => settings,
        }
    }
}

struct Shell {
    window: Option<WindowHandle<PlayerView>>,
    /// Later launches reach this process (it owns the instance socket).
    /// Only that one may wait in the tray: nothing could reach another.
    primary: bool,
    tx: Sender,
    tray: Option<Tray>,
    /// The setting asks for a tray icon.
    want_tray: bool,
    /// A tray icon is being set up.
    tray_starting: bool,
    /// Quitting for good (Quit chosen): don't wait in the tray.
    quitting: bool,
}

impl Global for Shell {}

/// Opens the first window (or the tray icon) and handles `rx`. Called from `app.run`.
pub fn run(cx: &mut App, start: Start, primary: bool, tx: Sender, mut rx: UnboundedReceiver<Incoming>) {
    let keep_in_tray = start.settings().keep_in_tray;
    cx.set_global(Shell {
        window: None,
        primary,
        tx,
        tray: None,
        want_tray: false,
        tray_starting: false,
        quitting: false,
    });
    match start {
        Start::Window { boot, fullscreen } => {
            if !open_window(boot, fullscreen, cx) {
                cx.quit();
                return;
            }
            if keep_in_tray {
                set_tray(true, false, cx);
            }
        }
        Start::Background { .. } => {
            crate::instance::set_background(true);
            // Without a tray icon there would be no way to reach or quit this process.
            start_tray(false, true, cx);
            log::debug!("startup: ready in the background at {:.0} ms", crate::ms_since_start());
        }
    }
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            last_window_closed(cx);
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Some(msg) = rx.next().await {
            if cx.update(|cx| handle(msg, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Quits MavPlayer for good, tray included (the Quit command).
pub fn quit(cx: &mut App) {
    if cx.has_global::<Shell>() {
        cx.global_mut::<Shell>().quitting = true;
    }
    crate::instance::stop_accepting();
    cx.quit();
}

/// Opens the player window for `boot`. Returns false if it couldn't.
fn open_window(boot: Boot, fullscreen: bool, cx: &mut App) -> bool {
    let fullscreen = fullscreen || boot.settings.start_fullscreen;
    let (w, h) = boot.settings.window_size.unwrap_or((960.0, 600.0));
    // Wide enough for every control chosen for the bottom bar. (GPUI can't
    // change it later; the bar leaves controls out if it must.)
    let min_w = super::bar::min_window_width(&boot.settings.bar_controls);
    let bounds = Bounds::centered(None, size(px(w.max(480.0).max(min_w)), px(h.max(320.0))), cx);
    let options = WindowOptions {
        window_bounds: Some(if fullscreen { WindowBounds::Fullscreen(bounds) } else { WindowBounds::Windowed(bounds) }),
        titlebar: Some(TitlebarOptions {
            title: Some("MavPlayer".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        app_id: Some(crate::integration::APP_ID.to_string()),
        window_min_size: Some(size(px(min_w), px(280.))),
        window_decorations: Some(WindowDecorations::Server),
        focus: true,
        show: true,
        ..Default::default()
    };
    log::debug!("startup: opening window at {:.0} ms", crate::ms_since_start());
    match cx.open_window(options, move |window, cx| cx.new(|cx| PlayerView::new(window, cx, boot))) {
        Ok(handle) => {
            cx.global_mut::<Shell>().window = Some(handle);
            crate::instance::set_background(false);
            cx.activate(true);
            log::debug!("startup: window open at {:.0} ms", crate::ms_since_start());
            true
        }
        Err(e) => {
            eprintln!("mavplayer: could not open a window: {e:#}");
            false
        }
    }
}

/// Opens the window from the background, playing `sources`.
fn reopen(sources: Vec<MediaSource>, fullscreen: bool, cx: &mut App) {
    crate::mark_launch();
    log::debug!("opening the window from the tray with {} item(s)", sources.len());
    let primary = cx.global::<Shell>().primary;
    let boot = Boot::start(Settings::load(), sources, primary);
    if !open_window(boot, fullscreen, cx) {
        quit(cx);
    }
}

/// The open player window, if any.
fn window(cx: &App) -> Option<WindowHandle<PlayerView>> {
    let w = cx.global::<Shell>().window?;
    cx.windows().iter().any(|h| h.window_id() == w.window_id()).then_some(w)
}

fn handle(msg: Incoming, cx: &mut App) {
    let open = window(cx);
    match msg {
        Incoming::Remote(req) => match open {
            Some(w) => {
                let _ = w.update(cx, |v, window, cx| v.open_remote(req, window, cx));
            }
            None => reopen(req.sources, req.fullscreen, cx),
        },
        Incoming::Urls(urls) => {
            let sources: Vec<MediaSource> = urls.iter().map(|u| crate::playlist::source_for_arg(u)).collect();
            match open {
                Some(w) => {
                    let _ = w.update(cx, |v, window, cx| v.open_sources(sources, window, cx));
                }
                None => reopen(sources, false, cx),
            }
        }
        Incoming::Tray(TrayEvent::Show) | Incoming::Reopen => match open {
            Some(w) => {
                let _ = w.update(cx, |_, window, _| window.activate_window());
            }
            None => reopen(Vec::new(), false, cx),
        },
        Incoming::Tray(TrayEvent::Quit) => {
            if let Some(w) = open {
                let _ = w.update(cx, |v, _, _| v.save_state());
            }
            quit(cx);
        }
    }
}

/// Waits in the tray when its icon is up; quits otherwise.
fn last_window_closed(cx: &mut App) {
    let shell = cx.global_mut::<Shell>();
    shell.window = None;
    let stay = shell.tray.is_some() && shell.primary && !shell.quitting;
    if !stay {
        quit(cx);
        return;
    }
    #[cfg(target_os = "macos")]
    {
        crate::instance::set_background(true);
        log::debug!("window closed; waiting in the menu bar");
    }
    #[cfg(not(target_os = "macos"))]
    {
        // GPUI is ending its event loop: hand over to a fresh process.
        crate::instance::stop_accepting();
        match spawn_background() {
            Ok(()) => log::debug!("window closed; a new MavPlayer takes over in the system tray"),
            Err(e) => log::warn!("could not keep running in the system tray: {e:#}"),
        }
        cx.quit();
    }
}

/// Starts `mavplayer --background`, detached from this process (and its terminal).
#[cfg(not(target_os = "macos"))]
fn spawn_background() -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    let mut cmd = if std::env::var_os("FLATPAK_ID").is_some() {
        // A process left in this sandbox would end with it: start a new one.
        let mut c = Command::new("flatpak-spawn");
        c.arg(&exe);
        c
    } else if let Some(appimage) = crate::integration::appimage_path() {
        // This AppImage's mount goes away when this process ends.
        Command::new(appimage)
    } else {
        Command::new(&exe)
    };
    cmd.arg("--background").stdin(Stdio::null()).stdout(Stdio::null());
    if log::log_enabled!(log::Level::Debug) {
        cmd.arg("-v");
    } else {
        cmd.stderr(Stdio::null());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: closing the terminal this one ran in mustn't end it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()?;
    Ok(())
}

/// Shows or removes the tray icon, following the "Keep running in the system
/// tray" setting. With `announce`, a desktop without a tray is reported in
/// the window (when the setting has just been turned on).
pub fn set_tray(on: bool, announce: bool, cx: &mut App) {
    let shell = cx.global_mut::<Shell>();
    shell.want_tray = on;
    if !on {
        shell.tray = None;
        return;
    }
    if !shell.primary {
        if announce {
            notify("Another MavPlayer window is the one that stays in the tray", cx);
        }
        return;
    }
    start_tray(announce, false, cx);
}

/// Sets up the tray icon unless it is already there. With `required`, a
/// failure quits (a background process can't be reached without it).
fn start_tray(announce: bool, required: bool, cx: &mut App) {
    let shell = cx.global_mut::<Shell>();
    shell.want_tray = true;
    if shell.tray.is_some() || shell.tray_starting {
        return;
    }
    shell.tray_starting = true;
    let tx = shell.tx.clone();
    let make = move || {
        Tray::new(move |ev| {
            let _ = tx.unbounded_send(Incoming::Tray(ev));
        })
    };
    cx.spawn(async move |cx| {
        // Registering over D-Bus can take a moment: keep it off the UI thread.
        #[cfg(target_os = "linux")]
        let tray = cx.background_executor().spawn(async move { make() }).await;
        // Windows and macOS need it on the main thread; let the window
        // appear first so it doesn't delay the first picture.
        #[cfg(not(target_os = "linux"))]
        let tray = {
            if !required {
                cx.background_executor().timer(std::time::Duration::from_millis(300)).await;
            }
            make()
        };
        let _ = cx.update(|cx| {
            let shell = cx.global_mut::<Shell>();
            shell.tray_starting = false;
            match tray {
                Ok(tray) if shell.want_tray => shell.tray = Some(tray),
                Ok(_) => {}
                Err(e) => {
                    log::warn!("no system tray: {e:#}");
                    if required {
                        quit(cx);
                    } else if announce {
                        notify("No system tray found: closing the window will quit", cx);
                    }
                }
            }
        });
    })
    .detach();
}

fn notify(msg: &'static str, cx: &mut App) {
    if let Some(w) = window(cx) {
        let _ = w.update(cx, |v, _, cx| v.osd(msg, cx));
    }
}
