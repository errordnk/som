use alacritty_terminal::tty::Pty;
use gpui::{Context, Task};
use parking_lot::{MappedRwLockReadGuard, Mutex, RwLock, RwLockReadGuard};
#[cfg(target_os = "windows")]
use std::num::NonZeroU32;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(target_os = "windows")]
use windows::Win32::{Foundation::HANDLE, System::Threading::GetProcessId};

use sysinfo::{Pid, Process, ProcessRefreshKind, RefreshKind, System, UpdateKind};

use crate::{Event, Terminal};

#[derive(Clone, Copy)]
pub struct ProcessIdGetter {
    handle: i32,
    fallback_pid: u32,
}

impl ProcessIdGetter {
    pub fn fallback_pid(&self) -> Pid {
        Pid::from_u32(self.fallback_pid)
    }
}

#[cfg(unix)]
impl ProcessIdGetter {
    fn new(pty: &Pty) -> ProcessIdGetter {
        ProcessIdGetter {
            handle: pty.file().as_raw_fd(),
            fallback_pid: pty.child().id(),
        }
    }

    fn pid(&self) -> Option<Pid> {
        // Negative pid means error.
        // Zero pid means no foreground process group is set on the PTY yet.
        // Avoid killing the current process by returning a zero pid.
        let pid = unsafe { libc::tcgetpgrp(self.handle) };
        if pid > 0 {
            return Some(Pid::from_u32(pid as u32));
        }

        if self.fallback_pid > 0 {
            return Some(Pid::from_u32(self.fallback_pid));
        }

        None
    }
}

#[cfg(windows)]
impl ProcessIdGetter {
    fn new(pty: &Pty) -> ProcessIdGetter {
        let child = pty.child_watcher();
        let handle = child.raw_handle();
        let fallback_pid = child.pid().unwrap_or_else(|| unsafe {
            NonZeroU32::new_unchecked(GetProcessId(HANDLE(handle as _)))
        });

        ProcessIdGetter {
            handle: handle as i32,
            fallback_pid: u32::from(fallback_pid),
        }
    }

    fn pid(&self) -> Option<Pid> {
        let pid = unsafe { GetProcessId(HANDLE(self.handle as _)) };
        // the GetProcessId may fail and returns zero, which will lead to a stack overflow issue
        if pid == 0 {
            // in the builder process, there is a small chance, almost negligible,
            // that this value could be zero, which means child_watcher returns None,
            // GetProcessId returns 0.
            if self.fallback_pid == 0 {
                return None;
            }
            return Some(Pid::from_u32(self.fallback_pid));
        }
        Some(Pid::from_u32(pid))
    }
}

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub name: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
}

/// Fetches Zed-relevant Pseudo-Terminal (PTY) process information
pub struct PtyProcessInfo {
    system: RwLock<System>,
    refresh_kind: ProcessRefreshKind,
    pid_getter: ProcessIdGetter,
    pub current: RwLock<Option<ProcessInfo>>,
    task: Mutex<Option<Task<()>>>,
    /// When the last refresh was started - see [`MIN_REFRESH_INTERVAL`].
    last_refresh: Mutex<Option<Instant>>,
    /// A refresh is already waiting for the end of the minimum interval.
    trailing_refresh_scheduled: AtomicBool,
}

/// The least time between two looks at the child process (its name and
/// working directory). Every look is a snapshot of ALL processes of the
/// machine plus reads of the child's memory - hundreds of processes and
/// thousands of threads - and used to run after every burst of terminal
/// output: a full-screen program redrawing once a second kept a thread
/// pool worker busy at 10+% of a core. A title that is up to a second
/// late is not noticed; the last look of a burst is not lost (see
/// `emit_title_changed_if_changed`).
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

impl PtyProcessInfo {
    pub fn new(pty: &Pty) -> PtyProcessInfo {
        let process_refresh_kind = ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_exe(UpdateKind::Always);
        let refresh_kind = RefreshKind::nothing().with_processes(process_refresh_kind);
        let system = System::new_with_specifics(refresh_kind);

        PtyProcessInfo {
            system: RwLock::new(system),
            refresh_kind: process_refresh_kind,
            pid_getter: ProcessIdGetter::new(pty),
            current: RwLock::new(None),
            task: Mutex::new(None),
            last_refresh: Mutex::new(None),
            trailing_refresh_scheduled: AtomicBool::new(false),
        }
    }

    pub fn pid_getter(&self) -> &ProcessIdGetter {
        &self.pid_getter
    }

    fn refresh(&self) -> Option<MappedRwLockReadGuard<'_, Process>> {
        let pid = self.pid_getter.pid()?;
        if self.system.write().refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            self.refresh_kind,
        ) == 1
        {
            RwLockReadGuard::try_map(self.system.read(), |system| system.process(pid)).ok()
        } else {
            None
        }
    }

    fn get_child(&self) -> Option<MappedRwLockReadGuard<'_, Process>> {
        let pid = self.pid_getter.fallback_pid();
        RwLockReadGuard::try_map(self.system.read(), |system| system.process(pid)).ok()
    }

    #[cfg(unix)]
    pub(crate) fn kill_current_process(&self) -> bool {
        let Some(pid) = self.pid_getter.pid() else {
            return false;
        };
        unsafe { libc::killpg(pid.as_u32() as i32, libc::SIGKILL) == 0 }
    }

    #[cfg(not(unix))]
    pub(crate) fn kill_current_process(&self) -> bool {
        self.refresh().is_some_and(|process| process.kill())
    }

    pub(crate) fn kill_child_process(&self) -> bool {
        self.get_child().is_some_and(|process| process.kill())
    }

    #[cfg(unix)]
    pub(crate) fn terminate_child_process(&self) -> bool {
        let pid = self.pid_getter.fallback_pid();
        unsafe { libc::killpg(pid.as_u32() as i32, libc::SIGTERM) == 0 }
    }

    #[cfg(not(unix))]
    pub(crate) fn terminate_child_process(&self) -> bool {
        false
    }

    fn load(&self) -> Option<ProcessInfo> {
        let process = self.refresh()?;
        let cwd = process.cwd().map_or(PathBuf::new(), |p| p.to_owned());

        let info = ProcessInfo {
            name: process.name().to_str()?.to_owned(),
            cwd,
            argv: process
                .cmd()
                .iter()
                .filter_map(|s| s.to_str().map(ToOwned::to_owned))
                .collect(),
        };
        *self.current.write() = Some(info.clone());
        Some(info)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn load_for_test(&self) -> Option<ProcessInfo> {
        self.load()
    }

    /// Updates the cached process info, emitting a [`Event::TitleChanged`] event if the Zed-relevant info has changed
    ///
    /// At most once per [`MIN_REFRESH_INTERVAL`]: a call inside the interval
    /// schedules ONE look for its end instead, so a change that happens
    /// right after a look is still picked up.
    pub fn emit_title_changed_if_changed(self: &Arc<Self>, cx: &mut Context<'_, Terminal>) {
        if self.task.lock().is_some() {
            return;
        }
        let since_last = self.last_refresh.lock().map(|at| at.elapsed());
        if let Some(since_last) = since_last
            && since_last < MIN_REFRESH_INTERVAL
        {
            if !self.trailing_refresh_scheduled.swap(true, Ordering::AcqRel) {
                let wait = MIN_REFRESH_INTERVAL - since_last;
                let this = self.clone();
                let timer = cx.background_executor().timer(wait);
                cx.spawn(async move |term, cx| {
                    timer.await;
                    this.trailing_refresh_scheduled.store(false, Ordering::Release);
                    term.update(cx, |_, cx| this.emit_title_changed_if_changed(cx)).ok();
                })
                .detach();
            }
            return;
        }
        *self.last_refresh.lock() = Some(Instant::now());
        let this = self.clone();
        let has_changed = cx.background_executor().spawn(async move {
            let previous = this.current.read().clone();
            let current = this.load();
            let has_changed = match (previous.as_ref(), current.as_ref()) {
                (None, None) => false,
                (Some(prev), Some(now)) => prev.cwd != now.cwd || prev.name != now.name,
                _ => true,
            };
            if has_changed {
                *this.current.write() = current;
            }
            has_changed
        });
        let this = Arc::downgrade(self);
        *self.task.lock() = Some(cx.spawn(async move |term, cx| {
            if has_changed.await {
                term.update(cx, |_, cx| cx.emit(Event::TitleChanged)).ok();
            }
            if let Some(this) = this.upgrade() {
                this.task.lock().take();
            }
        }));
    }

    pub fn pid(&self) -> Option<Pid> {
        self.pid_getter.pid()
    }
}
