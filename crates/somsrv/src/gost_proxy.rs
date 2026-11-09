//! A local proxy for Yandex Music that `somsrv` starts itself.
//!
//! Behind a VPN the service is closed (451 from Yandex's side), and the VPN
//! client can let a program round it, but a program as a whole: everything
//! `somsrv` fetches would leave the tunnel. So only Yandex Music's requests go
//! through a separate program, [gost](https://github.com/go-gost/gost), that the
//! user excludes from the VPN once (on Windows: its `gost.exe` in the VPN
//! client's list of programs that bypass it).
//!
//! `music.yandex.proxy: true` in `settings.json` asks for it: the first request
//! to Yandex starts `gost -L socks5://127.0.0.1:<port>` (`music.yandex.gost_port`,
//! 1080 by default; `music.yandex.gost` is the path of the program, else
//! `gost` of `PATH`, or `%USERPROFILE%\bin\gost.exe` on Windows), and the
//! requests use that proxy. Something already listening on the port is used as
//! it is (gost the user started, one an earlier `somsrv` left), and a gost
//! started here ends with `somsrv` (a job object on Windows, a parent-death
//! signal on Linux).
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
    /// `true`: gost, started by `somsrv`.
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
    let mut found: Vec<PathBuf> = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).map(|dir| dir.join(name)).collect()).unwrap_or_default();
    if cfg!(windows)
        && let Some(home) = std::env::var_os("USERPROFILE")
    {
        found.push(Path::new(&home).join("bin").join(name));
    }
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

/// Makes sure something listens on `port`: if not, `start` is run and the port waited for (up to 5 s).
fn ensure_listening(port: u16, start: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
    if listening(port) {
        return Ok(());
    }
    start()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if listening(port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("gost was started but nothing listens on 127.0.0.1:{port}")
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

/// The proxy URL for the requests of `settings`, starting gost when it asks for it.
pub fn proxy_url(settings: &Settings) -> anyhow::Result<Option<String>> {
    match &settings.proxy {
        Proxy::None => Ok(None),
        Proxy::Url(url) => Ok(Some(url.clone())),
        Proxy::Gost => {
            let port = settings.port.unwrap_or(DEFAULT_PORT);
            ensure_listening(port, || {
                let program = find_gost(settings.gost.as_deref(), &default_candidates())
                    .ok_or_else(|| anyhow::anyhow!("music.yandex.proxy is true, but gost is not found: put it in PATH (Windows: %USERPROFILE%\\bin) or set music.yandex.gost"))?;
                start_gost(&program, &listen_arg(port, settings.interface.as_deref()))
            })?;
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
    fn something_already_listening_is_used_and_gost_is_not_started() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        ensure_listening(port, || anyhow::bail!("must not be started")).unwrap();
    }

    #[test]
    fn a_port_nothing_listens_on_starts_gost_and_waits_for_it() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let mut started = None;
        // the "gost" opens the port a little later
        let result = ensure_listening(port, || {
            started = Some(std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
                std::thread::sleep(Duration::from_secs(2));
                drop(listener);
            }));
            Ok(())
        });
        assert!(result.is_ok(), "{result:?}");
        started.unwrap().join().unwrap();
        // and a start that fails is the error
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        assert!(ensure_listening(port, || anyhow::bail!("no gost")).unwrap_err().to_string().contains("no gost"));
    }

    /// Needs gost: `cargo test -p somsrv real_gost -- --ignored`. The process that started it is
    /// this test's: when it ends, gost must be gone too (checked by hand: no gost.exe left).
    #[test]
    #[ignore]
    fn real_gost_is_started_listens_and_ends_with_the_process() {
        let settings = Settings { proxy: Proxy::Gost, port: Some(1085), ..Settings::default() };
        let url = proxy_url(&settings).unwrap().unwrap();
        assert_eq!(url, "socks5://127.0.0.1:1085");
        assert!(listening(1085));
        // a second ask finds it listening and starts nothing
        assert_eq!(proxy_url(&settings).unwrap().unwrap(), url);
    }

    #[test]
    fn without_a_gost_the_error_says_where_to_put_it() {
        let settings = Settings { proxy: Proxy::Gost, gost: Some("definitely/not/there/gost".into()), port: Some(1), interface: None };
        let error = proxy_url(&settings).unwrap_err().to_string();
        assert!(error.contains("music.yandex.gost"), "{error}");
    }
}
