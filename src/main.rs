//! MavPlayer — a fast, native media player built on FFmpeg and GPUI.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod engine;
#[cfg(target_os = "linux")]
mod font_prefetch;
mod icons;
mod instance;
mod integration;
mod playlist;
mod settings;
mod ui;

use gpui::{App, Application};
use std::path::PathBuf;

const HELP: &str = "\
MavPlayer — play video, music, streams and DVDs

USAGE:
    mavplayer [OPTIONS] [FILE | FOLDER | URL | dvd://[TITLE][/DEVICE]] ...

OPTIONS:
    -f, --fullscreen          Start in fullscreen
    -n, --new-window          Open a new window even if MavPlayer is already open
        --background          Start in the system tray without a window, so
                              the first file opened plays almost instantly
        --integrate           Add this AppImage to the desktop's application menu
        --remove-integration  Remove the application menu entry created by --integrate
    -v, --verbose             Print FFmpeg diagnostics to stderr
    -h, --help                Show this help
    -V, --version             Show the version
";

struct Args {
    sources: Vec<engine::MediaSource>,
    fullscreen: bool,
    new_window: bool,
    /// Wait in the system tray without a window.
    background: bool,
    verbose: bool,
}

fn parse_args() -> Args {
    let mut args = Args { sources: vec![], fullscreen: false, new_window: false, background: false, verbose: false };
    let mut paths: Vec<PathBuf> = vec![];
    let mut only_files = false;
    for a in std::env::args().skip(1) {
        if !only_files && a.starts_with('-') && a.len() > 1 {
            match a.as_str() {
                "--" => only_files = true,
                "-h" | "--help" => {
                    print!("{HELP}");
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("mavplayer {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "-f" | "--fullscreen" => args.fullscreen = true,
                "-n" | "--new-window" => args.new_window = true,
                "--background" => args.background = true,
                "-v" | "--verbose" => args.verbose = true,
                "--integrate" => exit_with(integration::integrate().map(|p| format!("Installed {}", p.display()))),
                "--remove-integration" | "--unintegrate" => {
                    exit_with(integration::unintegrate().map(|_| "Removed desktop integration".to_string()))
                }
                // Ignore unknown options passed by desktop launchers (e.g. -psn_… on macOS).
                other => eprintln!("mavplayer: ignoring unknown option {other}"),
            }
            continue;
        }
        if a.contains("://") {
            // Flush pending paths first so command-line order is preserved.
            args.sources.extend(playlist::expand_paths(&std::mem::take(&mut paths)));
            args.sources.push(playlist::source_for_arg(&a));
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    args.sources.extend(playlist::expand_paths(&paths));
    args
}

/// Minimal stderr logger (enabled by --verbose or MAVPLAYER_DEBUG).
struct StderrLog(std::time::Instant);

impl log::Log for StderrLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("mavplayer") && m.level() <= log::max_level()
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{:>8.3}s {:5}] {}", self.0.elapsed().as_secs_f64(), r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

static START: parking_lot::Mutex<Option<std::time::Instant>> = parking_lot::Mutex::new(None);

/// When this launch began: the process start, or the moment a MavPlayer
/// waiting in the tray started opening its window again.
pub fn launch_instant() -> Option<std::time::Instant> {
    *START.lock()
}

pub fn mark_launch() {
    *START.lock() = Some(std::time::Instant::now());
}

/// Milliseconds since launch, for the startup timings printed with `-v`.
pub fn ms_since_start() -> f64 {
    launch_instant().map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0)
}

fn exit_with(r: anyhow::Result<String>) -> ! {
    match r {
        Ok(msg) => {
            println!("{msg}");
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("mavplayer: {e:#}");
            std::process::exit(1)
        }
    }
}

fn main() {
    let start = std::time::Instant::now();
    *START.lock() = Some(start);
    let args = parse_args();
    let verbose = args.verbose || std::env::var_os("MAVPLAYER_DEBUG").is_some();
    if verbose && log::set_boxed_logger(Box::new(StderrLog(start))).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    // Files on the command line win over --background.
    let background = args.background && args.sources.is_empty();

    let settings = settings::Settings::load();
    // Hand the files to an open window instead, if the settings say so. A
    // MavPlayer waiting in the tray takes any launch, files or not: opening
    // its window is much quicker than starting up.
    let hand_over =
        !args.sources.is_empty() && !args.new_window && settings.when_running != settings::RunningMode::NewWindow;
    if !background && instance::forward(settings.when_running, &args.sources, args.fullscreen, !hand_over) {
        log::debug!("startup: handed {} item(s) to the running MavPlayer", args.sources.len());
        return;
    }
    engine::init(verbose);

    // Requests from later launches, the OS and the tray icon, for the UI thread.
    let (tx, rx) = ui::shell::channel();
    let primary = {
        let tx = tx.clone();
        // A MavPlayer closing into the tray starts this one and quits: wait for its socket.
        let wait = std::time::Duration::from_secs(if background { 5 } else { 0 });
        instance::listen(wait, move |req| {
            let _ = tx.unbounded_send(ui::shell::Incoming::Remote(req));
        })
    };
    if background && !primary {
        log::debug!("startup: another MavPlayer is already running; nothing to do in the background");
        return;
    }
    // GPUI's font scan in Application::new() is slow on a cold cache: warm it in parallel first.
    #[cfg(target_os = "linux")]
    let fonts = font_prefetch::start();
    let boot = if background {
        // No file yet, but have the GPU ready for the first one.
        engine::prewarm(&settings.engine_config());
        ui::shell::Start::Background { settings }
    } else {
        // Start opening the first file now: it decodes while GPUI brings up the GPU.
        ui::shell::Start::Window {
            boot: ui::player::Boot::start(settings, args.sources, primary),
            fullscreen: args.fullscreen,
        }
    };
    integration::ensure_window_icon();
    #[cfg(target_os = "linux")]
    if let Some(fonts) = fonts {
        let _ = fonts.join();
    }
    let app = Application::new().with_assets(icons::Assets);
    log::debug!("startup: GPU and platform ready at {:.0} ms", ms_since_start());
    // Files opened from Finder / the dock while running (and URL schemes).
    {
        let tx = tx.clone();
        app.on_open_urls(move |urls| {
            let _ = tx.unbounded_send(ui::shell::Incoming::Urls(urls));
        });
    }
    // The dock icon clicked while MavPlayer waits in the background (macOS).
    {
        let tx = tx.clone();
        app.on_reopen(move |_| {
            let _ = tx.unbounded_send(ui::shell::Incoming::Reopen);
        });
    }
    app.run(move |cx: &mut App| {
        ui::keymap::apply(cx, &boot.settings().key_bindings);
        #[cfg(target_os = "macos")]
        ui::actions::set_native_menus(cx);
        // Quits when the last window closes (on every platform: a player has
        // no use windowless), unless it is to wait in the system tray.
        ui::shell::run(cx, boot, primary, tx, rx);
    });
}
