use super::wrap_command;
use crate::shell::Flavor;
use std::path::Path;

#[test]
fn each_shell_gets_a_script_in_its_own_syntax() {
    let file = Path::new("cwd-file");
    let posix = wrap_command(Flavor::Posix, "", "ls", file);
    assert!(posix.starts_with("ls\n__hx_ec=$?\n") && posix.ends_with("exit $__hx_ec"));
    let ps = wrap_command(Flavor::PowerShell, "", "ls", file);
    assert!(ps.starts_with("ls\n$__hx_ok = $?;") && ps.contains("'cwd-file'"));
    assert!(!ps.contains("||") && !ps.contains("/dev/null"), "{ps}");
    assert_eq!(wrap_command(Flavor::Cmd, "", "dir", file), "dir");
}

/// PowerShell 5, the one every Windows has, parses this script.
#[cfg(windows)]
#[test]
fn powershell_reports_status_and_directory() {
    let dir = std::env::temp_dir().join(format!("chanto-wrap-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("cwd");
    let run = |command: &str| {
        let script = wrap_command(Flavor::PowerShell, "", command, &file);
        std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &script])
            .current_dir(&dir)
            .status()
            .unwrap()
            .code()
    };
    assert_eq!(run("cmd /c exit 0"), Some(0));
    assert_eq!(run("cmd /c exit 7"), Some(7));
    assert_eq!(run("Get-Item no-such-file-here"), Some(1));
    std::fs::create_dir(dir.join("sub")).unwrap();
    assert_eq!(run("cd sub"), Some(0));
    let ended = std::fs::read_to_string(&file).unwrap();
    assert!(ended.trim_end().ends_with("sub"), "{ended}");
    let _ = std::fs::remove_dir_all(&dir);
}
