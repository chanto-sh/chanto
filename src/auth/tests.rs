use super::*;

#[test]
fn credentials_round_trip_with_private_permissions() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("chanto-auth-{}", std::process::id()));
    // SAFETY: test-only, serialized by TEST_HOME_LOCK.
    let previous = std::env::var_os("CHANTO_HOME");
    unsafe { std::env::set_var("CHANTO_HOME", &d) };
    set(
        "groq",
        Credential::ApiKey {
            key: "gsk_test".into(),
        },
    )
    .unwrap();
    assert!(matches!(get("groq"), Some(Credential::ApiKey { key }) if key.expose() == "gsk_test"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(remove("groq").unwrap() && get("groq").is_none());
    let id = device_id().unwrap();
    assert_eq!(id, device_id().unwrap(), "device id is stable");
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("CHANTO_HOME", previous);
        } else {
            std::env::remove_var("CHANTO_HOME");
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn keychain_keeps_secrets_out_of_the_file() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("chanto-keychain-{}", std::process::id()));
    let prev_home = std::env::var_os("CHANTO_HOME");
    let prev_kc = std::env::var_os("CHANTO_KEYCHAIN");
    // SAFETY: test-only, serialized by TEST_HOME_LOCK.
    unsafe {
        std::env::set_var("CHANTO_HOME", &d);
        std::env::set_var("CHANTO_KEYCHAIN", "1");
    }
    keychain::reset();
    set(
        "groq",
        Credential::ApiKey {
            key: "gsk_secret".into(),
        },
    )
    .unwrap();
    let raw = std::fs::read_to_string(path()).unwrap();
    assert!(
        !raw.contains("gsk_secret"),
        "the secret must not be in the file: {raw}"
    );
    assert!(raw.contains("keychain"), "a placeholder is written: {raw}");
    assert!(
        matches!(get("groq"), Some(Credential::ApiKey { key }) if key.expose() == "gsk_secret")
    );
    assert!(remove("groq").unwrap() && get("groq").is_none());
    // SAFETY: test-only, serialized by TEST_HOME_LOCK.
    unsafe {
        match prev_home {
            Some(v) => std::env::set_var("CHANTO_HOME", v),
            None => std::env::remove_var("CHANTO_HOME"),
        }
        match prev_kc {
            Some(v) => std::env::set_var("CHANTO_KEYCHAIN", v),
            None => std::env::remove_var("CHANTO_KEYCHAIN"),
        }
    }
    keychain::reset();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn the_commands_print_what_they_have_to_say() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("chanto-printed-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    // SAFETY: test-only, serialized by TEST_HOME_LOCK.
    let previous = std::env::var_os("CHANTO_HOME");
    unsafe { std::env::set_var("CHANTO_HOME", &d) };
    let printed = async |args: &[&str]| {
        let out = std::sync::Mutex::new(String::new());
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        cli(&args, &|m| out.lock().unwrap().push_str(m))
            .await
            .unwrap();
        out.into_inner().unwrap()
    };
    assert!(
        printed(&["login", "--help"])
            .await
            .starts_with("Usage: chanto login")
    );
    assert!(
        printed(&["logout", "--help"])
            .await
            .starts_with("Usage: chanto logout")
    );
    assert!(printed(&["providers"]).await.contains("openrouter"));
    assert!(
        printed(&["login"])
            .await
            .ends_with("Usage: chanto login <provider> [--as <name>]")
    );
    assert_eq!(
        printed(&["logout", "groq"]).await,
        "Wasn't logged in to groq."
    );
    unsafe {
        match previous {
            Some(previous) => std::env::set_var("CHANTO_HOME", previous),
            None => std::env::remove_var("CHANTO_HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn named_accounts_sit_beside_the_default_login() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("chanto-accounts-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    // An auth.json from before accounts existed.
    std::fs::write(d.join("auth.json"), r#"{"groqtest": {"type": "api_key", "key": "gsk_personal"}, "remote:box": {"type": "remote", "url": "https://x", "token": "t"}}"#).unwrap();
    let previous = std::env::var_os("CHANTO_HOME");
    // SAFETY: test-only, serialized by TEST_HOME_LOCK.
    unsafe { std::env::set_var("CHANTO_HOME", &d) };
    assert!(
        matches!(get("groqtest"), Some(Credential::ApiKey { key }) if key.expose() == "gsk_personal")
    );
    assert_eq!(
        account_label("groqtest"),
        None,
        "no named accounts: nothing to show"
    );
    assert!(
        cli(
            &[
                "login".into(),
                "groqtest".into(),
                "--as".into(),
                "bad name".into()
            ],
            &|_| {}
        )
        .await
        .is_err()
    );
    set_as(
        "groqtest",
        Some("work"),
        Credential::ApiKey {
            key: "gsk_work".into(),
        },
    )
    .unwrap();
    // The file keeps the old shape: the default login is still a plain entry.
    let raw: Value = serde_json::from_slice(&std::fs::read(d.join("auth.json")).unwrap()).unwrap();
    assert_eq!(raw["groqtest"]["key"], "gsk_personal");
    assert_eq!(raw["groqtest@work"]["key"], "gsk_work");
    assert_eq!(
        accounts().get("groqtest").unwrap(),
        &vec!["default".to_string(), "work".to_string()]
    );
    assert!(!accounts().contains_key("remote:box"));
    assert_eq!(account_label("groqtest").as_deref(), Some("default"));
    assert!(
        switch_account("groqtest", "nope")
            .unwrap_err()
            .to_string()
            .contains("default, work")
    );
    switch_account("groqtest", "work").unwrap();
    assert!(
        matches!(get("groqtest"), Some(Credential::ApiKey { key }) if key.expose() == "gsk_work")
    );
    assert_eq!(token("groqtest").await.unwrap(), "gsk_work");
    assert_eq!(account_label("groqtest").as_deref(), Some("work"));
    // No table in the failure messages: if it is wrong it may hold a key.
    let table = accounts_table();
    assert!(table.contains("*work"), "the table marks `work` active");
    assert!(!table.contains("gsk_"), "the table shows a key");
    switch_account("groqtest", DEFAULT_ACCOUNT).unwrap();
    assert!(
        matches!(get("groqtest"), Some(Credential::ApiKey { key }) if key.expose() == "gsk_personal")
    );
    let printed = std::sync::Mutex::new(String::new());
    cli(
        &["logout".into(), "groqtest".into(), "--as=work".into()],
        &|m| printed.lock().unwrap().push_str(m),
    )
    .await
    .unwrap();
    assert_eq!(
        *printed.lock().unwrap(),
        "Logged out of groqtest as account \"work\"."
    );
    assert!(get("groqtest").is_some() && !accounts()["groqtest"].contains(&"work".to_string()));
    SESSION_ACCOUNTS.lock().unwrap().remove("groqtest");
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("CHANTO_HOME", previous);
        } else {
            std::env::remove_var("CHANTO_HOME");
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}
