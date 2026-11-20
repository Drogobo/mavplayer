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

use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px, size,
};
use std::path::PathBuf;

const HELP: &str = "\
MavPlayer — play video, music, streams and DVDs

USAGE:
    mavplayer [OPTIONS] [FILE | FOLDER | URL | dvd://[TITLE][/DEVICE]] ...

OPTIONS:
    -f, --fullscreen          Start in fullscreen
    -n, --new-window          Open a new window even if MavPlayer is already open
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
    verbose: bool,
}

fn parse_args() -> Args {
    let mut args = Args { sources: vec![], fullscreen: false, new_window: false, verbose: false };
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

pub static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Milliseconds since launch, for the startup timings printed with `-v`.
pub fn ms_since_start() -> f64 {
    START.get().map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0)
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
    let start = *START.get_or_init(std::time::Instant::now);
    let args = parse_args();
    let verbose = args.verbose || std::env::var_os("MAVPLAYER_DEBUG").is_some();
    if verbose && log::set_boxed_logger(Box::new(StderrLog(start))).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    engine::init(verbose);

    let settings = settings::Settings::load();
    // Hand the files to an open window instead, if the settings say so.
    if !args.sources.is_empty()
        && !args.new_window
        && settings.when_running != settings::RunningMode::NewWindow
        && instance::forward(settings.when_running, &args.sources)
    {
        log::debug!("startup: handed {} item(s) to the open window", args.sources.len());
        return;
    }

    // GPUI's font scan in Application::new() is slow on a cold cache: warm it in parallel first.
    #[cfg(target_os = "linux")]
    let fonts = font_prefetch::start();
    // Start opening the first file now: it decodes while GPUI brings up the GPU.
    let boot = ui::player::Boot::start(settings, args.sources);
    integration::ensure_window_icon();
    #[cfg(target_os = "linux")]
    if let Some(fonts) = fonts {
        let _ = fonts.join();
    }
    let app = Application::new().with_assets(icons::Assets);
    log::debug!("startup: GPU and platform ready at {:.0} ms", ms_since_start());
    // Files opened from Finder / the dock while running (and URL schemes).
    app.on_open_urls(|urls| {
        let _ = ui::player::OPEN_REQUESTS.lock().map(|mut q| q.extend(urls));
    });
    app.run(move |cx: &mut App| {
        ui::actions::bind_keys(cx);
        ui::text_input::bind_keys(cx);
        #[cfg(target_os = "macos")]
        ui::actions::set_native_menus(cx);

        let fullscreen = args.fullscreen || boot.settings.start_fullscreen;
        let (w, h) = boot.settings.window_size.unwrap_or((960.0, 600.0));
        // Wide enough for every control chosen for the bottom bar. (GPUI can't
        // change it later; the bar leaves controls out if it must.)
        let min_w = ui::bar::min_window_width(&boot.settings.bar_controls);
        let bounds = Bounds::centered(None, size(px(w.max(480.0).max(min_w)), px(h.max(320.0))), cx);
        let options = WindowOptions {
            window_bounds: Some(if fullscreen {
                WindowBounds::Fullscreen(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            }),
            titlebar: Some(TitlebarOptions {
                title: Some("MavPlayer".into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            app_id: Some(integration::APP_ID.to_string()),
            window_min_size: Some(size(px(min_w), px(280.))),
            window_decorations: Some(WindowDecorations::Server),
            focus: true,
            show: true,
            ..Default::default()
        };
        log::debug!("startup: opening window at {:.0} ms", ms_since_start());
        let window =
            cx.open_window(options, move |window, cx| cx.new(|cx| ui::player::PlayerView::new(window, cx, boot)));
        if let Err(e) = window {
            eprintln!("mavplayer: could not open a window: {e:#}");
            cx.quit();
            return;
        }
        // Quit when the last window closes (all platforms, including macOS: a player has no use windowless).
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
        log::debug!("startup: window open at {:.0} ms", ms_since_start());
    });
}
