use super::*;

thread_local! {
    /// Every pattern runs, as before the literal check: the reference.
    pub(super) static UNFILTERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn unfiltered<T>(f: impl FnOnce() -> T) -> T {
    UNFILTERED.with(|u| u.set(true));
    let r = f();
    UNFILTERED.with(|u| u.set(false));
    r
}

#[test]
fn the_literal_check_changes_no_redaction() {
    let pieces: &[&str] = &[
        "ghp_0123456789abcdefghijABCDEFGHIJ012345",
        "github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ",
        "AKIAIOSFODNN7EXAMPLE",
        "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "AWS_SECRET_ACCESS_KEY=\"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\"",
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----",
        "glpat-abcdefghij0123456789",
        "xoxb-1234567890-abcdefghij",
        "https://hooks.slack.com/services/T000/B000/XXXX",
        "sk_live_0123456789abcdefABCD",
        "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
        "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789AB",
        // Split so secret scanners don't take the fake for a real key.
        concat!("AIza", "SyA-0123456789abcdefghijklmnopqrstu "),
        "npm_abcdefghijklmnopqrstuvwxyz0123456789",
        "pypi-AgEIcHlwaS5vcmcCJGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEyMzQ1Njc4OQ",
        concat!("hf_", "abcdefghijklmnopqrstuvwxyzABCDEFGH"),
        concat!(
            "SG.",
            "abcdefghijklmnopqrstuv.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ "
        ),
        concat!(
            "dop_v1_",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ),
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
        "postgres://user:hunter2pass@db.example.com/app",
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345",
        "API_KEY=Zx8kQ2vN7pL4mR9tW3yB6cF1",
        "\"client_secret\": \"Zx8kQ2vN7pL4mR9tW3yB6cF1hJ\"",
        "my_Token: Zx8kQ2vN7pL4mR9tW3yB6cF1",
        // Near misses, mixed case and the two non-ASCII case folds.
        "GHP_0123456789abcdefghijABCDEFGHIJ012345",
        "BEARER abcdefghijklmnopqrstuvwxyz012345",
        "AWS_\u{17f}ECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "API_\u{212a}EY=Zx8kQ2vN7pL4mR9tW3yB6cF1",
        "Bearer\u{17f}",
        "toKen = short",
        "sk-short",
        "eyJ only",
        "https://example.com/path",
        "fn main() { println!(\"hello\"); }",
        "plain words, é and ü, nothing secret",
        "\n",
        " ",
    ];
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    let mut redacted = 0;
    for _ in 0..4000 {
        let text: String = (0..1 + next(6))
            .map(|_| pieces[next(pieces.len())])
            .collect();
        let filtered = redact_always_with_labels(&text);
        let full = unfiltered(|| redact_always_with_labels(&text));
        assert_eq!(filtered, full, "{text:?}");
        assert_eq!(
            contains_secret(&text),
            unfiltered(|| contains_secret(&text)),
            "{text:?}"
        );
        redacted += usize::from(filtered.0.contains("[REDACTED"));
    }
    assert!(redacted > 1000, "the corpus barely has secrets: {redacted}");
}

#[test]
fn plain_key_values_need_entropy_and_public_keys_are_not_secret() {
    // Withheld from commands either way, but only a random-looking value
    // is masked: `APP_KEY=my-app` is a name.
    for name in ["APP_KEY", "ENCRYPTION_KEY"] {
        assert!(is_secret_name(name));
        assert!(!secret_value(name, "ordinary-config-value"));
        assert!(secret_value(name, "aB2cD3eF4gH5iJ6kL7mN8"));
    }
    // Names of keys, not keys.
    for name in [
        "MONKEY",
        "SSH_PUBLIC_KEY",
        "service_pubkey",
        "CACHE_KEY",
        "SORT_KEY",
        "PRIMARY_KEY",
        "TURBO_KEY",
    ] {
        assert!(!is_secret_name(name));
    }
    assert!(secret_value("API_KEY", "opaque-credential"));
}

#[test]
fn json_images_exclusion_does_not_hide_nested_arguments() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_values(&[("TEST_TOKEN", "nested-secret-value")]);
    let mut value = serde_json::json!({
        "images": ["nested-secret-value"],
        "arguments": {"images": "nested-secret-value"}
    });
    redact_json(&mut value);
    assert_eq!(value["images"][0], "nested-secret-value");
    assert_eq!(value["arguments"]["images"], "[REDACTED:TEST_TOKEN]");
}

#[test]
fn registration_deduplicates_batch_and_preserves_existing_base() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_values(&[("BASE_TOKEN", "registration-base-value")]);
    register_all(vec![
        ("BASE_TOKEN".into(), "registration-base-value".into()),
        ("NEW_TOKEN".into(), "registration-new-value".into()),
        ("OTHER_TOKEN".into(), "registration-new-value".into()),
    ]);
    let st = state().read().unwrap();
    for value in ["registration-base-value", "registration-new-value"] {
        assert_eq!(st.base.iter().filter(|(_, v)| v == value).count(), 1);
    }
}

/// Warming compiles the pattern set ahead of the first prompt; what that
/// prompt masks must be the same either way.
#[test]
fn warming_the_patterns_still_masks_secrets() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_values(&[]);
    warm(&std::env::temp_dir().join("chanto-warm-no-such-project"));
    let (out, labels) = redact_with_labels("export KEY_ID=AKIAIOSFODNN7EXAMPLE");
    assert!(out.contains("[REDACTED:AWS_ACCESS_KEY]"), "{out}");
    assert_eq!(labels, vec!["AWS_ACCESS_KEY".to_string()]);
}

fn with_values(vals: &[(&str, &str)]) {
    let m: Vec<(String, String)> = vals
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    *state().write().unwrap() = Arc::new(build(m, vec![], BTreeMap::new()));
    ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn run_secrets_reach_commands_and_are_masked() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cfg = SecretsConfig::default();
    // SAFETY: TEST_LOCK serializes the tests that touch these variables.
    unsafe {
        std::env::set_var("ORG_DEPLOY_TOKEN", "deploy-token-value-1");
        std::env::set_var("ORG_STAGING_HOST", "staging-host-value-2");
        std::env::remove_var("CHANTO_RUN_SECRETS");
    }
    assert!(
        withhold_env("ORG_DEPLOY_TOKEN", &cfg),
        "secret-named: withheld by default"
    );
    unsafe {
        std::env::set_var(
            "CHANTO_RUN_SECRETS",
            "ORG_DEPLOY_TOKEN, ORG_STAGING_HOST,CHANTO_API_KEY",
        )
    };
    assert_eq!(
        run_secret_names(),
        ["ORG_DEPLOY_TOKEN", "ORG_STAGING_HOST"],
        "CHANTO_* never counts"
    );
    assert!(
        !withhold_env("ORG_DEPLOY_TOKEN", &cfg),
        "org secrets reach the run's commands"
    );
    assert!(
        withhold_env("CHANTO_API_KEY", &cfg),
        "the model key never does"
    );
    assert!(withhold_env("OTHER_API_KEY", &cfg));

    let root = std::env::temp_dir().join(format!(
        "chanto-run-secrets-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    init(&crate::config::Config::default(), &root);
    let masked = redact("deploy-token-value-1 staging-host-value-2");
    assert!(
        !masked.contains("deploy-token-value-1") && !masked.contains("staging-host-value-2"),
        "{masked}"
    );
    unsafe {
        std::env::remove_var("CHANTO_RUN_SECRETS");
        std::env::remove_var("ORG_DEPLOY_TOKEN");
        std::env::remove_var("ORG_STAGING_HOST");
    }
    with_values(&[]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn message_cache_tracks_edits_and_secret_changes() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_values(&[]);
    let mut cache = MessageRedactor::default();
    let mut msgs = vec![
        crate::llm::Message::user("ordinary text"),
        crate::llm::Message::user("later-known-secret"),
    ];
    let check = |cache: &mut MessageRedactor, msgs: &[crate::llm::Message]| {
        let mut expected = msgs.to_vec();
        redact_messages(&mut expected);
        let mut actual = msgs.to_vec();
        cache.redact(&mut actual);
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    };
    check(&mut cache, &msgs);
    check(&mut cache, &msgs);
    with_values(&[("NEW_KEY", "later-known-secret")]);
    check(&mut cache, &msgs);
    msgs[0].set_content("changed later-known-secret");
    msgs[0].tool_calls.push(crate::llm::ToolCall {
        id: "1".into(),
        name: "Bash".into(),
        arguments: serde_json::json!({"command": "later-known-secret"}),
    });
    check(&mut cache, &msgs);
    msgs[0].tool_calls[0].arguments = serde_json::json!({"nested": ["later-known-secret"]});
    check(&mut cache, &msgs);
    // Unchanged arguments holding a secret learned since: masked now too.
    msgs[0].tool_calls.push(crate::llm::ToolCall {
        id: "2".into(),
        name: "Read".into(),
        arguments: serde_json::json!({"file_path": "/x/second-secret-value"}),
    });
    check(&mut cache, &msgs);
    with_values(&[
        ("NEW_KEY", "later-known-secret"),
        ("SECOND", "second-secret-value"),
    ]);
    check(&mut cache, &msgs);
    msgs[0].tool_calls.truncate(1);
    check(&mut cache, &msgs);
    msgs.remove(0);
    check(&mut cache, &msgs);
    msgs.clear();
    check(&mut cache, &msgs);
    msgs.push(crate::llm::Message::user("later-known-secret"));
    ENABLED.store(false, std::sync::atomic::Ordering::Relaxed);
    check(&mut cache, &msgs);
    ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    check(&mut cache, &msgs);
    with_values(&[]);
}

#[test]
fn a_message_counts_as_unchanged_only_with_the_same_content() {
    let a = Seen::of("token = abc123");
    assert!(matches!(a, Seen::Tag(14, _)));
    assert!(a == Seen::of("token = abc123"));
    // Same length, one byte different.
    assert!(a != Seen::of("token = abc124"));
    assert!(Seen::of("") != Seen::of("\0"));
    // A same-length edit to an earlier message is scanned again.
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_values(&[("KEY", "secret-value-1")]);
    let mut cache = MessageRedactor::default();
    let mut msgs = vec![crate::llm::Message::user("plain-value-1 here")];
    cache.redact(&mut msgs.clone());
    msgs[0].set_content("secret-value-1 her");
    assert_eq!(msgs[0].content.len(), "plain-value-1 here".len());
    let mut sent = msgs.clone();
    cache.redact(&mut sent);
    assert!(
        !sent[0].content.contains("secret-value-1"),
        "{}",
        sent[0].content
    );
    with_values(&[]);
}

#[test]
#[ignore = "manual per-call redaction benchmark"]
fn benchmark_message_redaction() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_values(&[("KEY", "known-secret-value")]);
    let text =
        "A typical tool result contains source code, paths, and ordinary prose.\n".repeat(32);
    let msgs: Vec<_> = (0..500)
        .map(|_| crate::llm::Message::user(text.clone()))
        .collect();
    let mut cache = MessageRedactor::default();
    cache.redact(&mut msgs.clone());
    let start = std::time::Instant::now();
    for _ in 0..30 {
        let mut copy = msgs.clone();
        redact_messages(&mut copy);
        std::hint::black_box(copy);
    }
    let regular = start.elapsed();
    let start = std::time::Instant::now();
    for _ in 0..30 {
        let mut copy = msgs.clone();
        cache.redact(&mut copy);
        std::hint::black_box(copy);
    }
    eprintln!(
        "500 messages, {} bytes, clone + redact per call: regular {:.3} ms, cached {:.3} ms",
        text.len() * msgs.len(),
        regular.as_secs_f64() * 1000.0 / 30.0,
        start.elapsed().as_secs_f64() * 1000.0 / 30.0
    );
    with_values(&[]);
}

#[test]
fn masks_known_values_and_their_encodings() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_values(&[
        ("STRIPE_KEY", "rk_live_51Habcdefghijklmnop"),
        ("DB_PASSWORD", "hunter2-correct-horse"),
    ]);
    let b64 = base64::engine::general_purpose::STANDARD.encode("hunter2-correct-horse");
    let text = format!("key rk_live_51Habcdefghijklmnop and {b64}");
    let (out, labels) = redact_with_labels(&text);
    assert!(
        !out.contains("rk_live_51H") && !out.contains(b64.trim_end_matches('=')),
        "{out}"
    );
    assert!(
        out.contains("[REDACTED:STRIPE_KEY]") && out.contains("[REDACTED:DB_PASSWORD]"),
        "{out}"
    );
    assert_eq!(labels, vec!["DB_PASSWORD", "STRIPE_KEY"]);
    with_values(&[]);
}

#[test]
fn a_secret_debug_prints_masked_and_serializes_plain() {
    let cred = crate::auth::Credential::Oauth {
        access: "at-123".into(),
        refresh: "rt-456".into(),
        expires: 1,
        client_id: "app".into(),
    };
    let shown = format!("{cred:?}");
    assert!(shown.contains("Secret(***)") && shown.contains("app"));
    assert!(!shown.contains("at-123") && !shown.contains("rt-456"));
    assert_eq!(format!("{:?}", Secret::from("")), "Secret(\"\")");
    // `auth.json` and the keychain keep the plain value.
    let json = serde_json::to_value(&cred).unwrap();
    assert_eq!(json["access"], "at-123");
    let back: crate::auth::Credential = serde_json::from_value(json).unwrap();
    let crate::auth::Credential::Oauth { refresh, .. } = back else {
        panic!("not an OAuth credential");
    };
    assert_eq!(refresh.expose(), "rt-456");
}

#[test]
fn learned_values_survive_init_and_refresh() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = |n: &str| {
        let d = std::env::temp_dir().join(format!(
            "chanto-learned-{n}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    let (worktree, source) = (tmp("worktree"), tmp("source"));
    std::fs::write(
        source.join(".env"),
        "DB_PASSWORD=source-only-pw-7c6b5a
",
    )
    .unwrap();
    let cfg = crate::config::Config::default();
    init(&cfg, &worktree);
    register("SIGNIN_TOKEN", "learned-token-5f4e3d2c1b");
    register("SHORT", "abc");
    add_project(&source);
    // Starting over (a worker's setup) and rescanning the worktree keep them.
    init(&cfg, &worktree);
    refresh(&worktree);
    let out = redact("learned-token-5f4e3d2c1b source-only-pw-7c6b5a abc");
    assert_eq!(
        out, "[REDACTED:SIGNIN_TOKEN] [REDACTED:DB_PASSWORD] abc",
        "too short to mask safely: abc"
    );
    with_values(&[]);
    let _ = std::fs::remove_dir_all(&worktree);
    let _ = std::fs::remove_dir_all(&source);
}

#[test]
fn plain_key_names_are_secret() {
    for name in ["STRIPE_KEY", "ENCRYPTION_KEY", "master_key", "KEY"] {
        assert!(is_secret_name(name), "{name}");
    }
    for name in [
        "MONKEY",
        "KEYBOARD",
        "TURKEY_COUNT",
        "SSH_KEY_PATH",
        "KEYCHAIN",
        "CACHE_KEY",
        "SORT_KEY",
        "PRIMARY_KEY",
        "SSH_PUBLIC_KEY",
        "GPG_PUBKEY",
    ] {
        assert!(!is_secret_name(name), "{name}");
    }
}

#[test]
fn requests_and_events_are_masked_but_images_kept() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_values(&[("DEPLOY_TOKEN", "event-secret-9a8b7c6d")]);
    let clean = vec![crate::llm::Message::user("nothing here")];
    assert!(redact_request_messages(&clean).is_none());
    let mut call = crate::llm::Message::assistant("");
    call.tool_calls.push(crate::llm::ToolCall {
        id: "1".into(),
        name: "Bash".into(),
        arguments: serde_json::json!({"command": "echo event-secret-9a8b7c6d"}),
    });
    let msgs = vec![clean[0].clone(), call];
    let masked = redact_request_messages(&msgs).unwrap();
    assert_eq!(*masked[0].content, "nothing here");
    assert_eq!(
        masked[1].tool_calls[0].arguments["command"],
        "echo [REDACTED:DEPLOY_TOKEN]"
    );
    // Masking what's already masked changes nothing (a stable prefix).
    assert!(redact_request_messages(&masked).is_none());

    let mut ev = serde_json::json!({"type": "tool_end", "output": "event-secret-9a8b7c6d",
        "nested": [{"label": "x event-secret-9a8b7c6d"}], "images": ["event-secret-9a8b7c6d"]});
    redact_json(&mut ev);
    assert_eq!(ev["output"], "[REDACTED:DEPLOY_TOKEN]");
    assert_eq!(ev["nested"][0]["label"], "x [REDACTED:DEPLOY_TOKEN]");
    assert_eq!(ev["images"][0], "event-secret-9a8b7c6d");
    with_values(&[]);
}

#[test]
fn masks_token_formats_but_not_ordinary_code() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_values(&[]);
    // A failure names the format; the sample itself stays out of the log.
    for (format, sample) in [
        ("AWS access key", "AKIAIOSFODNN7EXAMPLE"),
        ("GitHub token", "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"),
        ("Slack token", "xoxb-1234567890-abcdefghij"),
        // Formats whose last character can be `-`.
        ("Slack token ending in -", "xoxb-123456789-"),
        ("GitLab token ending in -", "glpat-abcdefghij012345678-"),
        (
            "Google API key ending in -",
            // Split so secret scanners don't take the fake for a real key.
            concat!("AIza", "SyA0123456789abcdefghijklmnopqrstu-"),
        ),
        (
            "SendGrid key ending in -",
            "SG.abcdefghij0123456789ab.abcdefghij0123456789abcdefghij0123456789ab-",
        ),
        ("Anthropic key", "sk-ant-api03-abcdefghijklmnopqrstuv"),
        (
            "JWT",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        ),
        (
            "OpenSSH private key",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----",
        ),
    ] {
        assert!(
            redact(sample).starts_with("[REDACTED:"),
            "{format} not redacted"
        );
    }
    assert_eq!(
        redact("postgres://app:s3cretPass@db:5432/x"),
        "postgres://app:[REDACTED:URL_PASSWORD]@db:5432/x"
    );
    assert_eq!(
        redact("API_KEY=Zm9vYmFyMTIzNDU2Nzg5MGFiY2Rl"),
        "API_KEY=[REDACTED:API_KEY]"
    );
    for fine in [
        "const token = getToken();",
        "PASSWORD_MIN_LENGTH=12",
        "api_key = os.environ[\"API_KEY\"]",
        "let secret_name = \"database-password\";",
        "git commit -m 'Rotate tokens' && cargo test",
        "SESSION_TIMEOUT=3600",
    ] {
        assert_eq!(redact(fine), fine);
    }
}

#[test]
fn secret_paths_ignore_case_trailing_slashes_links_and_globs() {
    let home = dirs::home_dir().unwrap();
    // Directories themselves, any case.
    for p in [
        "/Users/a/.aws",
        "/Users/a/.SSH/id_rsa",
        "/Users/a/.Kube/config",
        "/Users/a/.GNUPG",
    ] {
        assert!(is_secret_path(Path::new(p)), "{p}");
    }
    // Searching the home folder (or above) reaches every credential store.
    assert!(is_secret_target(&home));
    // An ancestor of the home directory also reaches its credential stores.
    if let Some(parent) = home.parent() {
        assert!(is_secret_target(parent), "{}", parent.display());
    }
    assert!(!is_secret_target(&home.join("projects/app/src")));
    // A project file that links to a key is the key.
    let d = std::env::temp_dir().join(format!("chanto-secret-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join(".ssh")).unwrap();
    std::fs::write(d.join(".ssh/id_ed25519"), "k").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(d.join(".ssh/id_ed25519"), d.join("notes.txt")).unwrap();
        assert!(is_secret_target(&d.join("notes.txt")));
    }
    assert!(!is_secret_target(&d.join("README.md")));
    let _ = std::fs::remove_dir_all(&d);
    // Shell globs are judged by the credential files they could match.
    for c in [
        "cat ~/.d*/config.json",
        "cat ~/.ss?/id_*",
        "grep -r . ~/.kube",
        "cat ~/.AWS/credentials",
    ] {
        assert!(command_touches_secret_file(c).is_some(), "{c}");
    }
    for c in ["cat src/*.rs", "ls ~/projects/*", "grep -r TODO ."] {
        assert_eq!(command_touches_secret_file(c), None, "{c}");
    }
    // Our own credentials wherever the Chanto home is.
    assert!(is_secret_path(&crate::config::home_dir().join("auth.json")));
}

#[test]
fn env_files_and_secret_paths() {
    let vals = env_file_values(
        "# comment\nexport API_KEY=\"abc123def456ghi7\"\nPORT=3000\nNODE_ENV=production\nSESSION=Q2hhbmdlTWVQbGVhc2UxMjM0NTY3OA\nDEBUG=true\n",
    );
    let keys: Vec<&str> = vals.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["API_KEY", "SESSION"]);
    for p in [
        ".env",
        "app/.env.local",
        ".dev.vars",
        "certs/server.key",
        "/Users/a/.ssh/id_ed25519",
        "/Users/a/.aws/credentials",
        "x/service-account-prod.json",
    ] {
        assert!(is_secret_path(Path::new(p)), "{p}");
    }
    for p in [
        ".env.example",
        "src/env.ts",
        "id_ed25519.pub",
        "keys.rs",
        "README.md",
        ".envrc.sample",
    ] {
        assert!(!is_secret_path(Path::new(p)), "{p}");
    }
    assert_eq!(
        command_touches_secret_file("cat .env | grep KEY").as_deref(),
        Some(".env")
    );
    assert!(command_touches_secret_file("grep -n token ~/.aws/credentials").is_some());
    assert!(command_touches_secret_file("cp .env.example .env").is_some());
    assert!(command_touches_secret_file("cargo test && cat .env.example").is_none());
    let cfg = SecretsConfig {
        pass_env: vec!["GH_TOKEN".into(), "AWS_*".into()],
        ..Default::default()
    };
    assert!(
        withhold_env("OPENAI_API_KEY", &cfg)
            && !withhold_env("GH_TOKEN", &cfg)
            && !withhold_env("AWS_SECRET_ACCESS_KEY", &cfg)
            && !withhold_env("PATH", &cfg)
    );
    // A cloud run's commands get its run-tools token, never its model key.
    let cfg = SecretsConfig::default();
    assert!(!withhold_env("CHANTO_RUN_TOKEN", &cfg) && withhold_env("CHANTO_API_KEY", &cfg));
    // Chanto's own token files, wherever its home is.
    let home = crate::config::home_dir();
    for f in [
        "auth.json",
        "server.json",
        "mcp-auth",
        "mcp-auth/github-0123456789abcdef.json",
        "runner/tokens.json",
        "runner/connections.json",
    ] {
        assert!(is_secret_path(&home.join(f)), "{f}");
    }
    assert!(!is_secret_path(&home.join("config.toml")));
    assert!(!is_secret_path(&home.join("runs/abc/run.json")));
}
