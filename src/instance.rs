//! Hands media to an already-open MavPlayer. When the "When MavPlayer is
//! already open" setting isn't "Open a new window", a launch with files sends
//! them to the running window and exits before any window or GPU work.
//!
//! Transport: a Unix socket in the user's runtime directory (Linux, macOS),
//! or a loopback TCP port plus a random token kept in the config folder
//! (Windows). Requests are one JSON line, answered with `ok`.

use crate::engine::types::MediaSource;
use crate::settings::RunningMode;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_millis(1500);
/// Largest request accepted. Thousands of paths fit; a client can't make the
/// window buffer an endless line.
const MAX_REQUEST: u64 = 4 << 20;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub mode: RunningMode,
    pub sources: Vec<MediaSource>,
    #[serde(default)]
    token: String,
}

/// Sends `sources` to a running MavPlayer. Returns true once it has them.
pub fn forward(mode: RunningMode, sources: &[MediaSource]) -> bool {
    let Some(conn) = sys::connect() else { return false };
    let req = Request { mode, sources: sources.iter().map(absolute).collect(), token: sys::client_token() };
    let Ok(line) = serde_json::to_string(&req) else { return false };
    exchange(conn, &line).is_some_and(|ack| ack.trim() == "ok")
}

fn exchange(mut conn: impl Read + Write, line: &str) -> Option<String> {
    conn.write_all(line.as_bytes()).ok()?;
    conn.write_all(b"\n").ok()?;
    conn.flush().ok()?;
    let mut ack = String::new();
    BufReader::new(conn).read_line(&mut ack).ok()?;
    Some(ack)
}

/// Accepts requests from later launches on a background thread, unless
/// another MavPlayer is already doing so. Returns whether this one listens.
pub fn listen(on_request: impl Fn(Request) + Send + 'static) -> bool {
    let Some(listener) = sys::bind() else { return false };
    let spawned = std::thread::Builder::new().name("instance".into()).spawn(move || {
        loop {
            match listener.accept() {
                Ok(conn) => {
                    if let Some(req) = serve(conn) {
                        on_request(req);
                    }
                }
                Err(e) => {
                    log::debug!("instance socket: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
    if let Err(e) = spawned {
        log::warn!("could not start the instance listener: {e}");
        return false;
    }
    true
}

fn serve(conn: sys::Stream) -> Option<Request> {
    sys::set_timeouts(&conn);
    let mut reader = BufReader::new(conn.take(MAX_REQUEST));
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if !line.ends_with('\n') {
        log::warn!("ignored an incomplete or oversized open request");
        return None;
    }
    let req: Request = serde_json::from_str(&line).map_err(|e| log::warn!("bad open request: {e}")).ok()?;
    if !sys::token_ok(&req.token) {
        log::warn!("ignored an open request with a wrong token");
        return None;
    }
    let _ = reader.get_mut().get_mut().write_all(b"ok\n");
    log::debug!("received {} item(s) from another launch ({:?})", req.sources.len(), req.mode);
    Some(req)
}

/// The receiving process may run in another folder: make paths absolute.
fn absolute(s: &MediaSource) -> MediaSource {
    let abs = |p: &std::path::Path| -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
        }
    };
    match s {
        MediaSource::File(p) => MediaSource::File(abs(p)),
        MediaSource::Dvd { path, title, angle } if !path.is_empty() && std::path::Path::new(path).is_relative() => {
            MediaSource::Dvd {
                path: abs(std::path::Path::new(path)).to_string_lossy().into_owned(),
                title: *title,
                angle: *angle,
            }
        }
        other => other.clone(),
    }
}

#[cfg(unix)]
mod sys {
    use super::TIMEOUT;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    pub type Stream = UnixStream;

    pub struct Listener(UnixListener);

    impl Listener {
        pub fn accept(&self) -> std::io::Result<Stream> {
            self.0.accept().map(|(s, _)| s)
        }
    }

    /// Per user (and per sandbox), so only this user's launches can reach it.
    fn socket_path() -> PathBuf {
        // A separate config folder gets its own window to talk to. Name the
        // socket after it rather than putting it inside: socket paths are
        // limited to about 100 bytes.
        // Never fall back to the shared temp folder: another user could
        // create the socket there first and receive (or answer) requests.
        if let Some(d) = std::env::var_os("MAVPLAYER_CONFIG_DIR") {
            use std::hash::{Hash, Hasher};
            let Some(dir) = dirs::runtime_dir() else { return PathBuf::from(d).join("instance.sock") };
            let mut h = std::collections::hash_map::DefaultHasher::new();
            d.hash(&mut h);
            return dir.join(format!("mavplayer-{:016x}.sock", h.finish()));
        }
        match dirs::runtime_dir() {
            // Inside Flatpak only the app's own runtime folder is shared between instances.
            Some(rt) => match std::env::var("FLATPAK_ID") {
                Ok(id) => rt.join("app").join(id).join("mavplayer.sock"),
                Err(_) => rt.join("mavplayer.sock"),
            },
            None => crate::settings::config_dir().join("instance.sock"),
        }
    }

    pub fn connect() -> Option<Stream> {
        let s = UnixStream::connect(socket_path()).ok()?;
        set_timeouts(&s);
        Some(s)
    }

    pub fn bind() -> Option<Listener> {
        let path = socket_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let owner_only = |l: UnixListener| {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            Listener(l)
        };
        match UnixListener::bind(&path) {
            Ok(l) => Some(owner_only(l)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if UnixStream::connect(&path).is_ok() {
                    return None; // another window is listening
                }
                // Left behind by a MavPlayer that didn't exit cleanly.
                let _ = std::fs::remove_file(&path);
                UnixListener::bind(&path).ok().map(owner_only)
            }
            Err(e) => {
                log::debug!("instance socket {}: {e}", path.display());
                None
            }
        }
    }

    pub fn set_timeouts(s: &Stream) {
        let _ = s.set_read_timeout(Some(TIMEOUT));
        let _ = s.set_write_timeout(Some(TIMEOUT));
    }

    /// The socket's location already limits it to this user.
    pub fn client_token() -> String {
        String::new()
    }

    pub fn token_ok(_: &str) -> bool {
        true
    }
}

#[cfg(windows)]
mod sys {
    use super::TIMEOUT;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::OnceLock;

    pub type Stream = TcpStream;

    pub struct Listener(TcpListener);

    impl Listener {
        pub fn accept(&self) -> std::io::Result<Stream> {
            self.0.accept().map(|(s, _)| s)
        }
    }

    static TOKEN: OnceLock<String> = OnceLock::new();

    /// `<port> <token>`, readable only through the user's profile.
    fn info_path() -> PathBuf {
        crate::settings::config_dir().join("instance")
    }

    fn read_info() -> Option<(u16, String)> {
        let text = std::fs::read_to_string(info_path()).ok()?;
        let (port, token) = text.trim().split_once(' ')?;
        Some((port.parse().ok()?, token.to_string()))
    }

    pub fn connect() -> Option<Stream> {
        let (port, _) = read_info()?;
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let s = TcpStream::connect_timeout(&addr, TIMEOUT).ok()?;
        set_timeouts(&s);
        Some(s)
    }

    pub fn bind() -> Option<Listener> {
        if connect().is_some() {
            return None; // another window is listening
        }
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
        let port = l.local_addr().ok()?.port();
        let token = random_token();
        let path = info_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, format!("{port} {token}")).ok()?;
        let _ = TOKEN.set(token);
        Some(Listener(l))
    }

    fn random_token() -> String {
        use std::hash::{BuildHasher, Hasher};
        (0..4)
            .map(|i| {
                let mut h = std::collections::hash_map::RandomState::new().build_hasher();
                h.write_u64(i);
                h.write_u128(
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos(),
                );
                format!("{:016x}", h.finish())
            })
            .collect()
    }

    pub fn set_timeouts(s: &Stream) {
        let _ = s.set_read_timeout(Some(TIMEOUT));
        let _ = s.set_write_timeout(Some(TIMEOUT));
    }

    pub fn client_token() -> String {
        read_info().map(|(_, t)| t).unwrap_or_default()
    }

    /// Constant-time, so the token can't be guessed byte by byte from timing.
    pub fn token_ok(t: &str) -> bool {
        TOKEN.get().is_some_and(|mine| {
            !mine.is_empty()
                && mine.len() == t.len()
                && mine.bytes().zip(t.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
        })
    }
}
