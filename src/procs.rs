//! What Chanto needs to know about memory and processes: totals for the
//! memory limits, the process table for the watchdog, whether a process is
//! still running, and the host's name.
//!
//! On macOS this asks the kernel directly through libSystem; sysinfo would
//! link IOKit and CoreFoundation, which macOS then loads at every launch.
//! Elsewhere it's sysinfo, as before.

/// One process, as the memory watchdog sees it.
pub struct Proc {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Resident memory, in bytes.
    pub memory: u64,
    pub name: String,
}

/// Whether a process is running, a zombie (exited, not yet reaped), or gone.
/// `state` gives `None` where it can't tell (no process table), so callers
/// can do nothing rather than act on a guess.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    Running,
    Zombie,
    Gone,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{Proc, State};

    fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        unsafe {
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let n = libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            );
            (n == size).then_some(info)
        }
    }

    fn resident(pid: u32) -> u64 {
        // SAFETY: as above, for the task's memory counters.
        unsafe {
            let mut info: libc::proc_taskinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
            let n = libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTASKINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            );
            if n == size { info.pti_resident_size } else { 0 }
        }
    }

    fn c_name(bytes: &[libc::c_char]) -> String {
        let bytes: Vec<u8> = bytes
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub fn total_memory() -> Option<u64> {
        let mut v: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: sysctlbyname writes at most `len` bytes into `v`.
        let r = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                &mut v as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (r == 0 && v > 0).then_some(v)
    }

    pub fn processes() -> Option<Vec<Proc>> {
        // SAFETY: a null buffer asks for the number of processes.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return None;
        }
        // Room for processes started in between.
        let mut pids = vec![0 as libc::c_int; n as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
        // SAFETY: the buffer holds `bytes` bytes.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, bytes) };
        if n <= 0 {
            return None;
        }
        pids.truncate(n as usize);
        Some(
            pids.into_iter()
                .filter(|&p| p > 0)
                .filter_map(|p| {
                    let pid = p as u32;
                    // Another user's processes can't be read; they can't be
                    // Chanto's descendants either.
                    let info = bsd_info(pid)?;
                    let name = if info.pbi_name[0] != 0 {
                        c_name(&info.pbi_name)
                    } else {
                        c_name(&info.pbi_comm)
                    };
                    Some(Proc {
                        pid,
                        parent: (info.pbi_ppid != 0).then_some(info.pbi_ppid),
                        memory: resident(pid),
                        name,
                    })
                })
                .collect(),
        )
    }

    pub fn state(pid: u32) -> Option<State> {
        Some(match bsd_info(pid) {
            Some(info) if info.pbi_status == libc::SZOMB => State::Zombie,
            Some(_) => State::Running,
            // Unreadable (another user's) still exists; only "no such
            // process" means it's gone.
            None => {
                // SAFETY: signal 0 only checks the process exists.
                let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
                if r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
                    State::Running
                } else {
                    State::Gone
                }
            }
        })
    }

    pub fn host_name() -> Option<String> {
        let mut buf = [0 as libc::c_char; 256];
        // SAFETY: gethostname writes at most buf.len() bytes.
        let r = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
        let name = c_name(&buf);
        (r == 0 && !name.is_empty()).then_some(name)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{Proc, State};
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

    // `System::new` only fails on platforms sysinfo doesn't support; each
    // caller falls back to what never blocks work or kills anything.

    pub fn total_memory() -> Option<u64> {
        let mut s = System::new().ok()?;
        s.refresh_memory();
        Some(s.total_memory())
    }

    pub fn available_memory() -> Option<u64> {
        let mut s = System::new().ok()?;
        s.refresh_memory();
        Some(s.available_memory())
    }

    pub fn processes() -> Option<Vec<Proc>> {
        let mut s = System::new().ok()?;
        s.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        Some(
            s.processes()
                .iter()
                .map(|(pid, p)| Proc {
                    pid: pid.as_u32(),
                    parent: p.parent().map(|p| p.as_u32()),
                    memory: p.memory(),
                    name: p.name().to_string_lossy().into_owned(),
                })
                .collect(),
        )
    }

    pub fn state(pid: u32) -> Option<State> {
        let p = Pid::from_u32(pid);
        let mut s = System::new().ok()?;
        s.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[p]),
            true,
            ProcessRefreshKind::nothing(),
        );
        Some(match s.process(p) {
            Some(x) if x.status() == ProcessStatus::Zombie => State::Zombie,
            Some(_) => State::Running,
            None => State::Gone,
        })
    }

    pub fn host_name() -> Option<String> {
        System::host_name().ok()
    }

    /// Kill one process (Windows has no process groups to signal).
    #[cfg(not(unix))]
    pub fn kill(pid: u32) {
        let p = Pid::from_u32(pid);
        if let Ok(mut s) = System::new() {
            s.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[p]),
                true,
                ProcessRefreshKind::nothing(),
            );
            if let Some(x) = s.process(p) {
                x.kill();
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub use imp::available_memory;
#[cfg(not(unix))]
pub use imp::kill;
pub use imp::{host_name, processes, state, total_memory};

#[cfg(test)]
mod tests;
