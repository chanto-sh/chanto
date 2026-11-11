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
        release_key: RELEASE_PUBKEY_B64.into(),
    }
}

/// A `brew` whose tap only has the new version after `brew update`, as with
/// the real one on a stale tap.
#[cfg(unix)]
fn fake_brew(dir: &Path, installed: &str, update_works: bool, upgrade_exit: u8) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("prefix/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let chanto = bin.join("chanto");
    std::fs::write(&chanto, format!("#!/bin/sh\necho chanto {installed}\n")).unwrap();
    std::fs::set_permissions(&chanto, std::fs::Permissions::from_mode(0o755)).unwrap();
    let brew = dir.join("brew");
    let new = "1.3.0";
    let refreshed = dir.join("refreshed");
    std::fs::write(
        &brew,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{log}'\ncase \"$1\" in\n\
                 update) {update} ;;\n\
                 upgrade) [ {upgrade_exit} = 0 ] || exit {upgrade_exit}\n\
                 if [ -e '{refreshed}' ]; then printf '#!/bin/sh\\necho chanto {new}\\n' > '{chanto}'; fi ;;\n\
                 --prefix) echo '{prefix}' ;;\nesac\n",
            log = dir.join("calls").display(),
            update = if update_works {
                format!("touch '{}'", refreshed.display())
            } else {
                "exit 1".to_string()
            },
            refreshed = refreshed.display(),
            chanto = chanto.display(),
            prefix = dir.join("prefix").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&brew, std::fs::Permissions::from_mode(0o755)).unwrap();
    brew
}

#[cfg(unix)]
#[test]
fn brew_refreshes_a_stale_tap_before_upgrading() {
    let dir = tempfile_dir("brew-stale");
    let brew = fake_brew(&dir, "1.2.0", true, 0);
    Manager::Brew
        .upgrade(brew.as_os_str(), Path::new("/x/chanto"), "v1.3.0")
        .unwrap();
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
    assert!(calls.starts_with("update\nupgrade chanto\n"), "{calls}");
}

/// `brew upgrade` exits 0 with nothing to do, which was reported as
/// "Upgraded Chanto to v1.3.0" with the old version still installed.
#[cfg(unix)]
#[test]
fn brew_leaving_the_old_version_is_an_error() {
    let dir = tempfile_dir("brew-noop");
    let brew = fake_brew(&dir, "1.2.0", false, 0);
    let e = Manager::Brew
        .upgrade(brew.as_os_str(), Path::new("/x/chanto"), "v1.3.0")
        .unwrap_err()
        .to_string();
    assert!(e.contains("didn't install v1.3.0"), "{e}");
    assert!(e.contains("1.2.0"), "{e}");
    assert!(e.contains("brew update"), "{e}");
}

#[cfg(unix)]
#[test]
fn a_failing_brew_upgrade_is_reported() {
    let dir = tempfile_dir("brew-fail");
    let brew = fake_brew(&dir, "1.2.0", true, 3);
    let e = Manager::Brew
        .upgrade(brew.as_os_str(), Path::new("/x/chanto"), "v1.3.0")
        .unwrap_err()
        .to_string();
    assert!(e.contains("failed"), "{e}");
}

#[cfg(unix)]
#[test]
fn npm_installs_and_the_new_version_is_checked() {
    let dir = tempfile_dir("npm-upgrade");
    let bin = dir.join("node_modules/@chanto/cli/bin/chanto");
    script(&bin, "#!/bin/sh\necho chanto 1.2.0\n");
    let npm = dir.join("npm");
    script(
        &npm,
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{calls}'\nprintf '#!/bin/sh\\necho chanto 1.3.0\\n' > '{bin}'\n",
            calls = dir.join("calls").display(),
            bin = bin.display(),
        ),
    );
    Manager::Npm
        .upgrade(npm.as_os_str(), &bin, "v1.3.0")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("calls")).unwrap().trim(),
        "install -g @chanto/cli@latest"
    );
}

/// npm exits 0 for the version the registry already had; the binary it
/// leaves behind is what says whether that was the new one.
#[cfg(unix)]
#[test]
fn npm_claiming_a_version_it_did_not_install_is_an_error() {
    let dir = tempfile_dir("npm-noop");
    let bin = dir.join("chanto");
    script(&bin, "#!/bin/sh\necho chanto 1.2.0\n");
    let npm = dir.join("npm");
    script(&npm, "#!/bin/sh\nexit 0\n");
    let e = Manager::Npm
        .upgrade(npm.as_os_str(), &bin, "v1.3.0")
        .unwrap_err()
        .to_string();
    assert!(e.contains("npm didn't install v1.3.0"), "{e}");
    assert!(e.contains("1.2.0"), "{e}");
    assert!(e.contains("npm install -g @chanto/cli@latest"), "{e}");
}

/// Scoop keeps each version in its own directory and moves `current` onto
/// the new one, so the path the running binary came from is the one it is
/// leaving.
#[cfg(unix)]
#[test]
fn scoop_upgrade_checks_the_version_scoop_now_points_at() {
    let dir = tempfile_dir("scoop-upgrade");
    let old = dir.join("scoop/apps/chanto/1.2.0/chanto");
    let new = dir.join("scoop/apps/chanto/1.3.0/chanto");
    for (path, version) in [(&old, "1.2.0"), (&new, "1.3.0")] {
        script(path, &format!("#!/bin/sh\necho chanto {version}\n"));
    }
    let scoop = dir.join("bin/scoop");
    script(
        &scoop,
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{calls}'\ncase \"$1\" in\nprefix) echo '{prefix}' ;;\nesac\n",
            calls = dir.join("calls").display(),
            prefix = new.parent().unwrap().display(),
        ),
    );
    Manager::Scoop
        .upgrade(scoop.as_os_str(), &old, "v1.3.0")
        .unwrap();
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
    assert!(
        calls.starts_with("update chanto\nprefix chanto\n"),
        "{calls}"
    );
}

#[test]
fn a_scoop_binary_is_found_under_the_version_scoop_moved_to() {
    let exe = Path::new("C:/Users/me/scoop/apps/chanto/1.2.0/chanto.exe");
    assert_eq!(
        scoop_current(exe, "C:/Users/me/scoop/apps/chanto/1.3.0"),
        Path::new("C:/Users/me/scoop/apps/chanto/1.3.0/chanto.exe")
    );
    // Anything it can't place stays where it was.
    assert_eq!(
        scoop_current(Path::new("/x/chanto"), ""),
        Path::new("/x/chanto")
    );
    assert_eq!(
        scoop_current(Path::new("/x/chanto"), "/new"),
        Path::new("/x/chanto")
    );
}

#[cfg(unix)]
fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn tempfile_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chanto-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn npm_and_scoop_installs_are_upgraded_by_chanto_itself() {
    let npm = Path::new("/usr/lib/node_modules/@chanto/cli/bin/chanto");
    assert_eq!(
        InstallMethod::classify(npm, false, false),
        InstallMethod::Npm
    );
    let scoop = Path::new("C:/Users/me/scoop/apps/chanto/current/chanto.exe");
    assert_eq!(
        InstallMethod::classify(scoop, false, false),
        InstallMethod::Scoop
    );
    let o = Options::default();
    assert_eq!(
        plan(&target(InstallMethod::Npm, npm), "v1.3.0", &o),
        Plan::Package(Manager::Npm)
    );
    assert_eq!(
        plan(&target(InstallMethod::Scoop, scoop), "v1.3.0", &o),
        Plan::Package(Manager::Scoop)
    );
    // `--check` names the command that moves the install on, which is now
    // Chanto's own rather than the manager's.
    let check = Options {
        check: true,
        ..Default::default()
    };
    for (method, exe) in [
        (InstallMethod::Npm, npm),
        (InstallMethod::Scoop, scoop),
        (InstallMethod::Homebrew, npm),
    ] {
        let Plan::Report(s) = plan(&target(method, exe), "v1.3.0", &check) else {
            panic!("{method:?} owns this install")
        };
        assert!(s.contains("Run `chanto upgrade`."), "{s}");
    }
    // The manager's own command is still what runs behind it.
    assert_eq!(Manager::Npm.command(), "npm install -g @chanto/cli@latest");
    assert_eq!(Manager::Scoop.command(), "scoop update chanto");
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
        Plan::Package(Manager::Brew)
    );
    let Plan::Report(s) = plan(&target(InstallMethod::Homebrew, exe), "v1.3.0", &check) else {
        panic!()
    };
    assert!(s.contains("Run `chanto upgrade`."), "{s}");
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

/// A fresh directory holding `dest` ("old") and `new` ("new").
fn swap_dir() -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("chanto-swap-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join(binary_name());
    std::fs::write(&dest, b"old").unwrap();
    std::fs::write(dir.join("new"), b"new").unwrap();
    (dir.clone(), dir.join("new"), dest)
}

fn staged_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().contains(".upgrade-"))
        .collect()
}

/// A rename that fails for the moves `fail` picks.
fn failing_rename(
    fail: impl Fn(&Path, &Path) -> bool,
) -> impl Fn(&Path, &Path) -> std::io::Result<()> {
    move |from, to| {
        if fail(from, to) {
            Err(std::io::Error::other("held open"))
        } else {
            std::fs::rename(from, to)
        }
    }
}

#[test]
fn a_failed_rename_over_the_binary_leaves_the_old_one() {
    let (dir, new, dest) = swap_dir();
    let rename = failing_rename(|_, _| true);
    assert!(replace_binary_with(&new, &dest, false, &rename).is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    assert!(staged_files(&dir).is_empty());
    // And the real rename replaces it in one step.
    replace_binary_with(&new, &dest, false, &failing_rename(|_, _| false)).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    assert!(staged_files(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn moving_aside_puts_the_old_binary_back_when_the_new_one_cannot_go_in() {
    let (dir, new, dest) = swap_dir();
    let old = dest.with_extension("old");
    // The new binary can't take the name (antivirus holding it, say).
    let rename = failing_rename(|from, _| from.to_string_lossy().contains(".upgrade-"));
    let err = replace_binary_with(&new, &dest, true, &rename).unwrap_err();
    assert!(format!("{err:#}").contains("installing"), "{err:#}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    assert!(!old.exists());
    assert!(staged_files(&dir).is_empty());

    // Nor can the old one go back: both copies are kept, and the error says where.
    let rename = failing_rename(|from, _| {
        let f = from.to_string_lossy();
        f.contains(".upgrade-") || f.ends_with(".old")
    });
    let err = format!(
        "{:#}",
        replace_binary_with(&new, &dest, true, &rename).unwrap_err()
    );
    assert!(!dest.exists());
    assert_eq!(std::fs::read(&old).unwrap(), b"old");
    let staged = staged_files(&dir);
    assert_eq!(staged.len(), 1, "{staged:?}");
    assert_eq!(std::fs::read(&staged[0]).unwrap(), b"new");
    assert!(
        err.contains(&old.display().to_string()) && err.contains(&staged[0].display().to_string()),
        "{err}"
    );

    // Moving aside works when nothing fails, leaving the old binary as `.old`.
    std::fs::rename(&old, &dest).unwrap();
    std::fs::remove_file(&staged[0]).unwrap();
    replace_binary_with(&new, &dest, true, &failing_rename(|_, _| false)).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    assert_eq!(std::fs::read(&old).unwrap(), b"old");
    assert!(staged_files(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A release server: the GitHub API's latest-release answer, SHA256SUMS
/// and the archive, all from a local port.
#[cfg(unix)]
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
    // Signed with a throwaway key, as the release workflow signs releases.
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let kp = Ed25519KeyPair::generate().unwrap();
    let sign = |sums: &str| b64.encode(kp.sign(sums.as_bytes()).as_ref()).into_bytes();
    let latest = br#"{"tag_name":"v9.9.9"}"#.to_vec();
    let url = serve(vec![
        ("/latest".into(), latest.clone()),
        (
            format!("/good/download/{tag}/SHA256SUMS"),
            good.clone().into_bytes(),
        ),
        (format!("/good/download/{tag}/SHA256SUMS.sig"), sign(&good)),
        (format!("/good/download/{tag}/{name}"), archive.clone()),
        (
            format!("/bad/download/{tag}/SHA256SUMS"),
            bad.clone().into_bytes(),
        ),
        (format!("/bad/download/{tag}/SHA256SUMS.sig"), sign(&bad)),
        (format!("/bad/download/{tag}/{name}"), archive.clone()),
        // Checksums swapped after signing: the signature no longer matches.
        (
            format!("/forged/download/{tag}/SHA256SUMS"),
            good.clone().into_bytes(),
        ),
        (format!("/forged/download/{tag}/SHA256SUMS.sig"), sign(&bad)),
        (format!("/forged/download/{tag}/{name}"), archive.clone()),
        // No signature at all.
        (
            format!("/unsigned/download/{tag}/SHA256SUMS"),
            good.into_bytes(),
        ),
        (format!("/unsigned/download/{tag}/{name}"), archive.clone()),
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
        release_key: b64.encode(kp.public_key().as_ref()),
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

    // A forged or missing signature is refused before any checksum is read.
    let yes = Options {
        yes: true,
        ..Default::default()
    };
    for (base, why) in [
        ("forged", "does not verify"),
        ("unsigned", "SHA256SUMS.sig could not be fetched"),
    ] {
        t.base_url = format!("{url}/{base}");
        let err = run(&t, &yes, &mut |_| true).await.unwrap_err();
        assert!(format!("{err:#}").contains(why), "{base}: {err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old binary");
    }

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
