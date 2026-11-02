use super::*;
#[tokio::test]
async fn release_request_uses_validated_tag_and_user_agent() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/repos/chanto-sh/chanto/releases/latest",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let n = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..n]).to_lowercase();
        assert!(request.starts_with("get /repos/chanto-sh/chanto/releases/latest http/1.1"));
        assert!(request.contains("user-agent: chanto/"));
        let body = r#"{"tag_name":"v9.8.7"}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    assert_eq!(
        latest_version_at(&url, Duration::from_secs(2))
            .await
            .unwrap(),
        "v9.8.7"
    );
    server.await.unwrap();
}
#[test]
fn versions() {
    assert!(newer("v1.10.0", "1.9.9"));
    for tag in ["v1.2.3", "v1.2.2", "v1.2.4-rc.1", "bad", "v1.2.4\n"] {
        assert!(!newer(tag, "1.2.3"));
    }
}
#[tokio::test]
async fn cache_roundtrip_and_lock_exclusion() {
    let home = std::env::temp_dir().join(format!("chanto-updates-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&home).await.unwrap();
    let path = home.join("update-check.json");
    let cache = Cache {
        checked_at: 123,
        latest: Some("v9.0.0".into()),
    };
    save_cache(&path, &cache).await.unwrap();
    let read: Cache = serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    assert!(!due(read.checked_at, 124));
    assert_eq!(read.latest.as_deref(), Some("v9.0.0"));
    let lock_path = home.join("update-check.lock");
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .await
        .unwrap();
    let lock = CheckLock(lock_path.clone());
    assert!(
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .await
            .is_err()
    );
    drop(file);
    drop(lock);
    assert!(!lock_path.exists());
    tokio::fs::remove_dir_all(home).await.unwrap();
}
#[test]
fn throttle() {
    assert!(due(0, 1));
    assert!(!due(100, 86499));
    assert!(due(100, 86500));
    assert!(!due(100, 99));
}

#[test]
fn a_release_is_announced_once_with_the_command_that_upgrades_it() {
    let notice = upgrade_notice("v99.0.0", InstallMethod::Homebrew).unwrap();
    // What it runs now, what it can move to, and the one command that does it.
    assert_eq!(
        notice,
        format!(
            "chanto v{} → v99.0.0 · run chanto upgrade",
            env!("CARGO_PKG_VERSION")
        )
    );
    // The caller dims or colours the line; backticks would show up literally.
    assert!(!notice.contains('`'), "{notice}");
    // Every install `chanto upgrade` can move on says so, whichever
    // package manager owns the binary.
    for method in [
        InstallMethod::Homebrew,
        InstallMethod::Npm,
        InstallMethod::Scoop,
        InstallMethod::Installer,
    ] {
        let notice = upgrade_notice("v99.0.0", method).unwrap();
        assert!(
            notice.ends_with("· run chanto upgrade"),
            "{method:?}: {notice}"
        );
    }
    // One it can't still names the command that does the job.
    let cargo = upgrade_notice("v99.0.0", InstallMethod::Cargo).unwrap();
    assert!(cargo.ends_with(CARGO_COMMAND), "{cargo}");
    assert!(upgrade_notice("v0.0.1", InstallMethod::Homebrew).is_none());
    // The same release a second time is old news; a newer one is not.
    assert!(worth_announcing("v99.0.0", None));
    assert!(!worth_announcing("v99.0.0", Some("v99.0.0")));
    assert!(worth_announcing("v99.1.0", Some("v99.0.0")));
    assert!(!worth_announcing("v0.0.1", None));
}

#[test]
fn the_cached_release_is_a_startup_notice() {
    let home = std::env::temp_dir().join(format!("chanto-known-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("update-check.json");
    assert!(cached_notice(&path).is_none(), "no cache, nothing to say");
    std::fs::write(&path, r#"{"checked_at":1,"latest":"v99.2.0"}"#).unwrap();
    let (tag, text) = cached_notice(&path).unwrap();
    assert_eq!(tag, "v99.2.0");
    assert!(text.contains("v99.2.0"), "{text}");
    std::fs::write(&path, r#"{"checked_at":1,"latest":"v0.0.1"}"#).unwrap();
    assert!(cached_notice(&path).is_none(), "an old cache says nothing");
    let _ = std::fs::remove_dir_all(&home);
}
#[test]
fn suppression() {
    assert!(enabled(true, false, false));
    assert!(!enabled(false, false, false));
    assert!(!enabled(true, true, false));
    assert!(!enabled(true, false, true));
    assert!(
        !toml::from_str::<crate::config::Config>("[updates]\ncheck = false")
            .unwrap()
            .updates
            .check
    );
}
#[test]
fn install_methods() {
    use InstallMethod::*;
    let m = |p: &str, installer, cargo| InstallMethod::classify(Path::new(p), installer, cargo);
    assert_eq!(
        m("/opt/homebrew/Cellar/chanto/1.0/bin/chanto", false, false),
        Homebrew
    );
    assert_eq!(
        m("/opt/homebrew/Cellar/chanto/1.0/bin/chanto", true, true),
        Homebrew
    );
    assert_eq!(m("/tmp/user/.cargo/bin/chanto", false, false), Cargo);
    assert_eq!(m("/tmp/custom/bin/chanto", false, true), Cargo);
    assert_eq!(m("/tmp/user/.local/bin/chanto", true, false), Installer);
    // The installer's marker wins over a stale cargo record.
    assert_eq!(m("/tmp/.cargo/bin/chanto", true, true), Installer);
    assert_eq!(m("/src/chanto/target/release/chanto", false, false), Source);
    assert_eq!(
        m("C:\\src\\chanto\\target\\debug\\chanto.exe", false, false),
        Source
    );
    assert_eq!(m("/usr/local/bin/chanto", false, false), Unknown);
    assert_eq!(Homebrew.command(), "brew update && brew upgrade chanto");
    assert!(Cargo.command().starts_with("cargo install"));
    assert_eq!(Installer.command(), "chanto upgrade");
    assert!(Unknown.command().contains("install."));
    // The notice names the channel command only where Chanto can't
    // upgrade itself.
    assert_eq!(Homebrew.notice_command(), "chanto upgrade");
    assert_eq!(Installer.notice_command(), "chanto upgrade");
    assert_eq!(Npm.notice_command(), "chanto upgrade");
    assert_eq!(Scoop.notice_command(), "chanto upgrade");
    assert!(Cargo.notice_command().starts_with("cargo install"));
}

#[cfg(unix)]
#[test]
fn detection_follows_a_link_into_the_homebrew_cellar() {
    let dir = std::env::temp_dir().join(format!("chanto-method-{}", uuid::Uuid::new_v4().simple()));
    let cellar = dir.join("Cellar/chanto/1.0.0/bin");
    std::fs::create_dir_all(&cellar).unwrap();
    std::fs::write(cellar.join("chanto"), "").unwrap();
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(cellar.join("chanto"), bin.join("chanto")).unwrap();
    assert_eq!(
        InstallMethod::detect(&bin.join("chanto")),
        InstallMethod::Homebrew
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn detection_reads_the_installer_marker_and_cargo_records() {
    let dir = std::env::temp_dir().join(format!("chanto-method-{}", uuid::Uuid::new_v4().simple()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("chanto");
    assert_eq!(InstallMethod::detect(&exe), InstallMethod::Unknown);
    std::fs::write(dir.join(".crates2.json"), r#"{"installs":{"chanto 0.1.0 (git+https://github.com/chanto-sh/chanto.git)":{"bins":["chanto"]}}}"#).unwrap();
    assert_eq!(InstallMethod::detect(&exe), InstallMethod::Cargo);
    std::fs::write(bin.join("chanto.install-method"), "installer\n").unwrap();
    assert_eq!(InstallMethod::detect(&exe), InstallMethod::Installer);
    let _ = std::fs::remove_dir_all(&dir);
}
