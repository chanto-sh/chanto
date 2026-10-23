//! User-managed Claude-compatible marketplaces and plugin installs.
use crate::util::Canonical as _;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn root() -> PathBuf {
    crate::config::home_dir().join("plugins")
}
fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(
        &fs::read(path).with_context(|| path.display().to_string())?,
    )?)
}
fn registry(base: &Path, file: &str) -> Result<Value> {
    let path = base.join(file);
    if path.exists() {
        read(&path)
    } else {
        Ok(json!({}))
    }
}
fn save(base: &Path, file: &str, value: &Value) -> Result<()> {
    fs::create_dir_all(base)?;
    let tmp = base.join(format!("{file}.tmp"));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(tmp, base.join(file))?;
    Ok(())
}
fn name(s: &str) -> Result<()> {
    if s.is_empty()
        || s == "."
        || s == ".."
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        bail!("Invalid name: {s}");
    }
    Ok(())
}
fn git(args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!(
            "git failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
fn url(source: &str) -> Result<String> {
    if source.starts_with('-') {
        bail!("Invalid git source");
    }
    if source.contains("://") || source.starts_with("git@") {
        return Ok(source.into());
    }
    let parts: Vec<_> = source.split('/').collect();
    if parts.len() != 2 {
        bail!("Expected GitHub owner/repo, git URL, or local directory");
    }
    for p in parts {
        name(p)?;
    }
    Ok(format!("https://github.com/{source}.git"))
}
fn copy(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        if e.file_name() == ".git" {
            continue;
        }
        let ty = e.file_type()?;
        if ty.is_symlink() {
            bail!("Plugin contains symlink: {}", e.path().display());
        }
        if ty.is_dir() {
            copy(&e.path(), &dst.join(e.file_name()))?;
        } else if ty.is_file() {
            fs::copy(e.path(), dst.join(e.file_name()))?;
        }
    }
    Ok(())
}
fn marketplace(path: &Path) -> Result<Value> {
    let m = read(&path.join(".claude-plugin/marketplace.json"))?;
    name(m["name"].as_str().context("Marketplace needs a name")?)?;
    let entries = m["plugins"]
        .as_array()
        .context("Marketplace needs plugins[]")?;
    for p in entries {
        name(p["name"].as_str().context("Plugin needs a name")?)?;
    }
    Ok(m)
}

pub fn loaded(base: &Path) -> Result<Vec<crate::compat::Plugin>> {
    let r = registry(base, "installed.json")?;
    Ok(r.as_object()
        .context("Invalid installed registry")?
        .iter()
        .filter(|(_, p)| p["enabled"] != false)
        .filter_map(|(id, p)| {
            let root = base.join("installed").join(id);
            root.is_dir().then(|| crate::compat::Plugin {
                name: id.split('@').next().unwrap_or(id).into(),
                root,
                manifest: p["manifest"].clone(),
            })
        })
        .collect())
}

/// The confirmation callback sees the actual staged contents before any install.
pub fn run(base: &Path, args: &[String], confirm: &mut dyn FnMut(&str) -> bool) -> Result<String> {
    let yes = args.iter().any(|a| a == "--yes");
    let a: Vec<&str> = args
        .iter()
        .filter(|a| a.as_str() != "--yes")
        .map(String::as_str)
        .collect();
    let mut markets = registry(base, "marketplaces.json")?;
    let mut installed = registry(base, "installed.json")?;
    if !markets.is_object() || !installed.is_object() {
        bail!("Invalid plugin registry: expected an object");
    }
    match a.as_slice() {
        ["marketplace", "add", source] => {
            fs::create_dir_all(base.join("marketplaces"))?;
            let staging = base
                .join("marketplaces")
                .join(uuid::Uuid::new_v4().to_string());
            let local = Path::new(source).is_dir();
            let result = (|| -> Result<String> {
                if local {
                    copy(&Path::new(source).canonical()?, &staging)?;
                } else {
                    git(&[
                        "clone",
                        "--",
                        &url(source)?,
                        staging.to_str().context("Non-UTF8 path")?,
                    ])?;
                }
                let m = marketplace(&staging)?;
                let n = m["name"].as_str().unwrap();
                if markets.get(n).is_some() {
                    bail!("Marketplace {n} already added");
                }
                let dest = base.join("marketplaces").join(n);
                fs::rename(&staging, &dest)?;
                markets[n] = json!({"source": source, "git": !local});
                save(base, "marketplaces.json", &markets)?;
                Ok(format!("Added marketplace {n}"))
            })();
            let _ = fs::remove_dir_all(staging);
            result
        }
        ["marketplace", "list"] => Ok(markets
            .as_object()
            .context("Invalid marketplace registry")?
            .iter()
            .map(|(n, m)| format!("{n}: {}", m["source"].as_str().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n"))
        .map(|s| {
            if s.is_empty() {
                "No marketplaces yet. Add one with `chanto plugin marketplace add <source>`.".into()
            } else {
                s
            }
        }),
        ["marketplace", "remove", n] => {
            name(n)?;
            if markets.get(n).is_none() {
                bail!("Unknown marketplace {n}");
            }
            fs::remove_dir_all(base.join("marketplaces").join(n))?;
            markets.as_object_mut().unwrap().remove(*n);
            save(base, "marketplaces.json", &markets)?;
            Ok(format!("Removed marketplace {n}"))
        }
        ["marketplace", "update"] | ["marketplace", "update", _] => {
            if a.len() == 3 && markets.get(a[2]).is_none() {
                bail!("Unknown marketplace {}", a[2]);
            }
            for (n, m) in markets
                .as_object()
                .context("Invalid marketplace registry")?
            {
                if a.len() == 3 && n != a[2] {
                    continue;
                }
                if m["git"] == true {
                    git(&[
                        "-C",
                        base.join("marketplaces")
                            .join(n)
                            .to_str()
                            .context("Non-UTF8 path")?,
                        "pull",
                        "--ff-only",
                    ])?;
                }
            }
            Ok("Updated git marketplaces (local snapshots are unchanged)".into())
        }
        [] | ["list"] => {
            let mut lines = Vec::new();
            for (id, p) in installed
                .as_object()
                .context("Invalid installed registry")?
            {
                lines.push(format!(
                    "{id}: installed ({})",
                    if p["enabled"] == false {
                        "disabled"
                    } else {
                        "enabled"
                    }
                ));
            }
            let claude_registry =
                crate::compat::claude_dir().join("plugins/installed_plugins.json");
            if claude_registry.exists() {
                let registry = read(&claude_registry)?;
                let enabled: Vec<_> = crate::compat::plugins(&std::env::current_dir()?, false)
                    .into_iter()
                    .map(|p| p.root)
                    .collect();
                for (id, entries) in registry["plugins"].as_object().into_iter().flatten() {
                    let active = entries.as_array().into_iter().flatten().any(|e| {
                        e["installPath"]
                            .as_str()
                            .is_some_and(|p| enabled.contains(&PathBuf::from(p)))
                    });
                    lines.push(format!(
                        "{id}: installed (Claude Code, {})",
                        if active {
                            "enabled"
                        } else {
                            "disabled or outside this scope"
                        }
                    ));
                }
            }
            for n in markets
                .as_object()
                .context("Invalid marketplace registry")?
                .keys()
            {
                let m = marketplace(&base.join("marketplaces").join(n))?;
                for p in m["plugins"].as_array().unwrap() {
                    lines.push(format!(
                        "{}@{n}: available — {} {}",
                        p["name"].as_str().unwrap(),
                        p["version"].as_str().unwrap_or(""),
                        p["description"].as_str().unwrap_or("")
                    ));
                }
            }
            if lines.is_empty() {
                return Ok("No plugins installed or available. Add a marketplace with `chanto plugin marketplace add <source>`, then `chanto plugin install <plugin>@<marketplace>`.".into());
            }
            Ok(lines.join("\n"))
        }
        ["install", id] => {
            let (p, m) = id.split_once('@').context("Name the plugin as <plugin>@<marketplace>; `chanto plugin list` shows what's available")?;
            name(p)?;
            name(m)?;
            if installed.get(*id).is_some() {
                bail!("{id} already installed");
            }
            if markets.get(m).is_none() {
                bail!("Unknown marketplace {m}");
            }
            let location = base.join("marketplaces").join(m);
            let market = marketplace(&location)?;
            let entry = market["plugins"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == p)
                .context("Unknown plugin")?;
            fs::create_dir_all(base)?;
            let stage = base.join(uuid::Uuid::new_v4().to_string());
            let result = (|| -> Result<String> {
                if let Some(src) = entry["source"].as_str() {
                    let src = location.join(src).canonical()?;
                    if !src.starts_with(location.canonical()?) {
                        bail!("Plugin source escapes marketplace");
                    }
                    copy(&src, &stage)?;
                } else {
                    let src = &entry["source"];
                    let remote = match src["source"].as_str() {
                        Some("github") => url(src["repo"].as_str().context("Missing repo")?)?,
                        Some("git" | "url") => url(src["url"].as_str().context("Missing URL")?)?,
                        _ => bail!("Unsupported plugin source"),
                    };
                    git(&[
                        "clone",
                        "--",
                        &remote,
                        stage.to_str().context("Non-UTF8 path")?,
                    ])?;
                    if let Some(rev) = src["ref"].as_str() {
                        if rev.starts_with('-') {
                            bail!("Invalid git ref");
                        }
                        git(&["-C", stage.to_str().unwrap(), "checkout", "--detach", rev])?;
                    }
                }
                let mut manifest = entry.clone();
                let path = stage.join(".claude-plugin/plugin.json");
                if path.exists() {
                    manifest.as_object_mut().unwrap().extend(
                        read(&path)?
                            .as_object()
                            .context("Invalid plugin manifest")?
                            .clone(),
                    );
                }
                manifest["_marketplace"] = json!(m);
                let plugin = crate::compat::Plugin {
                    name: p.into(),
                    root: stage.clone(),
                    manifest: manifest.clone(),
                };
                let (summary, risky) = plugin.contents();
                if risky && !yes && !confirm(&summary) {
                    bail!(
                        "Install cancelled; hooks/MCP/LSP require confirmation or --yes\n{summary}"
                    );
                }
                fs::create_dir_all(base.join("installed"))?;
                fs::rename(&stage, base.join("installed").join(id))?;
                installed[*id] = json!({"enabled": true, "manifest": manifest});
                save(base, "installed.json", &installed)?;
                Ok(format!("Installed {id}\n{summary}"))
            })();
            let _ = fs::remove_dir_all(stage);
            result
        }
        [action @ ("uninstall" | "enable" | "disable"), query] => {
            let ids: Vec<_> = installed
                .as_object()
                .context("Invalid installed registry")?
                .keys()
                .filter(|id| id.as_str() == *query || id.split('@').next() == Some(query))
                .cloned()
                .collect();
            if ids.len() != 1 {
                bail!("Expected one installed plugin matching {query}; use plugin@marketplace");
            }
            let id = &ids[0];
            if *action == "uninstall" {
                fs::remove_dir_all(base.join("installed").join(id))?;
                installed.as_object_mut().unwrap().remove(id);
            } else {
                installed[id]["enabled"] = json!(*action == "enable");
            }
            save(base, "installed.json", &installed)?;
            Ok(format!("{action}: {id}"))
        }
        _ => bail!(
            "Usage: plugin list|install <plugin>@<marketplace> [--yes]|uninstall|enable|disable <plugin>; plugin marketplace add <source>|list|remove <name>|update [name]"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_marketplace_lifecycle() {
        let _lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("chanto-market-{}", uuid::Uuid::new_v4()));
        let previous = std::env::var_os("CHANTO_COMPAT_HOME");
        let previous_chanto = std::env::var_os("CHANTO_HOME");
        unsafe {
            std::env::set_var("CHANTO_COMPAT_HOME", &home);
            std::env::set_var("CHANTO_HOME", home.join(".chanto"));
        }
        let source = home.join("source");
        let write = |path: &str, text: &str| {
            let p = source.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        };
        write(
            ".claude-plugin/marketplace.json",
            r#"{"name":"local","owner":{"name":"test"},"plugins":[{"name":"hello","source":"./hello","description":"Greeting","version":"1"},{"name":"risky","source":"./risky","mcpServers":{"test":{"command":"false"}}},{"name":"escape","source":"../../"}]}"#,
        );
        write("hello/.claude-plugin/plugin.json", r#"{"name":"hello"}"#);
        write(
            "hello/commands/greet.md",
            "---\ndescription: Greet\n---\nHello $ARGUMENTS",
        );
        write(
            "hello/skills/greeting/SKILL.md",
            "---\nname: greeting\ndescription: Greeting skill\n---\nSay hello.",
        );
        write("risky/.claude-plugin/plugin.json", r#"{"name":"risky"}"#);
        let base = root();
        let run = |args: &[&str]| {
            run(
                &base,
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &mut |_| false,
            )
        };
        run(&["marketplace", "add", source.to_str().unwrap()]).unwrap();
        assert!(run(&["marketplace", "list"]).unwrap().contains("local"));
        assert!(run(&["list"]).unwrap().contains("hello@local"));
        assert!(run(&["install", "escape@local"]).is_err());
        assert!(
            run(&["install", "risky@local"])
                .unwrap_err()
                .to_string()
                .contains("require confirmation")
        );
        assert!(!base.join("installed/risky@local").exists());
        run(&["install", "risky@local", "--yes"]).unwrap();
        run(&["uninstall", "risky"]).unwrap();
        let summary = run(&["install", "hello@local"]).unwrap();
        assert!(summary.contains("greet.md"));
        let cwd = home.join("project");
        fs::create_dir_all(cwd.join(".claude")).unwrap();
        fs::write(
            cwd.join(".claude/settings.json"),
            r#"{"enabledPlugins":{"hello@local":false}}"#,
        )
        .unwrap();
        let ps = crate::compat::plugins(&cwd, false);
        assert_eq!(ps.len(), 1);
        assert!(crate::compat::plugins(&cwd, true).is_empty());
        assert_eq!(
            crate::compat::plugins_with_compat(&cwd, false, false).len(),
            1
        );
        crate::compat::set_active_plugins(ps);
        assert!(
            crate::context::load_commands(&cwd)
                .iter()
                .any(|c| c.name == "hello:greet")
        );
        assert!(
            crate::context::load_skills(&cwd)
                .iter()
                .any(|s| s.name.contains("greeting"))
        );
        run(&["disable", "hello"]).unwrap();
        assert!(crate::compat::plugins(&cwd, false).is_empty());
        run(&["enable", "hello"]).unwrap();
        run(&["marketplace", "remove", "local"]).unwrap();
        assert_eq!(crate::compat::plugins(&cwd, false).len(), 1);
        run(&["uninstall", "hello"]).unwrap();
        assert!(loaded(&base).unwrap().is_empty());
        crate::compat::set_active_plugins(vec![]);
        unsafe {
            match previous_chanto {
                Some(v) => std::env::set_var("CHANTO_HOME", v),
                None => std::env::remove_var("CHANTO_HOME"),
            }
            match previous {
                Some(v) => std::env::set_var("CHANTO_COMPAT_HOME", v),
                None => std::env::remove_var("CHANTO_COMPAT_HOME"),
            }
        }
        fs::remove_dir_all(home).unwrap();
    }
}
