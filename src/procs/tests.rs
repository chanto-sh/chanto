use super::*;

#[test]
fn reads_memory_and_the_host() {
    let total = total_memory().expect("total memory");
    // Any machine that builds Chanto has more than 512 MB.
    assert!(total > 512 * 1024 * 1024, "{total} bytes");
    assert!(host_name().is_some_and(|h| !h.is_empty()));
}

#[cfg(unix)]
#[test]
fn sees_a_child_through_its_life() {
    let me = std::process::id();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("sleep");
    let pid = child.id();

    let list = processes().expect("process table");
    let mine = list.iter().find(|p| p.pid == me).expect("this process");
    assert!(mine.memory > 0, "this process's memory");
    let kid = list.iter().find(|p| p.pid == pid).expect("the child");
    assert_eq!(kid.parent, Some(me));
    assert_eq!(kid.name, "sleep");
    assert_eq!(state(pid), Some(State::Running));

    // Killed but not yet reaped. Linux reports a zombie; macOS won't give a
    // zombie's details, so it still counts as running until reaped, as it
    // did through sysinfo.
    child.kill().expect("kill");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let unreaped = state(pid);
    if cfg!(target_os = "macos") {
        assert!(
            matches!(unreaped, Some(State::Running | State::Zombie)),
            "{unreaped:?}"
        );
    } else {
        assert_eq!(unreaped, Some(State::Zombie));
    }

    child.wait().expect("reap");
    assert_eq!(state(pid), Some(State::Gone));
}

#[test]
fn this_process_is_running() {
    assert_eq!(state(std::process::id()), Some(State::Running));
}

#[cfg(windows)]
#[test]
fn kill_ends_a_process() {
    let mut child = std::process::Command::new("cmd")
        .args(["/c", "ping -n 30 127.0.0.1 > NUL"])
        .spawn()
        .expect("cmd");
    let pid = child.id();
    assert_eq!(state(pid), Some(State::Running));
    kill(pid);
    child.wait().expect("reap");
    // Closing the last handle lets Windows retire the process.
    drop(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while state(pid) != Some(State::Gone) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(state(pid), Some(State::Gone));
}
