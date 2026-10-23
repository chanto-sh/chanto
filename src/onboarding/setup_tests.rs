//! Saving the chosen default, and the model list every frontend shares, with a
//! throwaway home and local mock servers.

use super::*;
use crate::agent::tests::{make_env_with, remove_dir_eventually, tmpdir};
use crate::permissions::Mode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `CHANTO_HOME` (and the other agents' homes) in a temp dir for the test.
struct Home {
    dir: std::path::PathBuf,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Home {
    fn new(name: &str) -> Home {
        let lock = crate::compat::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir(name);
        let mut saved = Vec::new();
        for (k, v) in [
            ("CHANTO_HOME", dir.clone()),
            ("CHANTO_COMPAT_HOME", dir.clone()),
            ("CODEX_HOME", dir.join(".codex")),
        ] {
            saved.push((k, std::env::var_os(k)));
            // SAFETY: test-only, under TEST_HOME_LOCK.
            unsafe { std::env::set_var(k, v) };
        }
        Home {
            dir,
            saved,
            _lock: lock,
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            // SAFETY: test-only, under TEST_HOME_LOCK.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        remove_dir_eventually(&self.dir);
    }
}

/// Answers every request with an OpenAI-style model list.
async fn models_server(models: &[&str]) -> String {
    let body = serde_json::json!({
        "data": models.iter().map(|m| serde_json::json!({"id": m})).collect::<Vec<_>>()
    })
    .to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

/// Built-in providers are listed when they have credentials, which would
/// mean real network calls: these tests only run without any. (Cloud
/// platforms are never listed, so their ambient credentials don't count.)
fn no_ambient_credentials() -> bool {
    let none = crate::providers::CATALOG
        .iter()
        .filter(|h| !h.kind.is_cloud())
        .all(|h| detect::credential_source(h.id).is_none());
    if !none {
        eprintln!("skipped: provider credentials in the environment");
    }
    none
}

#[test]
fn bad_defaults_are_refused_before_anything_is_written() {
    let dir = tmpdir("setup-save");
    let path = dir.join("nested/config.toml");
    let err = |r: Result<()>| format!("{:#}", r.unwrap_err());
    assert!(err(save_to(&path, "  ", None, None)).starts_with("Model cannot be empty"));
    assert!(err(save_to(&path, "m", Some("pigeon"), None)).starts_with("Unknown provider"));
    assert!(err(save_to(&path, "m", None, Some("not a url"))).starts_with("Invalid base URL"));
    assert_eq!(
        err(save_to(&path, "m", None, Some("ftp://host/v1"))),
        "Base URL must use http or https"
    );
    assert!(!path.exists());
    // The parent directory is created on first save.
    save_to(&path, "m", Some("openai"), Some("http://localhost:1/v1")).unwrap();
    let t: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(t["base_url"].as_str(), Some("http://localhost:1/v1"));
    // A config that doesn't parse is left for the user to fix.
    std::fs::write(&path, "model = = broken").unwrap();
    assert!(err(save_to(&path, "m", None, None)).starts_with("Cannot edit invalid chanto config"));
    remove_dir_eventually(&dir);
}

#[test]
fn candidates_are_saved_with_their_server() {
    let home = Home::new("setup-candidate");
    let read = || -> toml::Table {
        toml::from_str(&std::fs::read_to_string(home.dir.join("config.toml")).unwrap()).unwrap()
    };
    let c = |provider: &str, source: &str, model: Option<&str>| detect::Candidate {
        source: source.into(),
        provider: provider.into(),
        model: model.map(String::from),
        note: "found".into(),
        recommended: false,
    };
    save_candidate(&c("ollama", "http://localhost:11434", Some("qwen3"))).unwrap();
    let t = read();
    assert_eq!(t["model"].as_str(), Some("qwen3"));
    assert_eq!(t["provider"].as_str(), Some("ollama"));
    assert_eq!(t["base_url"].as_str(), Some("http://localhost:11434"));
    save_candidate(&c("lmstudio", "http://localhost:1234", Some("local"))).unwrap();
    let t = read();
    assert_eq!(t["provider"].as_str(), Some("openai"));
    assert_eq!(t["base_url"].as_str(), Some("http://localhost:1234/v1"));
    // A hosted provider's model carries its prefix; no server is pinned.
    save_candidate(&c("groq", "env", Some("groq/llama"))).unwrap();
    let t = read();
    assert_eq!(t["model"].as_str(), Some("groq/llama"));
    assert!(!t.contains_key("base_url"));
    assert_eq!(
        format!("{:#}", save_candidate(&c("groq", "env", None)).unwrap_err()),
        "candidate has no model"
    );
}

#[test]
fn a_picker_needs_choices_and_cancelling_says_how_to_continue() {
    assert_eq!(
        choose("Pick", &[], true).unwrap_err().to_string(),
        "No choices available"
    );
    assert!(Cancelled.to_string().starts_with("Setup cancelled."));
}

#[tokio::test]
async fn models_are_listed_per_provider_and_failures_named() {
    let _home = Home::new("setup-models");
    if !no_ambient_credentials() {
        return;
    }
    let default = models_server(&["local-a", "local-b"]).await;
    let corp = models_server(&["big"]).await;
    let empty = models_server(&[]).await;
    let mut cfg = Config::default();
    cfg.provider = crate::llm::ProviderKind::Openai;
    cfg.base_url = Some(default.clone());
    for (name, url) in [
        ("corp", corp.as_str()),
        ("empty", empty.as_str()),
        ("down", "http://127.0.0.1:1/v1"),
    ] {
        cfg.providers.insert(
            name.into(),
            crate::config::ProviderConfig {
                kind: crate::llm::ProviderKind::Openai,
                base_url: url.into(),
                ..Default::default()
            },
        );
    }
    let provider = crate::llm::Provider::new(cfg.provider, &default, None, 32768);
    let (models, failures) = available_models(&cfg, provider).await;
    for m in ["local-a", "local-b", "corp/big"] {
        assert!(models.iter().any(|x| x == m), "{m} missing from {models:?}");
    }
    assert!(
        failures.iter().any(|f| f == "empty returned no models"),
        "{failures:?}"
    );
    assert!(
        failures.iter().any(|f| f == "down: not running"),
        "{failures:?}"
    );
}

#[tokio::test]
async fn refreshed_models_are_cached_for_the_next_session() {
    let _home = Home::new("setup-cache");
    if !no_ambient_credentials() {
        return;
    }
    let base = models_server(&["cached-model"]).await;
    let dir = tmpdir("setup-cache-proj");
    let env = make_env_with(&base, &dir, Mode::Default, 32768, |c| {
        c.base_url = Some(base.clone());
    });
    assert!(cached_models(&env).is_none());
    let models = refresh_models(&env, false).await;
    assert!(models.iter().any(|m| m == "cached-model"), "{models:?}");
    let (cached, age) = cached_models(&env).expect("the list is cached");
    assert_eq!(cached, models);
    assert!(age < std::time::Duration::from_secs(60));
    // Another provider set has its own entry.
    let other = make_env_with("http://127.0.0.1:1/v1", &dir, Mode::Default, 32768, |_| {});
    assert!(cached_models(&other).is_none());
    remove_dir_eventually(&dir);
}

#[tokio::test]
async fn suggestions_include_the_existing_default() {
    let home = Home::new("setup-suggest");
    if !no_ambient_credentials() {
        return;
    }
    std::fs::write(
        home.dir.join("config.toml"),
        "model = \"my-default\"\nprovider = \"openai\"\n",
    )
    .unwrap();
    let s = suggestions(None).await;
    assert!(s.iter().any(|m| m == "my-default"), "{s:?}");
    let mut sorted = s.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(s, sorted, "sorted and without duplicates");
}
