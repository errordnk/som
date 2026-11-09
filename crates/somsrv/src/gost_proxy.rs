//! A local proxy for Yandex Music that `somsrv` starts.
//!
//! Behind a VPN the service is closed (451 from Yandex's side), and the VPN
//! client can let a program round it, but a program as a whole: everything
//! `somsrv` fetches would leave the tunnel. So only Yandex Music's requests go
//! through a separate program, [gost](https://github.com/go-gost/gost), that the
//! user excludes from the VPN once (on Windows: its `gost.exe` in the VPN
//! client's list of programs that bypass it).
//!
//! `music.yandex.proxy: true` in `settings.json` asks for it. The daemon owns
//! gost, since it is the daemon that uses it, and it outlives Som (a radio goes on
//! when Som is closed): the first request to Yandex starts `gost` (found in
//! `PATH`) as a socks5 proxy on a free port of 127.0.0.1, the requests go through
//! it, and it ends with the daemon (a job object on Windows, a parent-death signal
//! on Linux). The settings are read for every request: when `proxy` is no longer
//! `true` the next request ends gost.
//!
//! `music.yandex.proxy` as a string is the address of a proxy of the user's own
//! and is used as it is.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What `music.yandex.proxy` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Proxy {
    #[default]
    None,
    /// A proxy of the user's: `socks5://host:port`, `http://...`.
    Url(String),
    /// `true`: gost, started here.
    Gost,
}

pub fn settings_from(yandex: &serde_json::Value) -> Proxy {
    match yandex.get("proxy") {
        Some(serde_json::Value::Bool(true)) => Proxy::Gost,
        Some(serde_json::Value::String(url)) if !url.trim().is_empty() => Proxy::Url(url.trim().to_string()),
        _ => Proxy::None,
    }
}

/// The first directory of `PATH` that has `name` as a file (not the current folder, which a program
/// started from `Command::new("gost")` would look in first on Windows).
fn find_in_path(name: &str, path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    std::env::split_paths(&path?).map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}

fn gost_name() -> &'static str {
    if cfg!(windows) { "gost.exe" } else { "gost" }
}

/// Whether a socks5 proxy answers on `port`: the greeting "no authentication" gets the same back.
fn socks5_answers(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(300)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let mut reply = [0u8; 2];
    stream.write_all(&[5, 1, 0]).is_ok() && stream.read_exact(&mut reply).is_ok() && reply == [5, 0]
}

/// A port nothing listens on now.
fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// The gost this process started, and its port: it ends with the process.
static STARTED: Mutex<Option<(std::process::Child, u16)>> = Mutex::new(None);

fn spawn_gost(program: &Path, port: u16) -> anyhow::Result<std::process::Child> {
    let mut command = std::process::Command::new(program);
    command
        .args(["-L", &format!("socks5://127.0.0.1:{port}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // no console window of its own
        command.creation_flags(0x0800_0000);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // gost ends when its parent does, however that happens
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    let child = command.spawn().map_err(|error| anyhow::anyhow!("starting {}: {error}", program.display()))?;
    #[cfg(windows)]
    bind_to_this_process(&child);
    Ok(child)
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
    let started = STARTED.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some((mut child, _)) = started {
        let _ = child.kill();
        let _ = child.wait();
        log::info!("Yandex Music: gost ended");
    }
}

/// The port of the gost that is running (started here and answering), else started now on a free port.
fn running_gost(gost: Option<PathBuf>) -> anyhow::Result<u16> {
    let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((child, port)) = started.as_mut() {
        // one that ended since (crashed, killed) is started again
        if child.try_wait().ok().flatten().is_none() && socks5_answers(*port) {
            return Ok(*port);
        }
        let _ = child.kill();
        let _ = child.wait();
        *started = None;
    }
    let program = gost.ok_or_else(|| anyhow::anyhow!("music.yandex.proxy is true, but {} is not in PATH", gost_name()))?;
    let port = free_port()?;
    let child = spawn_gost(&program, port)?;
    log::info!("Yandex Music: started {} on port {port}", program.display());
    *started = Some((child, port));
    // waited for with the lock held: a request that comes meanwhile waits for it, it does not start another
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if socks5_answers(port) {
            return Ok(port);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some((mut child, _)) = started.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    anyhow::bail!("{} was started but does not answer on 127.0.0.1:{port}", gost_name())
}

/// The proxy URL for the requests of `proxy`: the first request with `true` starts gost; a setting that is
/// no longer `true` ends it.
pub fn proxy_url(proxy: &Proxy) -> anyhow::Result<Option<String>> {
    proxy_url_with(proxy, || find_in_path(gost_name(), std::env::var_os("PATH")))
}

fn proxy_url_with(proxy: &Proxy, find_gost: impl FnOnce() -> Option<PathBuf>) -> anyhow::Result<Option<String>> {
    match proxy {
        Proxy::Gost => Ok(Some(format!("socks5://127.0.0.1:{}", running_gost(find_gost())?))),
        Proxy::Url(url) => {
            stop();
            Ok(Some(url.clone()))
        }
        Proxy::None => {
            stop();
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::net::TcpListener;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("somsrv-gost-{name}-{}", std::process::id()))
    }

    #[test]
    fn proxy_true_is_gost_a_string_is_the_users_own_proxy() {
        assert_eq!(settings_from(&json!({ "proxy": true })), Proxy::Gost);
        assert_eq!(settings_from(&json!({ "proxy": " socks5://10.0.0.1:1080 " })), Proxy::Url("socks5://10.0.0.1:1080".into()));
        for none in [json!(null), json!({}), json!({ "proxy": false }), json!({ "proxy": "" }), json!({ "proxy": "  " })] {
            assert_eq!(settings_from(&none), Proxy::None, "{none}");
        }
    }

    #[test]
    fn gost_is_found_in_path_not_in_the_current_folder() {
        let (one, two) = (temp("a"), temp("b"));
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        std::fs::write(two.join("gost"), "").unwrap();
        let path = std::env::join_paths([&one, &two]).unwrap();
        assert_eq!(find_in_path("gost", Some(path.clone())), Some(two.join("gost")));
        assert_eq!(find_in_path("missing", Some(path)), None);
        assert_eq!(find_in_path("gost", None), None);
        std::fs::remove_dir_all(one).unwrap();
        std::fs::remove_dir_all(two).unwrap();
    }

    #[test]
    fn a_socks5_proxy_is_told_from_anything_else_listening() {
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = proxy.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = proxy.accept() {
                let mut greeting = [0u8; 3];
                if stream.read_exact(&mut greeting).is_ok() {
                    let _ = stream.write_all(&[5, 0]);
                }
            }
        });
        assert!(socks5_answers(port));
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(!socks5_answers(other.local_addr().unwrap().port()), "listens, but does not answer the greeting");
        assert!(!socks5_answers(free_port().unwrap()));
    }

    #[test]
    fn with_no_gost_in_path_the_error_says_what_is_missing_and_the_other_settings_start_nothing() {
        let error = proxy_url_with(&Proxy::Gost, || None).unwrap_err().to_string();
        assert!(error.contains("not in PATH"), "{error}");
        assert_eq!(proxy_url_with(&Proxy::Url("socks5://10.0.0.1:1".into()), || None).unwrap().as_deref(), Some("socks5://10.0.0.1:1"));
        assert_eq!(proxy_url_with(&Proxy::None, || None).unwrap(), None);
    }

    /// Needs gost in PATH: `cargo test -p somsrv real_gost -- --ignored`. The first request starts it on a free
    /// port, the next ones use it, a setting that is no longer `true` ends it, and it ends with this process
    /// (checked by hand: no gost.exe left).
    #[test]
    #[ignore]
    fn real_gost_is_started_by_the_first_request_and_ended_by_the_setting() {
        let url = proxy_url(&Proxy::Gost).unwrap().unwrap();
        let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
        assert!(socks5_answers(port));
        assert_eq!(proxy_url(&Proxy::Gost).unwrap().unwrap(), url, "the same gost");
        assert_eq!(proxy_url(&Proxy::None).unwrap(), None);
        std::thread::sleep(Duration::from_millis(500));
        assert!(!socks5_answers(port), "gone");
        // asked for again: started again; left running: it must end with this process
        assert!(proxy_url(&Proxy::Gost).unwrap().is_some());
    }
}
