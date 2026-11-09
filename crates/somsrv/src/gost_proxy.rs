//! A local proxy for Yandex Music that Som starts.
//!
//! Behind a VPN the service is closed (451 from Yandex's side), and the VPN
//! client can let a program round it, but a program as a whole: everything
//! `somsrv` fetches would leave the tunnel. So only Yandex Music's requests go
//! through a separate program, [gost](https://github.com/go-gost/gost), that the
//! user excludes from the VPN once (on Windows: its `gost.exe` in the VPN
//! client's list of programs that bypass it).
//!
//! `music.yandex.proxy: true` in `settings.json` asks for it, and Som
//! ([`sync`]) owns it: it starts `gost -L socks5://127.0.0.1:<port>`
//! (`music.yandex.gost_port`, 1080 by default; `music.yandex.gost` is the path of
//! the program, else `gost` in `~/.local/bin`, else of `PATH`)
//! when it starts or the setting becomes true, ends it when the setting
//! stops being true, and it ends with Som (a job object on Windows, a
//! parent-death signal on Linux). Something already listening on the port is used
//! as it is (gost the user started). `somsrv` only uses the proxy ([`proxy_url`]).
//!
//! `music.yandex.proxy` as a string is the address of a proxy of the user's own
//! and is used as it is.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 1080;

/// What `music.yandex.proxy` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Proxy {
    #[default]
    None,
    /// A proxy of the user's: `socks5://host:port`, `http://...`.
    Url(String),
    /// `true`: gost, started by Som.
    Gost,
}

/// The proxy and gost settings of `settings.json`'s `music.yandex`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    pub proxy: Proxy,
    pub gost: Option<String>,
    pub port: Option<u16>,
    /// `music.yandex.direct_interface`: `gost` leaves through this adapter (where its `interface` works: not on Windows).
    pub interface: Option<String>,
}

pub fn settings_from(yandex: &serde_json::Value) -> Settings {
    let text = |name: &str| yandex.get(name).and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
    let proxy = match yandex.get("proxy") {
        Some(serde_json::Value::Bool(true)) => Proxy::Gost,
        Some(serde_json::Value::String(url)) if !url.trim().is_empty() => Proxy::Url(url.trim().to_string()),
        _ => Proxy::None,
    };
    Settings {
        proxy,
        gost: text("gost"),
        port: yandex.get("gost_port").and_then(|v| v.as_u64()).and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0),
        interface: text("direct_interface"),
    }
}

/// Where the gost program is: `configured`, else the first of `candidates` that is a file.
pub fn find_gost(configured: Option<&str>, candidates: &[PathBuf]) -> Option<PathBuf> {
    if let Some(path) = configured {
        return Some(PathBuf::from(path));
    }
    candidates.iter().find(|path| path.is_file()).cloned()
}

fn default_candidates() -> Vec<PathBuf> {
    let name = if cfg!(windows) { "gost.exe" } else { "gost" };
    // where all the programs of ours live comes first, then PATH
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let mut found: Vec<PathBuf> = home.iter().map(|home| Path::new(home).join(".local").join("bin").join(name)).collect();
    found.extend(std::env::var_os("PATH").map(|path| std::env::split_paths(&path).map(|dir| dir.join(name)).collect::<Vec<_>>()).unwrap_or_default());
    found
}

/// The address `gost` is told to listen on.
pub fn listen_arg(port: u16, interface: Option<&str>) -> String {
    match interface {
        Some(name) if !cfg!(windows) => format!("socks5://127.0.0.1:{port}?interface={name}"),
        _ => format!("socks5://127.0.0.1:{port}"),
    }
}

fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(300)).is_ok()
}

/// Whether something listens on `port` within `seconds` (a proxy Som is just starting).
fn wait_for_port(port: u16, seconds: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if listening(port) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The gost this process started: it ends with the process.
static STARTED: Mutex<Option<std::process::Child>> = Mutex::new(None);

fn start_gost(program: &Path, listen: &str) -> anyhow::Result<()> {
    let mut guard = STARTED.lock().unwrap_or_else(|e| e.into_inner());
    // one that ended since (crashed, killed) is started again
    if let Some(child) = guard.as_mut()
        && child.try_wait().ok().flatten().is_none()
    {
        return Ok(());
    }
    let mut command = std::process::Command::new(program);
    command.args(["-L", listen]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // no console window of its own
        command.creation_flags(0x0800_0000);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // gost ends when somsrv does, however that happens
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    let child = command.spawn().map_err(|error| anyhow::anyhow!("starting {}: {error} (music.yandex.gost is its path)", program.display()))?;
    #[cfg(windows)]
    bind_to_this_process(&child);
    log::info!("Yandex Music: started {} {listen}", program.display());
    *guard = Some(child);
    Ok(())
}

/// A job that kills what is in it when the last handle to it closes, which is when this process ends.
#[cfg(windows)]
fn bind_to_this_process(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    static JOB: Mutex<Option<isize>> = Mutex::new(None);
    let mut job = JOB.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        let handle = match *job {
            Some(raw) => HANDLE(raw as *mut std::ffi::c_void),
            None => {
                let Ok(handle) = CreateJobObjectW(None, None) else { return };
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
                if SetInformationJobObject(handle, JobObjectExtendedLimitInformation, &info as *const _ as *const std::ffi::c_void, size).is_err() {
                    return;
                }
                *job = Some(handle.0 as isize);
                handle
            }
        };
        let _ = AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle()));
    }
}

/// Ends the gost this process started, if any.
pub fn stop() {
    let mut guard = STARTED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut child) = guard.take() {
        let _ = child.kill();
        let _ = child.wait();
        log::info!("Yandex Music: gost ended");
    }
}

/// What Som does with the settings, at start and whenever they change: `proxy: true` starts
/// gost (unless something listens on its port already), anything else ends the gost this
/// process started. Does not wait for the port.
pub fn sync(settings: &Settings) {
    if settings.proxy != Proxy::Gost {
        stop();
        return;
    }
    let port = settings.port.unwrap_or(DEFAULT_PORT);
    if listening(port) {
        return;
    }
    let Some(program) = find_gost(settings.gost.as_deref(), &default_candidates()) else {
        log::error!("music.yandex.proxy is true, but gost is not found: put it in ~/.local/bin or set music.yandex.gost");
        return;
    };
    if let Err(error) = start_gost(&program, &listen_arg(port, settings.interface.as_deref())) {
        log::error!("Yandex Music: {error}");
    }
}

/// The proxy URL for the requests of `settings`. gost is Som's to start ([`sync`]); here it is only
/// waited for a moment, in case Som is just starting it.
pub fn proxy_url(settings: &Settings) -> anyhow::Result<Option<String>> {
    match &settings.proxy {
        Proxy::None => Ok(None),
        Proxy::Url(url) => Ok(Some(url.clone())),
        Proxy::Gost => {
            let port = settings.port.unwrap_or(DEFAULT_PORT);
            if !wait_for_port(port, 3) {
                anyhow::bail!(
                    "music.yandex.proxy is true, but nothing listens on 127.0.0.1:{port}: Som starts gost (it must be in ~/.local/bin or PATH, or music.yandex.gost is its path) when it starts or the setting changes"
                );
            }
            Ok(Some(format!("socks5://127.0.0.1:{port}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn proxy_true_is_gost_a_string_is_the_users_own_proxy() {
        assert_eq!(settings_from(&json!({ "proxy": true })).proxy, Proxy::Gost);
        assert_eq!(settings_from(&json!({ "proxy": " socks5://10.0.0.1:1080 " })).proxy, Proxy::Url("socks5://10.0.0.1:1080".into()));
        for none in [json!({}), json!({ "proxy": false }), json!({ "proxy": "" }), json!({ "proxy": "  " })] {
            assert_eq!(settings_from(&none).proxy, Proxy::None, "{none}");
        }
        let settings = settings_from(&json!({ "proxy": true, "gost": "C:/x/gost.exe", "gost_port": 1081, "direct_interface": "eth0" }));
        assert_eq!((settings.gost.as_deref(), settings.port, settings.interface.as_deref()), (Some("C:/x/gost.exe"), Some(1081), Some("eth0")));
        assert_eq!(settings_from(&json!({ "gost_port": 0 })).port, None);
        assert_eq!(settings_from(&json!({ "gost_port": 70000 })).port, None);
    }

    #[test]
    fn the_configured_path_wins_then_the_first_candidate_that_is_a_file() {
        let dir = std::env::temp_dir().join(format!("somsrv-gost-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gost");
        std::fs::write(&file, "").unwrap();
        assert_eq!(find_gost(Some("custom"), &[file.clone()]), Some(PathBuf::from("custom")));
        assert_eq!(find_gost(None, &[dir.join("missing"), file.clone()]), Some(file));
        assert_eq!(find_gost(None, &[dir.join("missing")]), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_listen_address_names_the_adapter_where_gost_can_use_it() {
        assert_eq!(listen_arg(1080, None), "socks5://127.0.0.1:1080");
        let with = listen_arg(1081, Some("eth0"));
        assert_eq!(with, if cfg!(windows) { "socks5://127.0.0.1:1081" } else { "socks5://127.0.0.1:1081?interface=eth0" });
    }

    #[test]
    fn something_already_listening_is_found_and_a_free_port_is_not() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(wait_for_port(port, 1));
        drop(listener);
        assert!(!wait_for_port(port, 0));
    }

    #[test]
    fn a_proxy_that_is_not_there_is_an_error_that_says_who_starts_it() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let settings = Settings { proxy: Proxy::Gost, port: Some(port), ..Settings::default() };
        let error = proxy_url(&settings).unwrap_err().to_string();
        assert!(error.contains("Som starts gost"), "{error}");
        // a proxy that is there is the address; the user's own proxy is as given
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        assert_eq!(proxy_url(&settings).unwrap(), Some(format!("socks5://127.0.0.1:{port}")));
        drop(listener);
        let own = Settings { proxy: Proxy::Url("socks5://10.0.0.1:1".into()), ..Settings::default() };
        assert_eq!(proxy_url(&own).unwrap().as_deref(), Some("socks5://10.0.0.1:1"));
        assert_eq!(proxy_url(&Settings::default()).unwrap(), None);
    }

    #[test]
    fn sync_with_no_gost_to_find_starts_nothing_and_stopping_nothing_is_fine() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        sync(&Settings { proxy: Proxy::Gost, gost: Some("definitely/not/there/gost".into()), port: Some(port), interface: None });
        assert!(!listening(port));
        sync(&Settings::default());
        stop();
    }

    /// Needs gost: `cargo test -p somsrv real_gost -- --ignored`. The proxy is started by `sync`, ended by it
    /// when the setting is no longer true, and would end with this process (checked by hand: no gost.exe left).
    #[test]
    #[ignore]
    fn real_gost_is_started_and_ended_by_the_settings() {
        let port = 1085;
        let on = Settings { proxy: Proxy::Gost, port: Some(port), ..Settings::default() };
        sync(&on);
        assert!(wait_for_port(port, 5), "gost listens");
        assert_eq!(proxy_url(&on).unwrap().as_deref(), Some("socks5://127.0.0.1:1085"));
        // asked again: nothing more is started (the port is taken, gost would fail to bind)
        sync(&on);
        assert!(listening(port));
        // the setting turned off: gost ends
        sync(&Settings::default());
        std::thread::sleep(Duration::from_millis(500));
        assert!(!listening(port), "gost is gone");
        // and on again
        sync(&on);
        assert!(wait_for_port(port, 5));
        // left running: it must end with this process
    }
}
