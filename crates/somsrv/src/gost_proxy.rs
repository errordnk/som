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
//! ([`sync`]) owns it: it starts `gost` (found in `PATH`) as a socks5 proxy on a
//! free port of 127.0.0.1 when it starts or the setting becomes true, ends it
//! when the setting stops being true, and it ends with Som (a job object on
//! Windows, a parent-death signal on Linux). The port goes to `somsrv` in a
//! file of the temporary folder ([`port_file`]), which `somsrv` ([`proxy_url`])
//! reads for every request and trusts only while a socks5 proxy answers there.
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
    /// `true`: gost, started by Som.
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

/// Where Som tells `somsrv` the port: a file of the temporary folder.
pub fn port_file() -> PathBuf {
    std::env::temp_dir().join("som-yandex-gost-port")
}

fn read_port(file: &Path) -> Option<u16> {
    std::fs::read_to_string(file).ok()?.trim().parse().ok().filter(|port| *port != 0)
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

/// The gost this process started: it ends with the process.
static STARTED: Mutex<Option<std::process::Child>> = Mutex::new(None);

fn start_gost(program: &Path, port: u16) -> anyhow::Result<()> {
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
    log::info!("Yandex Music: started {} on port {port}", program.display());
    *STARTED.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
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

/// Ends the gost this process started, if any, and takes the port file away.
pub fn stop() {
    let started = STARTED.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut child) = started {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(port_file());
        log::info!("Yandex Music: gost ended");
    }
}

/// What Som does with the setting, at start and whenever it changes: `true` starts gost (unless the
/// one started here still runs), anything else ends it. Does not wait for the proxy to answer.
pub fn sync(proxy: &Proxy) {
    sync_with(proxy, find_in_path(gost_name(), std::env::var_os("PATH")), &port_file());
}

fn sync_with(proxy: &Proxy, gost: Option<PathBuf>, port_file: &Path) {
    if *proxy != Proxy::Gost {
        stop();
        return;
    }
    {
        let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
        // one that ended since (crashed, killed) is started again
        if started.as_mut().is_some_and(|child| child.try_wait().ok().flatten().is_none()) {
            return;
        }
    }
    let Some(program) = gost else {
        log::error!("music.yandex.proxy is true, but {} is not in PATH", gost_name());
        return;
    };
    let result = free_port().map_err(anyhow::Error::from).and_then(|port| {
        start_gost(&program, port)?;
        std::fs::write(port_file, port.to_string())?;
        Ok(())
    });
    if let Err(error) = result {
        log::error!("Yandex Music: {error}");
    }
}

/// The proxy URL for the requests of `proxy`. gost is Som's to start ([`sync`]): here its port is read
/// from [`port_file`], and waited for a moment (up to 3 s) in case Som is just starting it.
pub fn proxy_url(proxy: &Proxy) -> anyhow::Result<Option<String>> {
    proxy_url_at(proxy, &port_file(), Duration::from_secs(3))
}

fn proxy_url_at(proxy: &Proxy, port_file: &Path, wait: Duration) -> anyhow::Result<Option<String>> {
    match proxy {
        Proxy::None => Ok(None),
        Proxy::Url(url) => Ok(Some(url.clone())),
        Proxy::Gost => {
            let deadline = Instant::now() + wait;
            loop {
                if let Some(port) = read_port(port_file)
                    && socks5_answers(port)
                {
                    return Ok(Some(format!("socks5://127.0.0.1:{port}")));
                }
                if Instant::now() >= deadline {
                    anyhow::bail!("music.yandex.proxy is true, but no proxy answers: Som starts gost ({} must be in PATH) when it starts or the setting changes", gost_name());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
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

    /// A socks5 proxy that only says hello: the greeting is answered "no authentication".
    fn fake_socks5() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepting = listener.try_clone().unwrap();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = accepting.accept() {
                let mut greeting = [0u8; 3];
                if stream.read_exact(&mut greeting).is_ok() {
                    let _ = stream.write_all(&[5, 0]);
                }
            }
        });
        (listener, port)
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
        let (_proxy, port) = fake_socks5();
        assert!(socks5_answers(port));
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(!socks5_answers(other.local_addr().unwrap().port()), "listens, but does not answer the greeting");
        let free = free_port().unwrap();
        assert!(!socks5_answers(free));
    }

    #[test]
    fn the_port_from_the_file_is_the_address_when_a_proxy_answers_there() {
        let file = temp("port");
        let (_proxy, port) = fake_socks5();
        std::fs::write(&file, port.to_string()).unwrap();
        assert_eq!(proxy_url_at(&Proxy::Gost, &file, Duration::ZERO).unwrap(), Some(format!("socks5://127.0.0.1:{port}")));
        // a stale file (nothing answers on its port) is an error that says who starts gost
        std::fs::write(&file, free_port().unwrap().to_string()).unwrap();
        let error = proxy_url_at(&Proxy::Gost, &file, Duration::ZERO).unwrap_err().to_string();
        assert!(error.contains("Som starts gost"), "{error}");
        std::fs::remove_file(&file).unwrap();
        assert!(proxy_url_at(&Proxy::Gost, &file, Duration::ZERO).is_err(), "no file");
        // the user's own proxy and no proxy do not look at the file
        assert_eq!(proxy_url_at(&Proxy::Url("socks5://10.0.0.1:1".into()), &file, Duration::ZERO).unwrap().as_deref(), Some("socks5://10.0.0.1:1"));
        assert_eq!(proxy_url_at(&Proxy::None, &file, Duration::ZERO).unwrap(), None);
    }

    #[test]
    fn with_no_gost_in_path_nothing_is_started_and_stopping_nothing_is_fine() {
        let file = temp("nogost");
        sync_with(&Proxy::Gost, None, &file);
        assert!(!file.exists());
        sync_with(&Proxy::None, None, &file);
        stop();
    }

    /// Needs gost in PATH: `cargo test -p somsrv real_gost -- --ignored`. `sync` starts it on a free port and
    /// says which; the setting turned off ends it and takes the port away; and it ends with this process
    /// (checked by hand: no gost.exe left).
    #[test]
    #[ignore]
    fn real_gost_is_started_and_ended_by_the_setting() {
        sync(&Proxy::Gost);
        let url = proxy_url(&Proxy::Gost).unwrap().unwrap();
        let port = read_port(&port_file()).unwrap();
        assert_eq!(url, format!("socks5://127.0.0.1:{port}"));
        assert!(socks5_answers(port));
        // asked again: the same gost, the same port
        sync(&Proxy::Gost);
        assert_eq!(read_port(&port_file()), Some(port));
        // off: gone, and the file with it
        sync(&Proxy::None);
        std::thread::sleep(Duration::from_millis(500));
        assert!(!socks5_answers(port));
        assert!(!port_file().exists());
        // on again, on another port; left running: it must end with this process
        sync(&Proxy::Gost);
        assert!(proxy_url(&Proxy::Gost).is_ok());
    }
}
