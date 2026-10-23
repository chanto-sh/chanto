//! `chanto upgrade` (and `/upgrade`): move to the latest release the way this
//! binary was installed. Release lookup and install-method detection are
//! shared with the daily notice in `update_check.rs`.
//!
//! - Homebrew: Homebrew owns the binary; say `brew upgrade chanto`, or run it
//!   when the user confirms.
//! - The installer (install.sh / install.ps1): download the release archive for
//!   this platform, check it against the release's `SHA256SUMS`, make sure the
//!   new binary runs, and swap it in (remove, then move into place: macOS kills
//!   a binary whose file is rewritten in place). Only after confirmation.
//! - cargo, a source checkout, or anything else: say what to run.
//!
//! Nothing is ever replaced without a prompt or `--yes`.

use crate::update_check::{self, InstallMethod};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const RELEASES: &str = concat!(site_url!(), "/releases");

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Only report whether a newer release exists.
    pub check: bool,
    /// Don't ask before upgrading (scripts).
    pub yes: bool,
    /// Reinstall the latest release even when it is the running version.
    pub force: bool,
}

/// Where to look and what to replace; tests point these at a local server.
pub struct Target {
    pub latest_url: String,
    /// Release downloads: `<base>/download/<tag>/<file>`.
    pub base_url: String,
    pub exe: PathBuf,
    pub method: InstallMethod,
    pub current: String,
}

impl Target {
    pub fn this_binary() -> Result<Target> {
        let exe = std::env::current_exe().context("finding the running executable")?;
        // Resolve symlinks (Homebrew's bin/ links into the Cellar) so detection
        // and replacement see the real file.
        let exe = crate::util::canonical(&exe).unwrap_or(exe);
        Ok(Target {
            latest_url: update_check::LATEST_URL.into(),
            base_url: std::env::var("CHANTO_RELEASE_BASE_URL").unwrap_or_else(|_| RELEASES.into()),
            method: InstallMethod::detect(&exe),
            exe,
            current: env!("CARGO_PKG_VERSION").into(),
        })
    }
}

/// The release target triple for this platform (the matrix in release.yml).
pub fn target_triple() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        (os, arch) => bail!("there is no prebuilt Chanto for {os}/{arch}; build it from source"),
    })
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "chanto.exe"
    } else {
        "chanto"
    }
}

/// The release archive's top-level directory and file name.
fn archive_name(tag: &str, triple: &str) -> (String, String) {
    let top = format!("chanto-{tag}-{triple}");
    let ext = if triple.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    (top.clone(), format!("{top}.{ext}"))
}

/// The digest for `name` in a `SHA256SUMS` file (`<hex>  <name>`, or
/// `<hex> *<name>` in sha256sum's binary mode). Only a full SHA-256 counts.
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The Ed25519 public key (standard base64 of the raw 32 bytes) that release
/// `SHA256SUMS` files are signed with. Empty until a release signing key is
/// generated and published: while empty, self-update is checksum-only (as it
/// has always been); once set, a valid `SHA256SUMS.sig` is *mandatory*, so a
/// tampered or unsigned checksum file is refused rather than trusted.
///
/// To turn signing on: generate a key
///   `openssl genpkey -algorithm ed25519 -out release.pem`
/// put the raw public key here
///   `openssl pkey -in release.pem -pubout -outform DER | tail -c 32 | base64`
/// and give CI the private key as the `RELEASE_SIGNING_KEY` secret (see
/// `.github/workflows/release.yml`). Keep `release.pem` offline.
const RELEASE_PUBKEY_B64: &str = "";

/// Verify a raw Ed25519 `signature` over `sums` against `pubkey_b64` (standard
/// base64 of the 32-byte key). Err on a bad key, a bad signature, or a mismatch.
fn verify_sums_signature(pubkey_b64: &str, sums: &[u8], signature: &[u8]) -> Result<()> {
    use base64::Engine as _;
    let key = base64::engine::general_purpose::STANDARD
        .decode(pubkey_b64.trim())
        .context("the release public key is not valid base64")?;
    aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, key)
        .verify(sums, signature)
        .map_err(|_| anyhow::anyhow!("the release signature does not verify; nothing was changed"))
}

/// Unpack the archive with the system `tar` (bsdtar on macOS and Windows 10+
/// also reads zip) and return the binary inside `<top>/`.
fn extract(archive: &Path, dir: &Path, top: &str) -> Result<PathBuf> {
    let flags = if archive.extension().is_some_and(|e| e == "zip") {
        "-xf"
    } else {
        "-xzf"
    };
    let status = std::process::Command::new("tar")
        .arg(flags)
        .arg(archive)
        .arg("-C")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .status()
        .context("running tar")?;
    if !status.success() {
        bail!("tar could not unpack {}", archive.display());
    }
    let bin = dir.join(top).join(binary_name());
    if !bin.is_file() {
        bail!("the release archive has no {top}/{}", binary_name());
    }
    Ok(bin)
}

/// Swap `new` in for `dest`. The new file is staged next to `dest` first, so
/// a failure leaves the old binary in place; then the old file is removed (on
/// Windows, where a running executable can't be removed, renamed aside to
/// `.old`) and the staged one moved into its place.
pub fn replace_binary(new: &Path, dest: &Path) -> Result<()> {
    let dir = dest
        .parent()
        .context("the binary has no parent directory")?;
    let staged = dir.join(format!(".{}.upgrade-{}", binary_name(), std::process::id()));
    std::fs::copy(new, &staged).with_context(|| {
        format!(
            "writing to {} (permission denied? rerun with the rights to change it)",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let swap = || -> Result<()> {
        if cfg!(windows) {
            let old = dest.with_extension("old");
            let _ = std::fs::remove_file(&old);
            if dest.exists() {
                std::fs::rename(dest, &old)
                    .with_context(|| format!("moving {} aside", dest.display()))?;
            }
        } else {
            match std::fs::remove_file(dest) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("removing {}", dest.display()));
                }
                _ => {}
            }
        }
        std::fs::rename(&staged, dest).with_context(|| format!("installing {}", dest.display()))
    };
    let r = swap();
    let _ = std::fs::remove_file(&staged);
    r
}

fn http() -> reqwest::Client {
    crate::http::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("chanto/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

async fn download(url: &str) -> Result<Vec<u8>> {
    let r = http()
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    let r = r
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    Ok(r.bytes()
        .await
        .with_context(|| format!("downloading {url}"))?
        .to_vec())
}

/// Download release `tag` for `triple`, verify it against the release's
/// SHA256SUMS, check that the binary runs, and put it at `dest`.
pub async fn install_release(base_url: &str, tag: &str, triple: &str, dest: &Path) -> Result<()> {
    let (top, name) = archive_name(tag, triple);
    let base = format!("{}/download/{tag}", base_url.trim_end_matches('/'));
    let sums_bytes = download(&format!("{base}/SHA256SUMS")).await?;
    // When a release signing key is configured, the checksum file itself must
    // carry a valid signature: this is what makes self-update tamper-evident
    // rather than trusting whatever SHA256SUMS the download host served. The
    // signature is verified over the exact bytes, before any checksum is read.
    if !RELEASE_PUBKEY_B64.is_empty() {
        use base64::Engine as _;
        let sig_b64 = download(&format!("{base}/SHA256SUMS.sig"))
            .await
            .context("this release should be signed but SHA256SUMS.sig could not be fetched; nothing was changed")?;
        let sig = base64::engine::general_purpose::STANDARD
            .decode(String::from_utf8_lossy(&sig_b64).trim())
            .context("SHA256SUMS.sig is not valid base64")?;
        verify_sums_signature(RELEASE_PUBKEY_B64, &sums_bytes, &sig)?;
    }
    let sums = String::from_utf8_lossy(&sums_bytes).to_string();
    let expected = sum_for(&sums, &name)
        .with_context(|| format!("the release's SHA256SUMS has no checksum for {name}"))?;
    let archive = download(&format!("{base}/{name}")).await?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        bail!(
            "checksum mismatch for {name}: SHA256SUMS says {expected}, the download is {actual}; nothing was changed"
        );
    }
    let work =
        std::env::temp_dir().join(format!("chanto-upgrade-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&work)?;
    let result = (|| -> Result<()> {
        let path = work.join(&name);
        std::fs::write(&path, &archive)?;
        let bin = extract(&path, &work, &top)?;
        // A binary that can't start here (wrong platform, broken download)
        // must never replace one that can.
        let ok = std::process::Command::new(&bin)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            bail!("the downloaded Chanto doesn't run on this machine; nothing was changed");
        }
        replace_binary(&bin, dest)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// What `upgrade` decides, before anything runs.
#[derive(Debug, PartialEq)]
enum Plan {
    UpToDate,
    Report(String),
    Brew,
    Install,
}

fn plan(t: &Target, latest: &str, opts: &Options) -> Plan {
    let newer = update_check::newer(latest, &t.current);
    let path = t.exe.display();
    if !newer && !opts.force {
        return Plan::UpToDate;
    }
    let what = if newer {
        format!("chanto {latest} is available (you have v{}).", t.current)
    } else {
        format!("Reinstalling Chanto {latest}.")
    };
    match t.method {
        InstallMethod::Homebrew if !opts.check => Plan::Brew,
        InstallMethod::Installer if !opts.check => Plan::Install,
        InstallMethod::Homebrew | InstallMethod::Installer => Plan::Report(format!(
            "{what} Run `{}`.",
            if t.method == InstallMethod::Homebrew {
                "brew upgrade chanto"
            } else {
                "chanto upgrade"
            }
        )),
        InstallMethod::Cargo => Plan::Report(format!(
            "{what}\nThis Chanto ({path}) was installed with cargo; upgrade it with:\n  {}",
            update_check::CARGO_COMMAND
        )),
        InstallMethod::Source => Plan::Report(format!(
            "{what}\nThis Chanto ({path}) is a build from a source checkout; update the checkout and rebuild (`git pull && cargo build --release`), or install a release with:\n  {}",
            update_check::INSTALL_COMMAND
        )),
        InstallMethod::Unknown => Plan::Report(format!(
            "{what}\nchanto can't tell how {path} was installed (not by the installer, Homebrew or cargo), so it won't replace it. \
             Upgrade it the way you installed it, or install the release with:\n  {}",
            update_check::INSTALL_COMMAND
        )),
    }
}

/// The upgrade itself; `confirm` asks the user (false when nobody can answer).
pub async fn run(
    t: &Target,
    opts: &Options,
    confirm: &mut (dyn FnMut(&str) -> bool + Send),
) -> Result<String> {
    let latest = match update_check::latest_version_at(&t.latest_url, Duration::from_secs(15)).await
    {
        Ok(v) => v,
        Err(e)
            if e.downcast_ref::<reqwest::Error>().and_then(|e| e.status())
                == Some(reqwest::StatusCode::NOT_FOUND) =>
        {
            bail!("no published release found at {} (none yet?)", t.latest_url)
        }
        Err(e) => return Err(e.context("looking up the latest release")),
    };
    match plan(t, &latest, opts) {
        Plan::UpToDate => Ok(format!(
            "chanto v{} is up to date (latest release {latest}).",
            t.current
        )),
        Plan::Report(s) => Ok(s),
        Plan::Brew => {
            let q = format!(
                "chanto {latest} is available (you have v{}). Homebrew manages this install. Run `brew upgrade chanto` now?",
                t.current
            );
            if !(opts.yes || confirm(&q)) {
                return Ok(format!(
                    "chanto {latest} is available. Upgrade with `brew upgrade chanto`."
                ));
            }
            let status = std::process::Command::new("brew")
                .args(["upgrade", "chanto"])
                .status()
                .context("running brew")?;
            if !status.success() {
                bail!("`brew upgrade chanto` failed ({status})");
            }
            Ok(format!("Upgraded Chanto to {latest} with Homebrew."))
        }
        Plan::Install => {
            let triple = target_triple()?;
            let q = format!(
                "Upgrade Chanto v{} → {latest} at {}?",
                t.current,
                t.exe.display()
            );
            if !opts.yes && !confirm(&q) {
                return Ok(format!(
                    "chanto {latest} is available; nothing was changed. Run `chanto upgrade --yes` to install it without asking."
                ));
            }
            install_release(&t.base_url, &latest, triple, &t.exe).await?;
            let mut msg = format!(
                "Upgraded Chanto v{} → {latest} ({}). New sessions use it; running ones keep the old version until restarted.",
                t.current,
                t.exe.display()
            );
            if crate::server::server_file().exists() {
                msg.push_str("\nThe background daemon keeps running the old version; `chanto daemon stop` lets it restart on the new one.");
            }
            Ok(msg)
        }
    }
}

/// Ask on the terminal; without one, the answer is no.
fn ask(q: &str) -> bool {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        eprintln!("{q} (no terminal to ask on; pass --yes)");
        return false;
    }
    print!("{q} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes")
}

pub const USAGE: &str = "usage: chanto upgrade [--check] [--yes] [--force]\n\n\
Upgrades Chanto to the latest release the way it was installed: Homebrew runs `brew upgrade chanto`,\n\
the installer's binary is replaced with the checksum-verified release, and cargo or source builds get\n\
the command to run. --check only reports; --yes skips the confirmation; --force reinstalls the latest.";

/// `chanto upgrade [--check] [--yes] [--force]`.
pub async fn cli(args: &[String]) -> Result<String> {
    let mut opts = Options::default();
    for a in args {
        match a.as_str() {
            "--check" | "-n" => opts.check = true,
            "--yes" | "-y" => opts.yes = true,
            "--force" | "-f" => opts.force = true,
            "-h" | "--help" => return Ok(USAGE.into()),
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    run(&Target::this_binary()?, &opts, &mut ask).await
}

/// `/upgrade`: report from inside a session. Replacing the binary under a
/// running session is left to `chanto upgrade` in a terminal.
pub async fn slash() -> String {
    let t = match Target::this_binary() {
        Ok(t) => t,
        Err(e) => return format!("Could not check for updates: {e:#}"),
    };
    let opts = Options {
        check: true,
        ..Default::default()
    };
    match run(&t, &opts, &mut |_| false).await {
        Ok(s) => s,
        Err(e) => format!("Could not check for updates: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_signature_verifies_and_rejects_tampering() {
        use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let kp = Ed25519KeyPair::generate().unwrap();
        let pubkey = b64.encode(kp.public_key().as_ref());
        let sums = b"abc123  chanto-v1-x86_64.tar.gz\n";
        let sig = kp.sign(sums);
        // A genuine signature over the exact bytes verifies.
        verify_sums_signature(&pubkey, sums, sig.as_ref()).unwrap();
        // A changed checksum file (one hex digit flipped) is rejected.
        let tampered = b"abc124  chanto-v1-x86_64.tar.gz\n";
        assert!(verify_sums_signature(&pubkey, tampered, sig.as_ref()).is_err());
        // A signature from a different key is rejected.
        let other = Ed25519KeyPair::generate().unwrap();
        assert!(
            verify_sums_signature(&b64.encode(other.public_key().as_ref()), sums, sig.as_ref())
                .is_err()
        );
        // A malformed public key is an error, not a panic.
        assert!(verify_sums_signature("not base64!!", sums, sig.as_ref()).is_err());
    }

    fn target(method: InstallMethod, exe: &Path) -> Target {
        Target {
            latest_url: String::new(),
            base_url: String::new(),
            exe: exe.into(),
            method,
            current: "1.2.3".into(),
        }
    }

    #[test]
    fn each_install_method_gets_its_own_plan() {
        let exe = Path::new("/x/bin/chanto");
        let o = Options::default();
        let check = Options {
            check: true,
            ..Default::default()
        };
        assert_eq!(
            plan(&target(InstallMethod::Installer, exe), "v1.2.3", &o),
            Plan::UpToDate
        );
        assert_eq!(
            plan(&target(InstallMethod::Installer, exe), "v1.2.2", &o),
            Plan::UpToDate,
            "never a downgrade"
        );
        assert_eq!(
            plan(
                &target(InstallMethod::Installer, exe),
                "v1.2.3",
                &Options {
                    force: true,
                    ..Default::default()
                }
            ),
            Plan::Install
        );
        assert_eq!(
            plan(&target(InstallMethod::Installer, exe), "v1.3.0", &o),
            Plan::Install
        );
        assert_eq!(
            plan(&target(InstallMethod::Homebrew, exe), "v1.3.0", &o),
            Plan::Brew
        );
        let Plan::Report(s) = plan(&target(InstallMethod::Homebrew, exe), "v1.3.0", &check) else {
            panic!()
        };
        assert!(s.contains("brew upgrade chanto"), "{s}");
        let Plan::Report(s) = plan(&target(InstallMethod::Installer, exe), "v1.3.0", &check) else {
            panic!()
        };
        assert!(
            s.contains("v1.3.0 is available (you have v1.2.3)") && s.contains("chanto upgrade"),
            "{s}"
        );
        let Plan::Report(s) = plan(&target(InstallMethod::Cargo, exe), "v1.3.0", &o) else {
            panic!()
        };
        assert!(s.contains("cargo install"), "{s}");
        let Plan::Report(s) = plan(&target(InstallMethod::Source, exe), "v1.3.0", &o) else {
            panic!()
        };
        assert!(s.contains("git pull"), "{s}");
        let Plan::Report(s) = plan(&target(InstallMethod::Unknown, exe), "v1.3.0", &o) else {
            panic!()
        };
        assert!(s.contains("won't replace it"), "{s}");
    }

    #[test]
    fn checksums_come_from_sha256sums() {
        let hex = "a".repeat(64);
        let sums = format!(
            "{hex}  chanto-v1.0.0-aarch64-apple-darwin.tar.gz\n{}  chanto-v1.0.0-x86_64-unknown-linux-musl.tar.gz\n{} *chanto-v1.0.0-x86_64-pc-windows-msvc.zip\n",
            "B".repeat(64),
            "c".repeat(64)
        );
        assert_eq!(
            sum_for(&sums, "chanto-v1.0.0-aarch64-apple-darwin.tar.gz"),
            Some(hex)
        );
        assert_eq!(
            sum_for(&sums, "chanto-v1.0.0-x86_64-unknown-linux-musl.tar.gz"),
            Some("b".repeat(64))
        );
        assert_eq!(
            sum_for(&sums, "chanto-v1.0.0-x86_64-pc-windows-msvc.zip"),
            Some("c".repeat(64))
        );
        assert_eq!(
            sum_for(&sums, "chanto-v1.0.0-aarch64-apple-darwin.tar"),
            None
        );
        assert_eq!(
            sum_for("abc  chanto.tar.gz", "chanto.tar.gz"),
            None,
            "not a full digest"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            archive_name("v1.0.0", "x86_64-pc-windows-msvc").1,
            "chanto-v1.0.0-x86_64-pc-windows-msvc.zip"
        );
        assert!(target_triple().is_ok(), "this platform has a release build");
    }

    #[test]
    fn replacing_keeps_the_old_binary_until_the_new_one_is_staged() {
        let dir =
            std::env::temp_dir().join(format!("chanto-replace-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join(binary_name());
        std::fs::write(&dest, b"old").unwrap();
        // A missing source fails before the old binary is touched.
        assert!(replace_binary(&dir.join("missing"), &dest).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        std::fs::write(dir.join("new"), b"new").unwrap();
        replace_binary(&dir.join("new"), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().contains("upgrade"))
            .collect();
        assert!(left.is_empty(), "staging files left: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A release server: the GitHub API's latest-release answer, SHA256SUMS
    /// and the archive, all from a local port.
    async fn serve(files: Vec<(String, Vec<u8>)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let files = files.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    let (status, body) = match files.iter().find(|(p, _)| *p == path) {
                        Some((_, b)) => ("200 OK", b.clone()),
                        None => ("404 Not Found", b"missing".to_vec()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        url
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_installer_path_verifies_and_swaps_in_the_release() {
        let dir = std::env::temp_dir().join(format!(
            "chanto-upgrade-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let triple = target_triple().unwrap();
        let tag = "v9.9.9";
        let (top, name) = archive_name(tag, triple);
        // The "release": a script standing in for the binary.
        std::fs::create_dir_all(dir.join("pkg").join(&top)).unwrap();
        let script = dir.join("pkg").join(&top).join("chanto");
        std::fs::write(&script, "#!/bin/sh\necho chanto 9.9.9\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(dir.join(&name))
            .arg("-C")
            .arg(dir.join("pkg"))
            .arg(&top)
            .status()
            .unwrap();
        assert!(ok.success());
        let archive = std::fs::read(dir.join(&name)).unwrap();
        let good = format!("{}  {name}\n", sha256_hex(&archive));
        let bad = format!("{}  {name}\n", "0".repeat(64));
        let latest = br#"{"tag_name":"v9.9.9"}"#.to_vec();
        let url = serve(vec![
            ("/latest".into(), latest.clone()),
            (
                format!("/good/download/{tag}/SHA256SUMS"),
                good.into_bytes(),
            ),
            (format!("/good/download/{tag}/{name}"), archive.clone()),
            (format!("/bad/download/{tag}/SHA256SUMS"), bad.into_bytes()),
            (format!("/bad/download/{tag}/{name}"), archive.clone()),
        ])
        .await;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("chanto");
        std::fs::write(&exe, b"old binary").unwrap();
        let mut t = Target {
            latest_url: format!("{url}/latest"),
            base_url: format!("{url}/bad"),
            exe: exe.clone(),
            method: InstallMethod::Installer,
            current: "1.0.0".into(),
        };

        // Declined: nothing changes.
        let mut asked = Vec::new();
        let out = run(&t, &Options::default(), &mut |q| {
            asked.push(q.to_string());
            false
        })
        .await
        .unwrap();
        assert!(out.contains("nothing was changed"), "{out}");
        assert!(asked[0].contains("v1.0.0 → v9.9.9"), "{asked:?}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old binary");

        // A checksum mismatch is refused.
        let err = run(
            &t,
            &Options {
                yes: true,
                ..Default::default()
            },
            &mut |_| true,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old binary");

        // Confirmed, with a good checksum: the release's binary is in place.
        t.base_url = format!("{url}/good");
        let out = run(&t, &Options::default(), &mut |_| true).await.unwrap();
        assert!(out.contains("Upgraded Chanto v1.0.0 → v9.9.9"), "{out}");
        let after = std::process::Command::new(&exe).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&after.stdout).trim(),
            "chanto 9.9.9"
        );

        // Now current: up to date.
        t.current = "9.9.9".into();
        let out = run(&t, &Options::default(), &mut |_| panic!("nothing to ask"))
            .await
            .unwrap();
        assert!(out.contains("up to date"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
