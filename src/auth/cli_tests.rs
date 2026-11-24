//! `chanto login`, `logout` and `providers`, in a child process with its own
//! credential home. Stdin is closed there, so prompts read an empty answer.

use super::*;
use crate::test_support::isolated;

async fn run(args: &[&str]) -> Result<Vec<String>> {
    let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let p2 = printed.clone();
    let print = move |s: &str| p2.lock().unwrap().push(s.to_string());
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    cli(&args, &print).await?;
    let out = printed.lock().unwrap().clone();
    Ok(out)
}

fn saved_key(id: &str) -> Option<String> {
    match get(id) {
        Some(Credential::ApiKey { key }) => Some(key.expose().to_string()),
        _ => None,
    }
}

#[tokio::test]
async fn login_and_logout_explain_each_refusal() {
    if isolated("auth::cli_tests::login_and_logout_explain_each_refusal") {
        return;
    }
    let text = |e: anyhow::Error| format!("{e:#}");
    assert!(text(run(&["logout"]).await.unwrap_err()).contains("usage: chanto logout"));
    assert!(text(run(&["logout", "nope"]).await.unwrap_err()).contains("unknown provider nope"));
    assert!(text(run(&["login", "nope"]).await.unwrap_err()).contains("unknown provider nope"));
    assert!(text(run(&["providers", "x"]).await.unwrap_err()).contains("unknown argument `x`"));
    assert!(
        text(run(&["login", "vertex"]).await.unwrap_err())
            .contains("Application Default Credentials")
    );
    // Stdin is empty: nothing is saved, whichever way the key was to arrive.
    assert!(
        text(
            run(&["login", "openai", "--with-api-key"])
                .await
                .unwrap_err()
        )
        .contains("no API key on stdin")
    );
    assert!(text(run(&["login", "openai"]).await.unwrap_err()).contains("no key entered"));
    assert_eq!(saved_key("openai"), None);

    let out = run(&["logout", "openai"]).await.unwrap().join("\n");
    assert_eq!(out, "Wasn't logged in to openai.");
    let out = run(&["logout", "openai", "--as", "work"])
        .await
        .unwrap()
        .join("\n");
    assert_eq!(out, "Wasn't logged in to openai as account \"work\".");

    set(
        "openai",
        Credential::ApiKey {
            key: "sk-test-value".into(),
        },
    )
    .unwrap();
    assert_eq!(token("openai").await.unwrap(), "sk-test-value");
    let out = run(&["logout", "openai"]).await.unwrap().join("\n");
    assert_eq!(out, "Logged out of openai.");
    assert_eq!(saved_key("openai"), None);
    let e = token("openai").await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "not logged in to openai; run `chanto login openai`"
    );

    // Listing never prints a saved key.
    set(
        "openai",
        Credential::ApiKey {
            key: "sk-test-value".into(),
        },
    )
    .unwrap();
    for args in [&["providers"][..], &["login"][..], &[][..]] {
        let out = run(args).await.unwrap().join("\n");
        assert!(out.contains("openai"), "{args:?}: {out}");
        assert!(!out.contains("sk-test-value"), "{args:?}: {out}");
    }
}

#[tokio::test]
async fn an_expired_sign_in_is_reported_or_used_while_valid() {
    if isolated("auth::cli_tests::an_expired_sign_in_is_reported_or_used_while_valid") {
        return;
    }
    let oauth = |expires| Credential::Oauth {
        access: "access-value".into(),
        refresh: "refresh-value".into(),
        expires,
        client_id: "c".into(),
    };
    set("other", oauth(now() + 600)).unwrap();
    assert_eq!(token("other").await.unwrap(), "access-value");
    set("other", oauth(now().saturating_sub(1))).unwrap();
    let e = token("other").await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "the other login expired; run `chanto login other`"
    );
}

#[test]
fn chatgpt_tokens_must_grant_the_direct_scope() {
    let good = serde_json::json!({
        "access_token": "a", "refresh_token": "r", "expires_in": 60,
        "scope": format!("openid {OAI_DIRECT_SCOPE}")
    });
    assert!(matches!(
        oauth_from(&good, "c").unwrap(),
        Credential::Oauth { .. }
    ));
    let narrow = serde_json::json!({"access_token": "a", "refresh_token": "r", "scope": "openid"});
    assert!(
        oauth_from(&narrow, "c")
            .unwrap_err()
            .to_string()
            .contains("didn't grant")
    );
    // A longer scope that merely contains the name doesn't count.
    let similar = serde_json::json!({
        "access_token": "a", "refresh_token": "r",
        "scope": format!("{OAI_DIRECT_SCOPE}.extra")
    });
    assert!(oauth_from(&similar, "c").is_err());
    let e = oauth_from(&serde_json::json!({"refresh_token": "r"}), "c").unwrap_err();
    assert_eq!(e.to_string(), "token response has no access_token");
    let e = oauth_from(&serde_json::json!({"access_token": "a"}), "c").unwrap_err();
    assert_eq!(e.to_string(), "token response has no refresh_token");
    assert_eq!(oai_redirect(), "http://127.0.0.1:1455/auth/callback");
}
