#![cfg(windows)]

// Compile the same native module used by the launcher. The tests themselves
// execute the built Chanto binary, so its CLI dispatch and pipe inheritance
// are covered too; a libtest binary cannot dispatch the private launch flag.
#[allow(dead_code)]
#[path = "../src/windows_sandbox.rs"]
mod windows_sandbox;

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use windows_sandbox::{LAUNCH_FLAG, Request};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        // Not under %TEMP%: production grants persist on the user's temp dir,
        // and the "outside" directory must not inherit them.
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("chanto-win-test-{}", uuid::Uuid::new_v4()));
        for dir in ["project", "outside", "temp", "home"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self(root)
    }
    fn request(&self, script: &str) -> Request {
        Request {
            program: PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32/cmd.exe"),
            args: vec!["/D".into(), "/C".into(), script.into()],
            cwd: self.0.join("project"),
            writable: vec![self.0.join("project"), self.0.join("temp")],
            parent_pid: std::process::id(),
        }
    }
    fn command(&self, request: &Request) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chanto"));
        cmd.arg(LAUNCH_FLAG)
            .arg(serde_json::to_string(request).unwrap())
            .env("CHANTO_HOME", self.0.join("home"))
            .env("CHANTO_COMPAT_HOME", self.0.join("home"))
            .env("TEMP", self.0.join("temp"))
            .env("TMP", self.0.join("temp"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }
    fn run(&self, request: &Request) -> Output {
        let mut child = self.command(request).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let out = child.wait_with_output().unwrap();
                panic!(
                    "sandbox launcher timed out: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait_with_output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn writes_are_confined_and_allow_write_is_honored() {
    let f = Fixture::new();
    let out = f.run(&f.request("echo ok>inside.txt"));
    assert!(out.status.success(), "{}", text(&out));
    assert!(f.0.join("project/inside.txt").exists());
    let outside = f.0.join("outside/new.txt");
    let mut req = f.request(&format!("echo blocked>\"{}\"", outside.display()));
    assert!(!f.run(&req).status.success());
    assert!(!outside.exists());
    let existing = f.0.join("outside/existing.txt");
    std::fs::write(&existing, "unchanged").unwrap();
    req.args[2] = format!("echo blocked>\"{}\"", existing.display());
    assert!(!f.run(&req).status.success());
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "unchanged");
    req.writable.push(f.0.join("outside"));
    let out = f.run(&req);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        std::fs::read_to_string(existing)
            .unwrap()
            .contains("blocked")
    );
}

#[test]
fn reads_are_unrestricted_and_grants_stay_on_writable_roots_only() {
    let f = Fixture::new();
    // Pre-existing files: readable everywhere, writable only in the project.
    std::fs::create_dir_all(f.0.join("project/sub")).unwrap();
    std::fs::write(f.0.join("project/sub/existing.txt"), "before").unwrap();
    std::fs::write(f.0.join("outside/existing.txt"), "outside").unwrap();
    let acl = |p: &str| {
        Command::new("icacls")
            .arg(f.0.join(p))
            .output()
            .unwrap()
            .stdout
    };
    let outside_before = acl("outside");
    let out = f.run(&f.request(
        "type sub\\existing.txt && type ..\\outside\\existing.txt && echo after>sub\\existing.txt && del sub\\existing.txt && rmdir sub",
    ));
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("before") && stdout.contains("outside"),
        "{stdout}"
    );
    assert!(!f.0.join("project/sub").exists());
    // The grant is persistent and idempotent; nothing touches other paths.
    let project_after = acl("project");
    let out = f.run(&f.request("echo again>again.txt"));
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(project_after, acl("project"));
    assert_eq!(outside_before, acl("outside"));
}

fn powershell(f: &Fixture, script: &str) -> Request {
    let mut req = f.request("");
    req.program = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    req.args = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        script.into(),
    ];
    req
}

#[test]
fn powershell_starts_and_writes_only_inside() {
    let f = Fixture::new();
    let outside = f.0.join("outside/ps.txt");
    let out = f.run(&powershell(
        &f,
        &format!(
            "'ok' | Out-File inside.txt; try {{ 'x' | Out-File -ErrorAction Stop '{}'; exit 2 }} catch {{ exit 0 }}",
            outside.display()
        ),
    ));
    assert!(out.status.success(), "{}", text(&out));
    assert!(f.0.join("project/inside.txt").exists());
    assert!(!outside.exists());
}

#[test]
fn killing_launcher_kills_running_descendants() {
    let f = Fixture::new();
    std::fs::write(
        f.0.join("project/child.ps1"),
        "'ready' | Out-File ready.txt; Start-Sleep 5; 'escaped' | Out-File escaped.txt",
    )
    .unwrap();
    let req = powershell(
        &f,
        r#"
$p = Start-Process powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File','child.ps1' -PassThru -NoNewWindow
while (!(Test-Path ready.txt)) { if ($p.HasExited) { exit 2 }; Start-Sleep -Milliseconds 20 }
Start-Sleep 60
"#,
    );
    let mut child = f.command(&req).spawn().unwrap();
    // Two PowerShell starts inside the sandbox; a busy CI runner has taken
    // longer than 30 seconds.
    let deadline = Instant::now() + Duration::from_secs(90);
    while !f.0.join("project/ready.txt").exists() && Instant::now() < deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "launcher exited before descendant started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let ready = f.0.join("project/ready.txt").exists();
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(ready, "descendant never started: {}", text(&out));
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        !f.0.join("project/escaped.txt").exists(),
        "descendant escaped the job"
    );
}

#[test]
fn sockets_can_listen() {
    let f = Fixture::new();
    let req = powershell(
        &f,
        r#"
$s = [Net.Sockets.Socket]::new([Net.Sockets.AddressFamily]::InterNetwork, [Net.Sockets.SocketType]::Stream, [Net.Sockets.ProtocolType]::Tcp)
try { $s.Bind([Net.IPEndPoint]::new([Net.IPAddress]::Loopback, 0)); $s.Listen(1); $s.Dispose(); exit 0 }
catch { Write-Error $_; exit 2 }
"#,
    );
    let out = f.run(&req);
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn descendants_are_killed_when_the_shell_exits_normally() {
    let f = Fixture::new();
    std::fs::write(
        f.0.join("project/child.ps1"),
        "'ready' | Out-File ready.txt; Start-Sleep 3; 'escaped' | Out-File escaped.txt",
    )
    .unwrap();
    let req = powershell(
        &f,
        r#"
$p = Start-Process powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File','child.ps1' -PassThru -NoNewWindow
while (!(Test-Path ready.txt)) { if ($p.HasExited) { exit 2 }; Start-Sleep -Milliseconds 20 }
exit 0
"#,
    );
    let out = f.run(&req);
    assert!(out.status.success(), "{}", text(&out));
    assert!(f.0.join("project/ready.txt").exists());
    std::thread::sleep(Duration::from_secs(4));
    assert!(!f.0.join("project/escaped.txt").exists());
}

#[test]
fn mechanism_probe_succeeds_and_is_cached() {
    assert!(
        windows_sandbox::available(),
        "Windows sandbox launch probe failed"
    );
    assert!(windows_sandbox::available());
}

#[test]
fn a_primed_cache_with_existing_files_is_writable() {
    let f = Fixture::new();
    // A cache with files in it before the sandbox ever saw it, as ~/.cargo would be.
    let cache = f.0.join("cache");
    std::fs::create_dir_all(cache.join("registry/index")).unwrap();
    let lock = cache.join("registry/index/.package-cache");
    std::fs::write(&lock, "old").unwrap();
    windows_sandbox::prime(std::slice::from_ref(&cache));
    let mut req = f.request(&format!("echo new>\"{}\"", lock.display()));
    req.writable.push(cache.clone());
    let out = f.run(&req);
    assert!(out.status.success(), "{}", text(&out));
    assert!(std::fs::read_to_string(&lock).unwrap().contains("new"));
    // Priming a missing path is a no-op, not an error.
    windows_sandbox::prime(&[f.0.join("no-such-cache")]);
}
