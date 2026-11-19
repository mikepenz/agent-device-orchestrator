//! Everything platform-specific (unix process groups/signals vs windows taskkill/ctrlc) lives here.
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// `$HOME` (or `%USERPROFILE%` on windows); a clear error when unset/empty.
pub fn home_dir() -> Result<PathBuf, String> {
    let v = |k| std::env::var_os(k).filter(|h| !h.is_empty());
    #[cfg(windows)]
    let h = v("HOME").or_else(|| v("USERPROFILE"));
    #[cfg(not(windows))]
    let h = v("HOME");
    h.map(PathBuf::from).ok_or_else(|| "HOME is not set; set it or DEVQ_HOME".to_string())
}

/// Opens `url` in the default browser; on failure prints it instead.
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let r = Command::new("open").arg(url).spawn();
    #[cfg(windows)]
    let r = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = Command::new("xdg-open").arg(url).spawn();
    if r.is_err() {
        println!("open this URL in a browser: {url}");
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    pub fn pid_alive(pid: i64) -> bool {
        if pid <= 0 || pid > i32::MAX as i64 {
            return false;
        }
        let r = unsafe { libc::kill(pid as i32, 0) };
        !(r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
    }

    /// Own process group so a terminal Ctrl-C reaches only devq, which forwards it to the group.
    pub fn prepare_grouped(c: &mut Command) {
        c.process_group(0);
    }

    /// Signals process group `g` if it is sane (> 1, not our own) and exists; `sig` 0 just probes.
    /// Returns whether the group exists/was signalled.
    // ponytail: pid reuse is not detected (a recycled pgid could be hit); upgrade path is storing the
    // child's process start time and comparing it before signalling.
    fn killpg_safe(g: i64, sig: i32) -> bool {
        if g <= 1 || g > i32::MAX as i64 || g as i32 == unsafe { libc::getpgrp() } {
            return false;
        }
        unsafe { libc::killpg(g as i32, 0) == 0 && (sig == 0 || libc::killpg(g as i32, sig) == 0) }
    }

    pub fn kill_tree(id: i64, force: bool) -> bool {
        killpg_safe(id, if force { libc::SIGKILL } else { libc::SIGTERM })
    }
    pub fn interrupt(id: i64) -> bool {
        killpg_safe(id, libc::SIGINT)
    }
    pub fn group_exists(id: i64) -> bool {
        killpg_safe(id, 0)
    }

    pub fn exit_code(s: &ExitStatus) -> i32 {
        match (s.code(), s.signal()) {
            (Some(c), _) => c,
            (_, Some(sig)) => 128 + sig,
            _ => 1,
        }
    }

    /// SIGINT/SIGTERM set the given flags.
    pub fn install_signals(int: &Arc<AtomicBool>, term: &Arc<AtomicBool>) {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, int.clone());
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, term.clone());
    }

    #[cfg(target_os = "macos")]
    pub fn uid() -> u32 {
        unsafe { libc::getuid() }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    /// Alive = process exists and its exit code is 259 (`STILL_ACTIVE`; a process that really exits
    /// with 259 looks alive, a known Windows ambiguity). Access denied counts as alive (like EPERM).
    pub fn pid_alive(pid: i64) -> bool {
        if pid <= 0 || pid > u32::MAX as i64 {
            return false;
        }
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if h.is_null() {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }

    pub fn prepare_grouped(c: &mut Command) {
        c.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    /// Never signal system pids, ourselves, or dead processes.
    fn sane(id: i64) -> bool {
        !(id <= 4 || id == std::process::id() as i64 || id > u32::MAX as i64 || !pid_alive(id))
    }

    // ponytail: the "group id" is the leader's pid; taskkill /T walks its (live) child tree, so
    // grandchildren of an already-exited leader are not found. Upgrade path: a Job Object.
    // Windows has no graceful stop for console apps (taskkill without /F does nothing), so `force`
    // is ignored and the tree is always force-killed.
    pub fn kill_tree(id: i64, _force: bool) -> bool {
        sane(id)
            && Command::new("taskkill").args(["/PID", &id.to_string(), "/T", "/F"]).output().map(|o| o.status.success()).unwrap_or(false)
    }
    // ponytail: no console Ctrl-C delivery to another group; force-kill is the only reliable stop.
    pub fn interrupt(id: i64) -> bool {
        kill_tree(id, true)
    }
    pub fn group_exists(id: i64) -> bool {
        sane(id)
    }

    pub fn exit_code(s: &ExitStatus) -> i32 {
        s.code().unwrap_or(1)
    }

    /// Ctrl-C / Ctrl-Break set `int`; there is no SIGTERM, so `term` is never set.
    pub fn install_signals(int: &Arc<AtomicBool>, _term: &Arc<AtomicBool>) {
        let int = int.clone();
        let _ = ctrlc::set_handler(move || int.store(true, std::sync::atomic::Ordering::Relaxed));
    }
}

pub use imp::*;
