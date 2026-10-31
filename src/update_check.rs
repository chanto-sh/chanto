//! Release lookup and install-method detection, shared by the daily update
//! notice (here) and `chanto upgrade` (`update.rs`).
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdatesConfig {
    pub check: bool,
}
impl Default for UpdatesConfig {
    fn default() -> Self {
        Self { check: true }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    checked_at: u64,
    latest: Option<String>,
}
struct CheckLock(std::path::PathBuf);
impl Drop for CheckLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
async fn save_cache(path: &Path, cache: &Cache) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    tokio::fs::write(&tmp, serde_json::to_vec(cache)?).await?;
    // Windows cannot rename over an existing file.
    #[cfg(windows)]
    if tokio::fs::try_exists(path).await? {
        tokio::fs::remove_file(path).await?;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}
static STARTED: AtomicBool = AtomicBool::new(false);
static NOTICE: Mutex<Option<String>> = Mutex::new(None);

pub fn newer(latest: &str, current: &str) -> bool {
    fn version(s: &str) -> Option<(u64, u64, u64)> {
        let mut parts = s.strip_prefix('v').unwrap_or(s).split('.');
        let number = |part: &str| {
            if part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            part.parse::<u64>().ok()
        };
        let result = (
            number(parts.next()?)?,
            number(parts.next()?)?,
            number(parts.next()?)?,
        );
        if parts.next().is_some() {
            None
        } else {
            Some(result)
        }
    }
    matches!((version(latest), version(current)), (Some(a), Some(b)) if a > b)
}
fn due(checked_at: u64, now: u64) -> bool {
    checked_at == 0 || now.saturating_sub(checked_at) >= 86400
}
fn enabled(check: bool, ci: bool, opt_out: bool) -> bool {
    check && !ci && !opt_out
}

pub const LATEST_URL: &str = concat!(site_url!(), "/releases/latest");

/// The latest release tag (validated `vX.Y.Z`): one GitHub API request with a
/// two-second total timeout, so the notice never holds anything up.
pub async fn latest_version() -> anyhow::Result<String> {
    latest_version_at(LATEST_URL, Duration::from_secs(2)).await
}
pub(crate) async fn latest_version_at(url: &str, timeout: Duration) -> anyhow::Result<String> {
    let client = crate::http::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let value: serde_json::Value = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            concat!("chanto/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tag = value["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("release has no tag"))?;
    // Validate before including untrusted response text in a terminal notice.
    if !newer(tag, "0.0.0") {
        anyhow::bail!("invalid release version");
    }
    Ok(tag.to_string())
}

/// How this binary was installed, which decides how it is upgraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    /// install.sh / install.ps1 (a `chanto.install-method` marker next to the binary).
    Installer,
    Cargo,
    /// A build inside a source checkout (`target/debug`, `target/release`).
    Source,
    Unknown,
}

pub const CARGO_COMMAND: &str =
    "cargo install --git https://github.com/chanto-sh/chanto.git --locked --force chanto";
pub const INSTALL_COMMAND: &str = if cfg!(windows) {
    concat!("irm ", site_url!(), "/install.ps1 | iex")
} else {
    concat!("curl -fsSL ", site_url!(), "/install.sh | sh")
};

impl InstallMethod {
    /// From the binary's path and what sits next to it: the installer's
    /// marker, and cargo's `.crates2.json` listing a `chanto` binary.
    pub fn classify(exe: &Path, installer: bool, cargo: bool) -> InstallMethod {
        let path = exe.to_string_lossy().replace('\\', "/");
        if path.contains("/Cellar/chanto/") || path.contains("/homebrew/opt/chanto/") {
            InstallMethod::Homebrew
        } else if installer {
            InstallMethod::Installer
        } else if cargo || path.contains("/.cargo/bin/") {
            InstallMethod::Cargo
        } else if path.contains("/target/debug/") || path.contains("/target/release/") {
            InstallMethod::Source
        } else {
            InstallMethod::Unknown
        }
    }

    /// Look at the files next to `exe` (a couple of tiny reads). Homebrew's
    /// `bin/chanto` is a link into its Cellar, so this goes by the real file.
    pub fn detect(exe: &Path) -> InstallMethod {
        let real = exe.canonicalize();
        let exe = real.as_deref().unwrap_or(exe);
        let installer = std::fs::read_to_string(exe.with_file_name("chanto.install-method"))
            .is_ok_and(|s| s.trim() == "installer");
        let cargo = exe
            .parent()
            .and_then(Path::parent)
            .and_then(|root| std::fs::read_to_string(root.join(".crates2.json")).ok())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .is_some_and(|v| {
                v["installs"].as_object().is_some_and(|installs| {
                    installs.values().any(|entry| {
                        entry["bins"].as_array().is_some_and(|bins| {
                            bins.iter()
                                .any(|bin| bin == "chanto" || bin == "chanto.exe")
                        })
                    })
                })
            });
        InstallMethod::classify(exe, installer, cargo)
    }

    /// The command that upgrades this install, for the notice.
    pub fn command(self) -> &'static str {
        match self {
            InstallMethod::Homebrew => "brew upgrade chanto",
            InstallMethod::Cargo => CARGO_COMMAND,
            InstallMethod::Installer => "chanto upgrade",
            InstallMethod::Source => "git pull && cargo build --release",
            InstallMethod::Unknown => INSTALL_COMMAND,
        }
    }
}

pub fn take_notice() -> Option<String> {
    NOTICE.lock().ok()?.take()
}

static ANNOUNCED: Mutex<Option<String>> = Mutex::new(None);

/// How often a running session looks again, so a long conversation hears about
/// a release that shipped after it started.
const SESSION_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Whether `tag` earns a notice: newer than this binary, and not the tag the
/// previous notice already named.
fn worth_announcing(tag: &str, announced: Option<&str>) -> bool {
    newer(tag, env!("CARGO_PKG_VERSION")) && announced != Some(tag)
}

/// The one line the user sees, when `tag` is newer than this binary.
fn upgrade_notice(tag: &str, method: InstallMethod) -> Option<String> {
    newer(tag, env!("CARGO_PKG_VERSION")).then(|| {
        format!(
            "chanto {tag} is available (you have v{}) — run `{}`",
            env!("CARGO_PKG_VERSION"),
            method.command()
        )
    })
}

/// Queue the notice for `tag`, once: a re-check that finds the same release
/// again stays quiet, so a second reminder means a new release shipped.
fn announce(tag: &str, exe: &Path) {
    let Ok(mut announced) = ANNOUNCED.lock() else {
        return;
    };
    if !worth_announcing(tag, announced.as_deref()) {
        return;
    }
    let Some(text) = upgrade_notice(tag, InstallMethod::detect(exe)) else {
        return;
    };
    *announced = Some(tag.to_string());
    if let Ok(mut notice) = NOTICE.lock() {
        *notice = Some(text);
    }
}

/// The release the last check cached and its notice: the startup line, which
/// must not wait on the network.
fn cached_notice(path: &Path) -> Option<(String, String)> {
    let cache: Cache = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let tag = cache.latest?;
    let exe = std::env::current_exe().unwrap_or_default();
    let text = upgrade_notice(&tag, InstallMethod::detect(&exe))?;
    Some((tag, text))
}

/// The cached notice, for a startup screen that has no time for a request.
/// Recorded as announced so the background check doesn't repeat it.
pub fn known() -> Option<String> {
    let (tag, text) = cached_notice(&crate::config::home_dir().join("update-check.json"))?;
    if let Ok(mut announced) = ANNOUNCED.lock() {
        *announced = Some(tag);
    }
    Some(text)
}

/// `network`: whether this run may ask GitHub. Short print-mode runs pass
/// false so an exiting process never waits on the request; they only show a
/// notice an interactive session already cached. A networked run keeps
/// checking every `SESSION_INTERVAL` while the session lasts.
pub fn start(config: &UpdatesConfig, network: bool) {
    if !enabled(
        config.check,
        std::env::var_os("CI").is_some(),
        std::env::var("CHANTO_NO_UPDATE_CHECK").is_ok_and(|v| v == "1"),
    ) {
        return;
    }
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let home = crate::config::home_dir();
    let exe = std::env::current_exe().unwrap_or_default();
    tokio::spawn(async move {
        let path = home.join("update-check.json");
        let mut cache: Cache = tokio::fs::read(&path)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let now = epoch_secs();
        if network && due(cache.checked_at, now) {
            if tokio::fs::create_dir_all(&home).await.is_err() {
                return;
            }
            let lock_path = home.join("update-check.lock");
            // Recover a lock left by a terminated process; active checks take <= 2s.
            if tokio::fs::metadata(&lock_path)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(60))
            {
                let _ = tokio::fs::remove_file(&lock_path).await;
            }
            let Ok(lock_file) = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .await
            else {
                return;
            };
            drop(lock_file);
            let _lock = CheckLock(lock_path);
            // Another process may have refreshed between our read and lock acquisition.
            cache = tokio::fs::read(&path)
                .await
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            if due(cache.checked_at, now) {
                cache.checked_at = now;
                // Persist the attempt before networking, including cancelled/offline checks.
                if save_cache(&path, &cache).await.is_err() {
                    return;
                }
                if let Ok(tag) = latest_version().await {
                    cache.latest = Some(tag);
                }
                let _ = save_cache(&path, &cache).await;
            }
        }
        if let Some(tag) = cache.latest.clone() {
            announce(&tag, &exe);
        }
        // A session outlives releases: keep looking while it runs, so one that
        // ships after the session started still gets a reminder in the
        // transcript. Each tag is announced once (see `announce`).
        if !network {
            return;
        }
        loop {
            tokio::time::sleep(SESSION_INTERVAL).await;
            let Ok(tag) = latest_version().await else {
                continue;
            };
            // Keep the cache current, so the next process shows the startup
            // line at once instead of waiting out the daily throttle.
            let _ = save_cache(
                &path,
                &Cache {
                    checked_at: epoch_secs(),
                    latest: Some(tag.clone()),
                },
            )
            .await;
            announce(&tag, &exe);
        }
    });
}

/// Seconds since the epoch, for the cache's throttle stamp.
fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
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
        assert!(notice.contains("v99.0.0"), "{notice}");
        assert!(
            notice.contains(&format!("you have v{}", env!("CARGO_PKG_VERSION"))),
            "{notice}"
        );
        assert!(notice.contains("brew upgrade chanto"), "{notice}");
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
        assert_eq!(Homebrew.command(), "brew upgrade chanto");
        assert!(Cargo.command().starts_with("cargo install"));
        assert_eq!(Installer.command(), "chanto upgrade");
        assert!(Unknown.command().contains("install."));
    }

    #[cfg(unix)]
    #[test]
    fn detection_follows_a_link_into_the_homebrew_cellar() {
        let dir =
            std::env::temp_dir().join(format!("chanto-method-{}", uuid::Uuid::new_v4().simple()));
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
        let dir =
            std::env::temp_dir().join(format!("chanto-method-{}", uuid::Uuid::new_v4().simple()));
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
}
