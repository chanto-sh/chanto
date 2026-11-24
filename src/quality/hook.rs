//! `chanto prime --hook`: a git pre-push hook that runs
//! `chanto quality check --checks` over the commits being pushed. It's a
//! local convenience (`git push --no-verify` skips it); CI is where the
//! profile is enforced. A repository whose hooks live in a committed
//! directory (`core.hooksPath`) or that already has a pre-push hook gets the
//! lines to add instead: Chanto doesn't edit files other people share or own.

use super::*;

/// Marks a hook Chanto wrote.
const MARKER: &str = "# Installed by `chanto prime --hook`";

/// The hook script, calling `chanto` at `exe` (or on PATH when that's gone).
pub fn script(exe: &str) -> String {
    format!(
        r#"#!/bin/sh
{MARKER}: check the quality profile before a push.
# Skip it once with `git push --no-verify`.
chanto={exe}
[ -x "$chanto" ] || chanto=chanto
command -v "$chanto" >/dev/null 2>&1 || {{ echo "chanto not found; skipping the quality check" >&2; exit 0; }}
checks=--checks
while read -r local_ref local_sha remote_ref remote_sha; do
    # A deleted branch pushes nothing.
    case "$local_sha" in *[!0]*) ;; *) continue ;; esac
    base=
    case "$remote_sha" in
        *[!0]*) git cat-file -e "$remote_sha^{{commit}}" 2>/dev/null && base=$remote_sha ;;
    esac
    [ -n "$base" ] || base=$(git merge-base "$local_sha" refs/remotes/origin/HEAD 2>/dev/null)
    if [ -n "$base" ]; then
        "$chanto" quality check $checks --base "$base" --head "$local_sha" || exit 1
    else
        "$chanto" quality check $checks --all || exit 1
    fi
    checks=
done
exit 0
"#,
        exe = shell_words::quote(exe)
    )
}

/// Install the hook in `root`'s repository, or say what to add by hand.
pub fn install(root: &Path) -> Result<String> {
    if load(root)?.is_none() {
        anyhow::bail!("No quality profile here yet: run `chanto prime` first.");
    }
    if cfg!(windows) {
        anyhow::bail!(
            "Installing the hook isn't supported on Windows yet. Add `chanto quality check --checks` to your pre-push hook, or run it in CI."
        );
    }
    let exe = std::env::current_exe()?;
    let exe = exe.to_str().context("chanto's path isn't valid UTF-8")?;
    let by_hand = |why: &str| {
        format!(
            "{why} Add this to its pre-push hook instead:\n\n    chanto quality check --checks --base \"$remote_sha\" --head \"$local_sha\"\n\n(the hook reads `<local ref> <local sha> <remote ref> <remote sha>` lines on stdin; for a new branch, use the merge base with the default branch)."
        )
    };
    // Reading config runs nothing the repository defines.
    let hooks_path = std::process::Command::new("git")
        .args(["config", "--get", "core.hooksPath"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(p) = hooks_path {
        return Ok(by_hand(&format!(
            "This repository's hooks are in {p} (core.hooksPath), which may be shared."
        )));
    }
    let common = crate::hostgit::read(root, &["rev-parse", "--git-common-dir"])
        .context("Not a git repository, so there's nothing to hook.")?;
    let common = PathBuf::from(common);
    let dir = if common.is_absolute() {
        common
    } else {
        root.join(common)
    }
    .join("hooks");
    let hook = dir.join("pre-push");
    if let Ok(existing) = std::fs::read_to_string(&hook) {
        if !existing.contains(MARKER) {
            return Ok(by_hand(&format!(
                "{} already exists and isn't Chanto's.",
                hook.display()
            )));
        }
    } else if std::fs::symlink_metadata(&hook).is_ok() {
        return Ok(by_hand(&format!(
            "{} exists and isn't a readable file.",
            hook.display()
        )));
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&hook, script(exe))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(format!(
        "Installed {}: each push checks the profile's rules, the secrets scan and its checks first. `git push --no-verify` skips it once; CI is still where it's enforced.",
        hook.display()
    ))
}
