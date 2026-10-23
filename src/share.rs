//! Portable, redacted session transcripts. Uploading always needs confirmation.
use crate::llm::{Message, Role};
use crate::session::{Session, SessionFile};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn sanitize(value: &mut serde_json::Value, home: &str) {
    match value {
        serde_json::Value::String(s) => {
            *s = crate::secrets::redact_for_export(s).into_owned();
            if !home.is_empty() {
                *s = s.replace(home, "~");
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|v| sanitize(v, home)),
        serde_json::Value::Object(o) => {
            // Checkpoint file paths are object keys, too.
            let old = std::mem::take(o);
            for (key, mut v) in old {
                sanitize(&mut v, home);
                let key = if home.is_empty() {
                    key
                } else {
                    key.replace(home, "~")
                };
                o.insert(key, v);
            }
        }
        _ => {}
    }
}

pub fn html(
    session: &Session,
    messages: &[Message],
    no_tool_output: bool,
    home: &str,
) -> Result<String> {
    let mut messages = messages.to_vec();
    for m in &mut messages {
        m.images.clear();
        if no_tool_output && m.role == Role::Tool {
            m.content = "[tool output omitted]".into();
        }
    }
    let mut meta = session.clone();
    // Snapshots can contain binary secrets and depend on a local shadow repo.
    meta.checkpoints.clear();
    meta.rewind_undo = None;
    let mut value = serde_json::to_value(SessionFile { meta, messages })?;
    sanitize(&mut value, home);
    let portable: SessionFile = serde_json::from_value(value.clone())?;
    let mut body = String::new();
    for m in &portable.messages {
        if m.role == Role::System {
            continue;
        }
        let text = escape(&m.content);
        if m.role == Role::Tool {
            body.push_str(&format!(
                "<details><summary>Tool output / diff</summary><pre>{text}</pre></details>"
            ));
        } else {
            body.push_str(&format!(
                "<article><h2>{:?}</h2><pre>{text}</pre></article>",
                m.role
            ));
        }
        for call in &m.tool_calls {
            body.push_str(&format!(
                "<details><summary>{}</summary><pre>{}</pre></details>",
                escape(&call.name),
                escape(&serde_json::to_string_pretty(&call.arguments)?)
            ));
        }
    }
    // Escape '<' in JSON so even an adversarial </script> remains inert.
    let json = serde_json::to_string(&value)?
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let meta = portable.meta;
    Ok(format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>{title}</title>
<style>:root{{color-scheme:light dark}}body{{font:16px system-ui;max-width:960px;margin:2em auto;padding:0 1em}}pre{{white-space:pre-wrap;overflow-wrap:anywhere}}article,details{{border:1px solid #8886;border-radius:8px;padding:1em;margin:1em 0}}summary,button{{cursor:pointer}}h2{{font-size:1em}}</style>
<button onclick="document.documentElement.style.colorScheme=document.documentElement.style.colorScheme==='dark'?'light':'dark'">Dark / light</button>
<h1>{title}</h1><p>Model: {model} · Date: {date} · Tokens: {input} input / {output} output · Cost: unavailable (billing is provider-specific)</p>{body}
<script type="application/json" id="chanto-session">{json}</script></html>"#,
        title = escape(&meta.title),
        model = escape(&meta.model),
        date = escape(&meta.created),
        input = meta.stats.prompt_tokens,
        output = meta.stats.completion_tokens
    ))
}

pub fn export(session: &Session, messages: &[Message], no_tool_output: bool) -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let content = html(session, messages, no_tool_output, &home)?;
    let dir = crate::config::home_dir().join("shares");
    std::fs::create_dir_all(&dir)?;
    // IDs in imported metadata are never used as paths.
    if session.id.contains(['/', '\\']) || session.id == "." || session.id == ".." {
        bail!("invalid session id");
    }
    let path = dir.join(format!("{}.html", session.id));
    crate::util::write_private(&path, content.as_bytes())?;
    Ok(path)
}

pub fn import(path: &Path, cwd: &Path) -> Result<Session> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("can't read {}: {e}", path.display()))?;
    let json = if content.trim_start().starts_with('{') {
        content.as_str()
    } else {
        content
            .split_once("<script type=\"application/json\" id=\"chanto-session\">")
            .and_then(|(_, rest)| rest.split_once("</script>").map(|(json, _)| json))
            .ok_or_else(|| anyhow::anyhow!("{} has no chanto session data", path.display()))?
    };
    let mut data: SessionFile = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("{} isn't a chanto session: {e}", path.display()))?;
    let fresh = Session::new(cwd, &data.meta.model);
    data.meta.id = fresh.id;
    data.meta.cwd = cwd.into();
    data.meta.created = fresh.created;
    data.meta.checkpoints.clear();
    data.meta.rewind_undo = None;
    crate::secrets::redact_messages(&mut data.messages);
    data.meta.write(data.messages)?;
    Ok(data.meta)
}

/// Confirm the exact bytes, then upload those bytes, even if the local file changes.
pub fn gist(path: &Path, confirm: &mut dyn FnMut(&str) -> bool) -> Result<String> {
    let content = std::fs::read_to_string(path)?;
    let preview = format!(
        "Upload {} as a SECRET GitHub gist. Secret gists are accessible to anyone with the URL.\nExactly these bytes will be uploaded:\n{content}",
        path.display()
    );
    if !confirm(&preview) {
        bail!("upload cancelled");
    }
    let status = std::process::Command::new("gh")
        .args(["auth", "status"])
        .output()?;
    if !status.status.success() {
        bail!("gh is not logged in");
    }
    let mut child = std::process::Command::new("gh")
        .args(["gist", "create", "--filename", "session.html", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    use std::io::Write;
    child.stdin.take().unwrap().write_all(content.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "gh gist create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

pub fn cli(args: &[String]) -> Result<String> {
    let cwd = std::env::current_dir()?;
    match args[0].as_str() {
        "sessions" => crate::session_cmds::sessions_cli(&cwd, &args[1..]),
        "import-session" => {
            let path = args.get(1).ok_or_else(|| {
                anyhow::anyhow!("usage: chanto import-session <file.html|file.json>")
            })?;
            let session = import(Path::new(path), &cwd)?;
            Ok(format!(
                "Imported session {}. Continue with chanto -r {}",
                session.id, session.id
            ))
        }
        "share" => {
            if args
                .iter()
                .skip(1)
                .any(|s| s.starts_with('-') && !matches!(s.as_str(), "--gist" | "--no-tool-output"))
            {
                bail!("usage: chanto share [id] [--gist] [--no-tool-output]");
            }
            let id = args
                .iter()
                .skip(1)
                .find(|s| !s.starts_with('-'))
                .cloned()
                .or_else(|| Session::list(&cwd).first().map(|s| s.id.clone()))
                .ok_or_else(|| anyhow::anyhow!("no saved sessions"))?;
            let (session, messages) = Session::load(&cwd, &id)?;
            // Initialize known-value redaction for the offline CLI as well.
            let (mut cfg, _) = crate::config::Config::load(&cwd, false)?;
            cfg.secrets.redact = true;
            crate::secrets::init(&cfg, &cwd);
            let path = export(
                &session,
                &messages,
                args.iter().any(|s| s == "--no-tool-output"),
            )?;
            if args.iter().any(|s| s == "--gist") {
                use std::io::Write;
                let url = gist(&path, &mut |preview| {
                    println!("{preview}");
                    print!("Upload? Type yes to confirm: ");
                    let _ = std::io::stdout().flush();
                    let mut answer = String::new();
                    std::io::stdin().read_line(&mut answer).is_ok() && answer.trim() == "yes"
                })?;
                Ok(format!("{}\n{url}", path.display()))
            } else {
                Ok(path.display().to_string())
            }
        }
        _ => bail!("unknown session command"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_html_and_import_round_trip() {
        let _home = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _secrets = crate::secrets::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("chanto-share-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("CHANTO_HOME", &dir);
        }
        let mut session = Session::new(&dir, "test-model");
        session.title = "<title>".into();
        let messages = vec![
            Message::system("system"),
            Message::user(
                "/Users/test/project ghp_0123456789abcdefghijklmnopqrstuvwxyzAB </script>",
            ),
            Message::assistant("reply"),
        ];
        let content = html(&session, &messages, false, "/Users/test").unwrap();
        assert!(!content.contains("/Users/test"));
        assert!(!content.contains("ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"));
        assert!(!content.contains("src=\"") && !content.contains("href=\""));
        assert!(content.contains("&lt;/script&gt;") && content.contains("\\u003c/script\\u003e"));
        assert!(content.contains("color-scheme:light dark"));
        let path = dir.join("share.html");
        std::fs::write(&path, content).unwrap();
        let imported = import(&path, &dir).unwrap();
        assert_ne!(imported.id, session.id);
        let (_, back) = Session::load(&dir, &imported.id).unwrap();
        assert_eq!(back.len(), messages.len());
        assert!(back[1].content.contains("~/project"));
        assert_eq!(back[2].content, "reply");
        let json = dir.join("session.json");
        std::fs::copy(imported.path(), &json).unwrap();
        assert_ne!(import(&json, &dir).unwrap().id, imported.id);
        let mut tool = Message::assistant("output-secret");
        tool.role = Role::Tool;
        assert!(
            !html(&session, &[tool], true, "")
                .unwrap()
                .contains("output-secret")
        );
        unsafe {
            std::env::remove_var("CHANTO_HOME");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn gist_requires_exact_confirmation_and_uses_secret_default() {
        // PATH is process-wide; exercise the fake CLI in an isolated test process.
        if std::env::var_os("CHANTO_TEST_GIST_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "share::tests::gist_requires_exact_confirmation_and_uses_secret_default",
                    "--nocapture",
                ])
                .env("CHANTO_TEST_GIST_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let _home = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("chanto-gh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("CHANTO_HOME", &dir);
        }
        let path = dir.join("share.html");
        std::fs::write(&path, "exact upload bytes").unwrap();
        let gh = dir.join("gh");
        std::fs::write(&gh, "#!/bin/sh\nif [ \"$1\" = auth ]; then exit 0; fi\n[ \"$1 $2 $3 $4 $5\" = 'gist create --filename session.html -' ] || exit 2\nIFS= read -r text\n[ \"$text\" = 'exact upload bytes' ] || exit 3\nprintf 'https://gist.example/secret\\n'\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let old_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &dir);
        }
        assert!(
            gist(&path, &mut |preview| {
                assert!(preview.contains("exact upload bytes"));
                false
            })
            .is_err()
        );
        assert_eq!(
            gist(&path, &mut |preview| {
                assert!(preview.contains("SECRET"));
                // A change after approval cannot change the bytes sent to gh.
                std::fs::write(&path, "different bytes").unwrap();
                true
            })
            .unwrap(),
            "https://gist.example/secret"
        );
        unsafe {
            if let Some(old) = old_path {
                std::env::set_var("PATH", old);
            } else {
                std::env::remove_var("PATH");
            }
            std::env::remove_var("CHANTO_HOME");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
