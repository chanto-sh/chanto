//! Installing from a marketplace copies someone else's files onto the machine:
//! registries, sources and symlinks are checked before anything is kept.

use super::*;
use crate::agent::tests::tmpdir;
use serde_json::json;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn market(name: &str, plugin_source: serde_json::Value) -> PathBuf {
    let dir = tmpdir("plugin-market");
    fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    fs::write(
        dir.join(".claude-plugin/marketplace.json"),
        json!({"name": name, "plugins": [{"name": "tool", "source": plugin_source}]}).to_string(),
    )
    .unwrap();
    fs::create_dir_all(dir.join("tool")).unwrap();
    fs::write(dir.join("tool/README.md"), "hello").unwrap();
    dir
}

#[test]
fn a_local_marketplace_installs_lists_and_removes_a_plugin() {
    let base = tmpdir("plugin-base");
    let yes = &mut |_: &str| true;
    let source = market("shop", json!("./tool"));
    let out = run(
        &base,
        &args(&["marketplace", "add", source.to_str().unwrap()]),
        yes,
    )
    .unwrap();
    assert!(out.contains("shop"), "{out}");
    let listed = run(&base, &args(&["list"]), yes).unwrap();
    assert!(listed.contains("shop"), "{listed}");
    let e = run(&base, &args(&["marketplace", "update", "other"]), yes).unwrap_err();
    assert_eq!(e.to_string(), "Unknown marketplace other");
    let out = run(&base, &args(&["marketplace", "update"]), yes).unwrap();
    assert!(out.starts_with("Updated"), "{out}");

    // Nothing in it can run commands, so there's nothing to confirm.
    let no = &mut |_: &str| false;
    run(&base, &args(&["install", "tool@shop"]), no).unwrap();
    let listed = run(&base, &args(&["list"]), yes).unwrap();
    assert!(
        listed.contains("tool@shop: installed (enabled)"),
        "{listed}"
    );

    let e = run(&base, &args(&["install", "tool@shop"]), no).unwrap_err();
    assert_eq!(e.to_string(), "tool@shop already installed");
    let e = run(&base, &args(&["install", "tool@elsewhere"]), no).unwrap_err();
    assert_eq!(e.to_string(), "Unknown marketplace elsewhere");
    let e = run(&base, &args(&["install", "tool"]), no).unwrap_err();
    assert!(e.to_string().starts_with("Name the plugin as"), "{e}");
}

#[test]
fn a_broken_registry_is_refused() {
    let base = tmpdir("plugin-bad");
    fs::write(base.join("marketplaces.json"), "[]").unwrap();
    let e = run(&base, &args(&["list"]), &mut |_| true).unwrap_err();
    assert_eq!(e.to_string(), "Invalid plugin registry: expected an object");
}

#[cfg(unix)]
#[test]
fn symlinks_in_a_plugin_are_refused() {
    let dir = tmpdir("plugin-link");
    fs::create_dir_all(dir.join("sub/.git")).unwrap();
    assert!(reject_symlinks(&dir).is_ok());
    std::os::unix::fs::symlink("/etc/passwd", dir.join("sub/creds")).unwrap();
    let e = reject_symlinks(&dir).unwrap_err();
    assert!(e.to_string().starts_with("Plugin contains symlink:"), "{e}");
}

#[test]
fn git_failures_are_reported_with_their_output() {
    let dir = tmpdir("plugin-git");
    let e = git(&["-C", dir.to_str().unwrap(), "no-such-subcommand"]).unwrap_err();
    assert!(e.to_string().starts_with("git failed"), "{e}");
    assert!(git(&["--version"]).is_ok());
}
