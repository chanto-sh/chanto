//! `chanto upgrade` (and `/upgrade`): move to the latest release the way this
//! binary was installed. Release lookup and install-method detection are
//! shared with the daily notice in `update_check.rs`.
//!
//! - Homebrew, npm and Scoop: the package manager owns the binary; Chanto runs
//!   its upgrade when the user confirms, then checks the new version really
//!   arrived.
//! - The installer (install.sh / install.ps1): download the release archive for
//!   this platform, check it against the release's `SHA256SUMS`, make sure the
//!   new binary runs, and swap it in (remove, then move into place: macOS kills
//!   a binary whose file is rewritten in place). Only after confirmation.
//! - cargo, a source checkout, or anything else: say what to run.
//!
//! Nothing is ever replaced without a prompt or `--yes`.

use crate::update_check::{self, InstallMethod};
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
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
    /// The Ed25519 public key (standard base64) the release's `SHA256SUMS`
    /// must be signed with: [`RELEASE_PUBKEY_B64`] outside tests.
    pub release_key: String,
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
            release_key: RELEASE_PUBKEY_B64.into(),
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
/// `SHA256SUMS` files are signed with, from v0.1.2 on. A valid `SHA256SUMS.sig`
/// is mandatory: a tampered or unsigned checksum file is refused rather than
/// trusted. The same key, as PEM, is `packaging/release-signing-key.pub`.
///
/// The private key is the `RELEASE_SIGNING_KEY` secret in CI (see
/// `.github/workflows/release.yml`), with an offline backup. To rotate it:
/// generate a key (`openssl genpkey -algorithm ed25519 -out release.pem`), put
/// its raw public key here
/// (`openssl pkey -in release.pem -pubout -outform DER | tail -c 32 | base64`)
/// and in the PEM file, and replace the secret.
const RELEASE_PUBKEY_B64: &str = "Hk8ypq/plTuzCUZ4P9+bmwesZLkwM18CBO7AcUQLGd0=";

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
pub async fn install_release(
    base_url: &str,
    tag: &str,
    triple: &str,
    dest: &Path,
    release_key: &str,
) -> Result<()> {
    let (top, name) = archive_name(tag, triple);
    let base = format!("{}/download/{tag}", base_url.trim_end_matches('/'));
    let sums_bytes = download(&format!("{base}/SHA256SUMS")).await?;
    // When a release signing key is configured, the checksum file itself must
    // carry a valid signature: this is what makes self-update tamper-evident
    // rather than trusting whatever SHA256SUMS the download host served. The
    // signature is verified over the exact bytes, before any checksum is read.
    if !release_key.is_empty() {
        use base64::Engine as _;
        let sig_b64 = download(&format!("{base}/SHA256SUMS.sig"))
            .await
            .context("this release should be signed but SHA256SUMS.sig could not be fetched; nothing was changed")?;
        let sig = base64::engine::general_purpose::STANDARD
            .decode(String::from_utf8_lossy(&sig_b64).trim())
            .context("SHA256SUMS.sig is not valid base64")?;
        verify_sums_signature(release_key, &sums_bytes, &sig)?;
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
    /// A package manager owns the binary and upgrades it on request.
    Package(Manager),
    Install,
}

/// A package manager that owns the binary, and the command that moves it on.
/// Chanto runs these itself, so every install it can upgrade is upgraded by
/// `chanto upgrade` rather than by a command the user has to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Manager {
    Brew,
    Npm,
    Scoop,
}

impl Manager {
    fn of(method: InstallMethod) -> Option<Manager> {
        match method {
            InstallMethod::Homebrew => Some(Manager::Brew),
            InstallMethod::Npm => Some(Manager::Npm),
            InstallMethod::Scoop => Some(Manager::Scoop),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Manager::Brew => "Homebrew",
            Manager::Npm => "npm",
            Manager::Scoop => "Scoop",
        }
    }

    fn program(self) -> &'static str {
        match self {
            Manager::Brew => "brew",
            Manager::Npm => "npm",
            Manager::Scoop => "scoop",
        }
    }

    /// The command that upgrades this install, as the user would type it.
    fn command(self) -> &'static str {
        match self {
            Manager::Brew => InstallMethod::Homebrew.command(),
            Manager::Npm => InstallMethod::Npm.command(),
            Manager::Scoop => InstallMethod::Scoop.command(),
        }
    }

    /// Where a release has to appear before the manager can install it.
    fn source(self) -> &'static str {
        match self {
            Manager::Brew => "the tap",
            Manager::Npm => "the registry",
            Manager::Scoop => "the bucket",
        }
    }

    /// Run the manager's upgrade, then ask the binary it manages what version
    /// it reports: the exit code can't say the new version arrived, since
    /// Homebrew on a stale tap and Scoop on a stale bucket exit 0 with nothing
    /// to do.
    fn upgrade(self, program: &OsStr, exe: &Path, latest: &str) -> Result<()> {
        match self {
            Manager::Brew => {
                // A failed refresh (offline, say) may still leave a tap new enough.
                let _ = std::process::Command::new(program).arg("update").status();
                run_manager(program, &["upgrade", "chanto"], "brew upgrade chanto")?;
                let prefix = manager_output(program, &["--prefix", "chanto"])?;
                let bin = Path::new(prefix.trim()).join("bin").join(binary_name());
                version_arrived(&bin, latest, self)
            }
            Manager::Npm => {
                run_manager(
                    program,
                    &["install", "-g", "@chanto/cli@latest"],
                    self.command(),
                )?;
                // npm replaces the package in place, so the path this binary
                // runs from is where the new one lands.
                version_arrived(exe, latest, self)
            }
            Manager::Scoop => {
                // `scoop update <app>` reads the bucket it already has; a
                // stale bucket is caught by the version check below.
                run_manager(program, &["update", "chanto"], "scoop update chanto")?;
                // Each version lives in its own directory and `current` moves
                // onto the new one, so the path this binary runs from is the
                // one it is leaving. The file keeps its place inside the app
                // directory.
                let prefix = manager_output(program, &["prefix", "chanto"])?;
                version_arrived(&scoop_current(exe, prefix.trim()), latest, self)
            }
        }
    }
}

/// Run one of the manager's commands, failing with what the user would type.
fn run_manager(program: &OsStr, args: &[&str], shown: &str) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running `{shown}`"))?;
    if !status.success() {
        bail!("`{shown}` failed ({status})");
    }
    Ok(())
}

/// The manager's stdout, for the paths it can tell us about.
fn manager_output(program: &OsStr, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running `{} {}`", program.to_string_lossy(), args.join(" ")))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What the manager left behind, from the binary it installed. Nothing
/// readable counts as the old version, not as success.
fn version_arrived(bin: &Path, latest: &str, manager: Manager) -> Result<()> {
    let printed = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let version = printed.rsplit(' ').next().unwrap_or_default();
    if version.is_empty() || update_check::newer(latest, version) {
        bail!(
            "{} didn't install {latest}: `chanto --version` still says {}. \
             A release can take a few minutes to reach {}. Try `{}`.",
            manager.name(),
            if version.is_empty() {
                "nothing"
            } else {
                version
            },
            manager.source(),
            manager.command()
        );
    }
    Ok(())
}

/// The binary inside Scoop's versioned app directory (`apps/chanto/<version>/…`),
/// kept at the same place under the directory `scoop prefix` now names.
fn scoop_current(exe: &Path, prefix: &str) -> PathBuf {
    let text = exe.to_string_lossy().replace('\\', "/");
    let lower = text.to_lowercase();
    let tail = lower
        .find("/scoop/apps/chanto/")
        .and_then(|at| text.get(at + "/scoop/apps/chanto/".len()..))
        .and_then(|rest| rest.split_once('/').map(|(_, tail)| tail));
    match tail {
        Some(tail) if !prefix.is_empty() => Path::new(prefix).join(tail),
        _ => exe.to_path_buf(),
    }
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
        method @ (InstallMethod::Homebrew | InstallMethod::Npm | InstallMethod::Scoop)
            if !opts.check =>
        {
            Plan::Package(Manager::of(method).expect("a package-managed install"))
        }
        InstallMethod::Installer if !opts.check => Plan::Install,
        // The manager owns the binary, so `chanto upgrade` is the command that
        // moves it on -- and the one the notice names.
        InstallMethod::Homebrew
        | InstallMethod::Npm
        | InstallMethod::Scoop
        | InstallMethod::Installer => Plan::Report(format!("{what} Run `chanto upgrade`.")),
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
        Plan::Package(m) => {
            let command = m.command();
            let q = format!(
                "chanto {latest} is available (you have v{}). {} manages this install. Run `{command}` now?",
                t.current,
                m.name()
            );
            if !(opts.yes || confirm(&q)) {
                return Ok(format!(
                    "chanto {latest} is available. Upgrade with `{command}`."
                ));
            }
            m.upgrade(OsStr::new(m.program()), &t.exe, &latest)?;
            Ok(format!("Upgraded Chanto to {latest} with {}.", m.name()))
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
            install_release(&t.base_url, &latest, triple, &t.exe, &t.release_key).await?;
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
Upgrades Chanto to the latest release the way it was installed: Homebrew, npm and Scoop run their\n\
own upgrade and the new version is checked, the installer's binary is replaced with the\n\
checksum-verified release, and cargo or source builds get the command to run. --check only reports;\n\
--yes skips the confirmation; --force reinstalls the latest.";

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
mod tests;
