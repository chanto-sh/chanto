//! Keep agents from taking the machine down: cap how many subagents and
//! commands run at once, wait for memory before starting more work, and kill
//! a runaway child process (never Chanto itself) when memory gets critical.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ResourcesConfig {
    /// Subagents running at the same time (default 3); more wait their turn.
    pub max_parallel_agents: Option<usize>,
    /// Background agents alive at once, running or queued (default 8).
    pub max_background_agents: Option<usize>,
    /// Shell commands running at the same time across all agents (default 4).
    pub max_parallel_commands: Option<usize>,
    /// Don't start agents or commands while less than this much RAM is free
    /// (MB; default: 5% of RAM, at least 1024).
    pub min_free_memory_mb: Option<u64>,
    /// Kill the largest child process when Chanto's process tree exceeds
    /// this share of RAM (percent; default 70, 0 = off).
    pub memory_limit_percent: Option<u64>,
}

impl ResourcesConfig {
    pub fn merge(&mut self, o: ResourcesConfig) {
        macro_rules! m {
            ($($f:ident),*) => { $( if o.$f.is_some() { self.$f = o.$f; } )* };
        }
        m!(
            max_parallel_agents,
            max_background_agents,
            max_parallel_commands,
            min_free_memory_mb,
            memory_limit_percent
        );
    }
}

struct Limits {
    agents: Semaphore,
    commands: Semaphore,
    max_background: usize,
    min_free: u64,
    limit_pct: u64,
}

static LIMITS: OnceLock<Limits> = OnceLock::new();
/// Process groups the watchdog killed (so the tool result can say why).
static KILLED: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

fn total_mb() -> u64 {
    let mut s = System::new();
    s.refresh_memory();
    s.total_memory() / 1_048_576
}

fn limits() -> &'static Limits {
    LIMITS.get_or_init(|| init_limits(&ResourcesConfig::default()))
}

fn init_limits(c: &ResourcesConfig) -> Limits {
    let total = total_mb();
    Limits {
        agents: Semaphore::new(c.max_parallel_agents.unwrap_or(3).max(1)),
        commands: Semaphore::new(c.max_parallel_commands.unwrap_or(4).max(1)),
        max_background: c.max_background_agents.unwrap_or(8).max(1),
        min_free: c.min_free_memory_mb.unwrap_or((total / 20).max(1024)),
        limit_pct: c.memory_limit_percent.unwrap_or(70),
    }
}

/// Apply the config (once, at startup) and start the memory watchdog.
pub fn init(c: &ResourcesConfig, on_kill: impl Fn(String) + Send + Sync + 'static) {
    if LIMITS.set(init_limits(c)).is_err() {
        return;
    }
    // Outside a runtime (some tools and tests) there is nothing to watch.
    if let (true, Ok(rt)) = (
        limits().limit_pct > 0,
        tokio::runtime::Handle::try_current(),
    ) {
        rt.spawn(watchdog(on_kill));
    }
}

pub fn max_background_agents() -> usize {
    limits().max_background
}

/// Memory available for new work, in MB.
pub fn available_mb() -> u64 {
    let mut s = System::new();
    s.refresh_memory();
    #[cfg(target_os = "macos")]
    {
        // sysinfo's "available" is ~0 on macOS (it ignores compressible and
        // inactive memory); the kernel's own free percentage is what memory
        // pressure is based on.
        if let Some(pct) = macos_free_percent() {
            return s.total_memory() / 1_048_576 * pct / 100;
        }
    }
    s.available_memory() / 1_048_576
}

#[cfg(target_os = "macos")]
fn macos_free_percent() -> Option<u64> {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let name = c"kern.memorystatus_level";
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut v as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (r == 0 && (0..=100).contains(&v)).then_some(v as u64)
}

/// Wait (up to `patience`) until enough memory is free to start more work.
pub async fn wait_for_memory(what: &str, patience: Duration) -> Result<(), String> {
    let min = limits().min_free;
    let t0 = std::time::Instant::now();
    loop {
        let free = available_mb();
        if free >= min {
            return Ok(());
        }
        if t0.elapsed() >= patience {
            return Err(format!(
                "Not starting {what}: the system is low on memory ({free} MB free, need {min} MB). \
                 Wait for running agents or commands to finish, or do this work sequentially."
            ));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// A slot for running a subagent; held until it finishes.
pub async fn agent_slot() -> tokio::sync::SemaphorePermit<'static> {
    limits().agents.acquire().await.expect("semaphore closed")
}

/// A slot for running a shell command; held until it exits.
pub async fn command_slot() -> tokio::sync::SemaphorePermit<'static> {
    limits().commands.acquire().await.expect("semaphore closed")
}

/// Was this process group killed by the watchdog?
pub fn was_killed(pgid: u32) -> bool {
    KILLED
        .lock()
        .unwrap()
        .as_mut()
        .is_some_and(|k| k.remove(&pgid))
}

/// Whether a process is still running. Used to reap stale IDE bridge locks.
pub fn pid_alive(pid: u32) -> bool {
    let p = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(p).is_some()
}

/// Memory a process really uses. On macOS resident size leaves out compressed
/// and swapped pages, which is exactly where a runaway process's memory goes
/// under pressure; the physical footprint (Activity Monitor's "Memory") doesn't.
fn footprint(pid: Pid) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::proc_pid_rusage(
                pid.as_u32() as i32,
                libc::RUSAGE_INFO_V2,
                &mut info as *mut _ as *mut libc::rusage_info_t,
            )
        };
        if r == 0 {
            return Some(info.ri_phys_footprint);
        }
    }
    let _ = pid;
    None
}

/// Every few seconds: if Chanto's descendants use too much memory (or the
/// system is nearly out), kill the biggest descendant's process group.
async fn watchdog(on_kill: impl Fn(String) + Send + Sync + 'static) {
    let me = Pid::from_u32(std::process::id());
    let mut sys = System::new();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        sys.refresh_memory();
        let total = sys.total_memory().max(1);
        let critical = available_mb() < limits().min_free / 2;
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        let procs = sys.processes();
        let descends = |mut p: Pid| {
            for _ in 0..64 {
                match procs.get(&p).and_then(|x| x.parent()) {
                    Some(parent) if parent == me => return true,
                    Some(parent) => p = parent,
                    None => return false,
                }
            }
            false
        };
        let kids: Vec<(Pid, u64)> = procs
            .iter()
            .filter(|(pid, _)| **pid != me && descends(**pid))
            .map(|(pid, p)| (*pid, footprint(*pid).unwrap_or(p.memory())))
            .collect();
        let used: u64 = kids.iter().map(|(_, m)| m).sum();
        if std::env::var_os("CHANTO_DEBUG_WATCHDOG").is_some() {
            eprintln!(
                "[watchdog] procs={} kids={} used={}MB avail={}MB",
                procs.len(),
                kids.len(),
                used / 1_048_576,
                available_mb()
            );
        }
        let over = used * 100 / total >= limits().limit_pct;
        if !(over || critical && used > 512 * 1_048_576) {
            continue;
        }
        let Some((pid, mem)) = kids.iter().max_by_key(|(_, m)| *m).copied() else {
            continue;
        };
        let name = procs
            .get(&pid)
            .map(|p| p.name().to_string_lossy().to_string())
            .unwrap_or_default();
        #[cfg(unix)]
        {
            // Kill the whole process group (a build's compiler workers too).
            let pgid = unsafe { libc::getpgid(pid.as_u32() as i32) };
            if pgid > 0 && pgid as u32 != std::process::id() && unsafe { libc::getpgid(0) } != pgid
            {
                KILLED
                    .lock()
                    .unwrap()
                    .get_or_insert_with(Default::default)
                    .insert(pgid as u32);
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            } else {
                unsafe { libc::kill(pid.as_u32() as i32, libc::SIGKILL) };
            }
        }
        #[cfg(not(unix))]
        if let Some(p) = procs.get(&pid) {
            p.kill();
        }
        on_kill(format!(
            "Memory watchdog killed `{name}` (pid {pid}, {} MB): Chanto's processes were using {}% of RAM{}.",
            mem / 1_048_576,
            used * 100 / total,
            if critical {
                " and the system was nearly out of memory"
            } else {
                ""
            }
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_queries_work() {
        assert!(total_mb() > 0);
        assert!(available_mb() > 0);
        let _a = agent_slot().await;
        let _c = command_slot().await;
        assert!(
            wait_for_memory("test", Duration::from_millis(1))
                .await
                .is_ok()
                || available_mb() < limits().min_free
        );
    }
}
