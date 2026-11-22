//! Build script.
//!
//! * Embeds the Papirus symbolic icons MavPlayer uses into the binary. The
//!   icons come from the Papirus git submodule (`third_party/papirus-icon-theme`)
//!   or from `MAVPLAYER_PAPIRUS_DIR`; nothing from Papirus is committed here
//!   and nothing needs to be installed at runtime.
//! * On Windows, embeds the application icon and version info in the .exe.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Freedesktop icon names used by the UI (`<name>-symbolic.svg` in Papirus).
const ICONS: &[&str] = &[
    "media-playback-start",
    "media-playback-pause",
    "media-playback-stop",
    "media-skip-backward",
    "media-skip-forward",
    "media-seek-backward",
    "media-seek-forward",
    "audio-volume-high",
    "audio-volume-medium",
    "audio-volume-low",
    "audio-volume-muted",
    "view-fullscreen",
    "view-restore",
    "view-list",
    "media-playlist-repeat",
    "media-playlist-repeat-song",
    "media-playlist-shuffle",
    "media-playlist-consecutive",
    "preferences-system",
    "media-view-subtitles",
    "window-close",
    "window-minimize",
    "window-maximize",
    "window-restore",
    "document-open",
    "folder-open",
    "list-add",
    "list-remove",
    "edit-clear-all",
    "media-optical-dvd",
    "globe",
    "dialog-information",
    "audio-x-generic",
    "video-x-generic",
    "go-up",
    "go-down",
    "camera-photo",
    "go-next",
    "go-previous",
    "object-select",
    "pan-down",
    "pan-end",
    "pan-start",
    "media-eject",
    "open-menu",
    "view-more",
    "document-save",
    "edit-delete",
    "zoom-in",
    "zoom-out",
    "view-refresh",
    "help-about",
    "audio-speakers",
    "document-open-recent",
    "speedometer",
    "multimedia-player",
];

const CATEGORIES: &[&str] = &["actions", "status", "devices", "places", "mimetypes", "categories", "emblems", "apps"];

fn papirus_root() -> PathBuf {
    println!("cargo:rerun-if-env-changed=MAVPLAYER_PAPIRUS_DIR");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let candidates: Vec<PathBuf> = env::var_os("MAVPLAYER_PAPIRUS_DIR")
        .map(PathBuf::from)
        .into_iter()
        .chain(std::iter::once(manifest.join("third_party/papirus-icon-theme")))
        .collect();
    for c in &candidates {
        // Accept either the repository root or the `Papirus` theme folder itself.
        for p in [c.join("Papirus"), c.clone()] {
            if p.join("16x16/symbolic").is_dir() {
                return p;
            }
        }
    }
    panic!(
        "\n\nPapirus icons not found (looked in {:?}).\n\
         Fetch the submodule with:\n\n    git submodule update --init --depth 1\n\n\
         or point MAVPLAYER_PAPIRUS_DIR at a papirus-icon-theme checkout or an installed\n\
         Papirus theme directory (e.g. /usr/share/icons/Papirus).\n",
        candidates
    );
}

/// Reads an SVG, following symlinks. On Windows checkouts without symlink
/// support git writes the link target as the file content; follow that too.
fn read_svg(path: &Path, depth: u32) -> Option<Vec<u8>> {
    let data = fs::read(path).ok()?;
    let trimmed = String::from_utf8_lossy(&data).trim().to_string();
    if depth < 8 && !trimmed.starts_with('<') && trimmed.ends_with(".svg") && !trimmed.contains('\n') {
        return read_svg(&path.parent()?.join(trimmed), depth + 1);
    }
    Some(data)
}

fn embed_icons() {
    let root = papirus_root();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("icons");
    fs::create_dir_all(&out).unwrap();
    let mut table = String::from("pub static BUNDLED_ICONS: &[(&str, &[u8])] = &[\n");
    for name in ICONS {
        let mut found = None;
        'search: for size in ["16x16", "24x24", "22x22"] {
            for cat in CATEGORIES {
                let p = root.join(size).join("symbolic").join(cat).join(format!("{name}-symbolic.svg"));
                if let Some(d) = read_svg(&p, 0) {
                    found = Some(d);
                    break 'search;
                }
            }
        }
        let data = found.unwrap_or_else(|| panic!("Papirus icon '{name}-symbolic' not found under {}", root.display()));
        let dest = out.join(format!("{name}.svg"));
        fs::write(&dest, data).unwrap();
        table
            .push_str(&format!("    ({name:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/icons/{name}.svg\"))),\n"));
    }
    table.push_str("];\n");
    fs::write(PathBuf::from(env::var("OUT_DIR").unwrap()).join("bundled_icons.rs"), table).unwrap();
    println!("cargo:rerun-if-changed={}", root.join("16x16/symbolic").display());
}

fn main() {
    embed_icons();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=packaging/windows/mavplayer.ico");

    link_clang_runtime_on_macos();
    detect_dvdnav();

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("packaging/windows/mavplayer.ico");
        res.set("ProductName", "MavPlayer");
        res.set("FileDescription", "MavPlayer media player");
        res.set("LegalCopyright", "MavPlayer contributors. GNU GPL v3 or later.");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed Windows resources: {e}");
        }
    }
}

/// FFmpeg's VideoToolbox code uses `@available(...)`, which compiles to calls
/// into clang's runtime (`__isPlatformVersionAtLeast`). C programs get that
/// library automatically, but rustc links with `-nodefaultlibs`, so a static
/// FFmpeg needs it added by hand.
/// DVD menus call libdvdnav directly (FFmpeg's `dvdvideo` demuxer uses it
/// too, but only plays titles). Enables `cfg(has_dvdnav)` when it can be
/// linked: from the static FFmpeg's own link flags, or as a shared library
/// found with pkg-config. Without it DVDs play title by title.
fn detect_dvdnav() {
    println!("cargo::rustc-check-cfg=cfg(has_dvdnav)");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=MAVPLAYER_NO_DVDNAV");
    if env::var_os("MAVPLAYER_NO_DVDNAV").is_some() {
        return;
    }
    let pkg_config = |args: &[&str]| -> Option<String> {
        let exe = env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".into());
        let out = std::process::Command::new(exe).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    if env::var_os("CARGO_FEATURE_STATIC_FFMPEG").is_some() {
        // ffmpeg-sys links libavformat's private libraries, libdvdnav among them.
        if pkg_config(&["--static", "--libs", "libavformat"]).is_some_and(|l| l.contains("-ldvdnav")) {
            println!("cargo:rustc-cfg=has_dvdnav");
        }
        return;
    }
    if let Some(libs) = pkg_config(&["--libs", "dvdnav", "dvdread"]) {
        for tok in libs.split_whitespace() {
            if let Some(dir) = tok.strip_prefix("-L") {
                println!("cargo:rustc-link-search=native={dir}");
            }
        }
        println!("cargo:rustc-link-lib=dylib=dvdnav");
        // libdvdnav's own dependency, called directly for the IFO files.
        println!("cargo:rustc-link-lib=dylib=dvdread");
        println!("cargo:rustc-cfg=has_dvdnav");
    }
}

fn link_clang_runtime_on_macos() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") || env::var_os("CARGO_FEATURE_STATIC_FFMPEG").is_none()
    {
        return;
    }
    let resource_dir = std::process::Command::new("xcrun")
        .args(["clang", "-print-resource-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    match resource_dir.map(|d| d.join("lib/darwin")).filter(|d| d.join("libclang_rt.osx.a").exists()) {
        Some(dir) => {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
        None => println!("cargo:warning=libclang_rt.osx.a not found; linking a static FFmpeg may fail"),
    }
}
