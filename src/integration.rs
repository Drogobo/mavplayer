//! Desktop integration for portable builds (mainly the AppImage): installs
//! or removes a `.desktop` entry and icons in the user's XDG directories so
//! MavPlayer shows up in application menus and "Open With".

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

pub const APP_ID: &str = "io.github.drogobo.MavPlayer";

pub const MIME_TYPES: &str = "application/ogg;application/x-ogg;application/mxf;application/sdp;application/smil;\
application/x-smil;application/streamingmedia;application/x-streamingmedia;application/vnd.rn-realmedia;\
application/vnd.rn-realmedia-vbr;application/x-extension-m4a;application/x-extension-mp4;application/x-flac;\
application/x-flash-video;application/x-matroska;application/x-netshow-channel;application/x-quicktimeplayer;\
application/x-shorten;application/vnd.apple.mpegurl;application/x-mpegurl;application/x-cd-image;\
audio/3gpp;audio/3gpp2;audio/AMR;audio/aac;audio/ac3;audio/aiff;audio/amr-wb;audio/dv;audio/eac3;audio/flac;\
audio/m3u;audio/m4a;audio/mp1;audio/mp2;audio/mp3;audio/mp4;audio/mpeg;audio/mpeg2;audio/mpeg3;audio/mpegurl;\
audio/mpg;audio/musepack;audio/ogg;audio/opus;audio/rn-mpeg;audio/scpls;audio/vnd.dolby.heaac.1;\
audio/vnd.dolby.heaac.2;audio/vnd.dts;audio/vnd.dts.hd;audio/vnd.rn-realaudio;audio/vnd.wave;audio/vorbis;\
audio/wav;audio/webm;audio/x-aac;audio/x-adpcm;audio/x-aiff;audio/x-ape;audio/x-m4a;audio/x-matroska;\
audio/x-mp1;audio/x-mp2;audio/x-mp3;audio/x-mpegurl;audio/x-mpg;audio/x-ms-asf;audio/x-ms-wma;audio/x-musepack;\
audio/x-pls;audio/x-pn-au;audio/x-pn-realaudio;audio/x-pn-wav;audio/x-pn-windows-pcm;audio/x-realaudio;\
audio/x-scpls;audio/x-shorten;audio/x-tta;audio/x-vorbis;audio/x-vorbis+ogg;audio/x-wav;audio/x-wavpack;\
video/3gp;video/3gpp;video/3gpp2;video/avi;video/divx;video/dv;video/fli;video/flv;video/mkv;video/mp2t;\
video/mp4;video/mp4v-es;video/mpeg;video/msvideo;video/ogg;video/quicktime;video/vnd.avi;video/vnd.divx;\
video/vnd.mpegurl;video/vnd.rn-realvideo;video/webm;video/x-avi;video/x-flc;video/x-fli;video/x-flv;\
video/x-m4v;video/x-matroska;video/x-mpeg2;video/x-mpeg3;video/x-ms-afs;video/x-ms-asf;video/x-ms-wmv;\
video/x-ms-wmx;video/x-ms-wvxvideo;video/x-msvideo;video/x-ogm+ogg;video/x-theora;video/x-theora+ogg;\
x-content/video-dvd;x-scheme-handler/dvd;";

/// The .desktop entry. `exec` is the program to run, already quoted for
/// an `Exec` key ([`quote_exec`]); `tryexec` is its plain path.
pub fn desktop_entry(exec: &str, tryexec: &str, extra: &str) -> String {
    format!(
        "[Desktop Entry]\n\
Type=Application\n\
Name=MavPlayer\n\
GenericName=Media Player\n\
Comment=Play videos, music and DVDs\n\
Icon={APP_ID}\n\
Exec={exec} %U\n\
TryExec={tryexec}\n\
Terminal=false\n\
StartupNotify=true\n\
StartupWMClass={APP_ID}\n\
Categories=AudioVideo;Audio;Video;Player;TV;\n\
Keywords=video;movie;music;audio;dvd;player;mkv;mp4;\n\
MimeType={MIME_TYPES}\n\
Actions=OpenDisc;\n\
{extra}\
\n\
[Desktop Action OpenDisc]\n\
Name=Play DVD\n\
Exec={exec} dvd://\n",
        tryexec = escape_value(tryexec),
    )
}

pub fn appimage_path() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|p| p.exists())
}

pub fn is_appimage() -> bool {
    appimage_path().is_some()
}

fn data_home() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
}

fn desktop_file() -> PathBuf {
    data_home().join("applications").join(format!("{APP_ID}.desktop"))
}

fn icon_files() -> Vec<PathBuf> {
    let base = data_home().join("icons/hicolor");
    vec![base.join(format!("scalable/apps/{APP_ID}.svg")), base.join(format!("256x256/apps/{APP_ID}.png"))]
}

/// Is there a user-level entry pointing at this AppImage?
pub fn is_integrated() -> bool {
    std::fs::read_to_string(desktop_file()).map(|s| s.contains("X-MavPlayer-Integrated=true")).unwrap_or(false)
}

/// Escapes a value for a .desktop file (the "string" type).
fn escape_value(s: &str) -> String {
    s.replace('\\', "\\\\")
}

/// Quotes a program path for an `Exec` key, per the Desktop Entry spec:
/// reserved characters need double quotes, inside which `"`, `` ` ``, `$` and
/// `\` take a backslash; `%` is doubled (field codes), and finally the
/// general string escaping doubles every backslash again.
fn quote_exec(p: &Path) -> String {
    let s = p.display().to_string();
    let reserved = |c: char| " \t\n\"'\\><~|&;$*?#()`".contains(c);
    let quoted = if s.contains(reserved) {
        let mut q = String::from("\"");
        for c in s.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                q.push('\\');
            }
            q.push(c);
        }
        q.push('"');
        q
    } else {
        s
    };
    escape_value(&quoted.replace('%', "%%"))
}

/// The executable to put in a .desktop entry. Paths with control characters
/// (a line break would start a new key) can't be written safely.
fn entry_exe() -> anyhow::Result<PathBuf> {
    let exe = appimage_path()
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| anyhow::anyhow!("cannot find executable path"))?;
    anyhow::ensure!(
        !exe.to_string_lossy().chars().any(char::is_control),
        "the program's path contains control characters: {}",
        exe.display()
    );
    Ok(exe)
}

pub fn integrate() -> anyhow::Result<PathBuf> {
    let exe = entry_exe()?;
    let entry = desktop_entry(&quote_exec(&exe), &exe.display().to_string(), "X-MavPlayer-Integrated=true\n");
    let df = desktop_file();
    std::fs::create_dir_all(df.parent().unwrap())?;
    std::fs::write(&df, entry)?;
    let icons = icon_files();
    std::fs::create_dir_all(icons[0].parent().unwrap())?;
    std::fs::write(&icons[0], include_bytes!("../assets/app-icon.svg"))?;
    std::fs::create_dir_all(icons[1].parent().unwrap())?;
    std::fs::write(&icons[1], include_bytes!("../packaging/icons/mavplayer-256.png"))?;
    refresh_caches();
    Ok(df)
}

pub fn unintegrate() -> anyhow::Result<()> {
    let df = desktop_file();
    if df.exists() {
        std::fs::remove_file(&df)?;
    }
    for f in icon_files() {
        let _ = std::fs::remove_file(f);
    }
    refresh_caches();
    Ok(())
}

fn refresh_caches() {
    let apps = data_home().join("applications");
    let icons = data_home().join("icons/hicolor");
    let _ = std::process::Command::new("update-desktop-database").arg(&apps).output();
    let _ = std::process::Command::new("gtk-update-icon-cache").args(["-f", "-t"]).arg(&icons).output();
    let _ = std::process::Command::new("kbuildsycoca6").output();
}

/// Wayland compositors look up a window's icon through the `.desktop` file
/// whose name matches the window's app ID; there is no way for a GPUI app to
/// hand the compositor an icon directly. Installed packages (deb, rpm,
/// pkg.tar.zst, Flatpak) and integrated AppImages ship that file. For
/// everything else (`cargo run`, a tarball, a non-integrated AppImage) we
/// register a hidden, icon-only entry in the user's data directory: it has no
/// MIME types and `NoDisplay=true`, so it never shows up in menus or "Open
/// With". Returns quickly; cache refreshes run in the background.
pub fn ensure_window_icon() {
    if !cfg!(target_os = "linux") || std::env::var_os("FLATPAK_ID").is_some() {
        return;
    }
    let df = desktop_file();
    let user_entry = std::fs::read_to_string(&df).ok();
    let icon_only = user_entry.as_deref().is_some_and(|t| t.contains("X-MavPlayer-IconOnly=true"));
    if user_entry.is_some() && !icon_only {
        return; // a real (integrated) entry
    }
    if system_entry_installed() {
        // Installed from a package. A hidden entry left behind by an earlier
        // portable run would shadow the package's entry (user entries win),
        // hiding MavPlayer from menus and "Open With", so remove it.
        if icon_only {
            remove_icon_only_entry();
        }
        return;
    }
    let Ok(exe) = entry_exe() else { return };
    // An absolute icon path sidesteps icon-theme caches, so even the very
    // first window gets the icon.
    let svg = icon_files().remove(0);
    let entry = format!(
        "[Desktop Entry]\n\
Type=Application\n\
Name=MavPlayer\n\
Comment=Window icon for MavPlayer (created automatically; safe to delete)\n\
Icon={icon}\n\
Exec={exec} %U\n\
NoDisplay=true\n\
StartupWMClass={APP_ID}\n\
X-MavPlayer-IconOnly=true\n",
        icon = escape_value(&svg.display().to_string()),
        exec = quote_exec(&exe)
    );
    if user_entry.as_deref() == Some(entry.as_str()) && svg.exists() {
        return;
    }
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(svg.parent().unwrap())?;
        std::fs::write(&svg, include_bytes!("../assets/app-icon.svg"))?;
        std::fs::create_dir_all(df.parent().unwrap())?;
        std::fs::write(&df, &entry)
    };
    if let Err(e) = write() {
        log::debug!("could not register window icon: {e}");
    }
}

/// Is there a `.desktop` entry for MavPlayer in a system data directory
/// (from a deb, rpm or pkg.tar.zst)?
fn system_entry_installed() -> bool {
    let name = format!("{APP_ID}.desktop");
    let sys_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    sys_dirs.split(':').any(|d| Path::new(d).join("applications").join(&name).exists())
}

/// Deletes the hidden window-icon entry (and its icon) and refreshes the
/// desktop's caches in the background so the package's entry takes over.
fn remove_icon_only_entry() {
    let df = desktop_file();
    if let Err(e) = std::fs::remove_file(&df) {
        log::debug!("could not remove {}: {e}", df.display());
        return;
    }
    let _ = std::fs::remove_file(icon_files().remove(0));
    log::info!("removed the window-icon entry {}; the installed one is used now", df.display());
    std::thread::spawn(refresh_caches);
}

/// Marker so the first-run prompt is only shown once.
pub fn prompt_marker() -> PathBuf {
    crate::settings::config_dir().join("appimage-integration-asked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting_follows_the_spec() {
        assert_eq!(quote_exec(Path::new("/opt/MavPlayer.AppImage")), "/opt/MavPlayer.AppImage");
        assert_eq!(quote_exec(Path::new("/home/a b/M.AppImage")), r#""/home/a b/M.AppImage""#);
        // `$` and `` ` `` are escaped inside quotes, then the backslash itself is doubled.
        assert_eq!(quote_exec(Path::new("/x/$(id)`id`.AppImage")), r#""/x/\\$(id)\\`id\\`.AppImage""#);
        assert_eq!(quote_exec(Path::new("/x/50%.AppImage")), "/x/50%%.AppImage");
        assert_eq!(quote_exec(Path::new(r"/x/a\b c")), r#""/x/a\\\\b c""#);
    }
}
