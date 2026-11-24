use super::*;

#[test]
fn only_web_addresses_go_to_the_browser() {
    let meta = |endpoint: &str| AuthMeta {
        authorization_endpoint: endpoint.to_string(),
        token_endpoint: String::new(),
        registration_endpoint: None,
        resource: String::new(),
        scopes: None,
    };
    let url = |e: &str| authorize_url(&meta(e), "c", "http://localhost/cb", "x", "s", None);
    for good in [
        "https://auth.example/a",
        "http://localhost:8080/a",
        "http://127.0.0.1/a",
    ] {
        assert!(url(good).is_ok(), "{good}");
    }
    for bad in [
        "file:///etc/passwd",
        "vscode://x/y",
        "smb://evil/share",
        "http://evil.example/a",
        "javascript:alert(1)",
    ] {
        assert!(url(bad).is_err(), "{bad}");
    }
}

#[test]
fn www_authenticate_params() {
    let h = r#"Bearer realm="mcp", resource_metadata="https://x.dev/.well-known/oauth-protected-resource", scope=read"#;
    assert_eq!(
        www_auth_param(h, "resource_metadata").as_deref(),
        Some("https://x.dev/.well-known/oauth-protected-resource")
    );
    assert_eq!(www_auth_param(h, "scope").as_deref(), Some("read"));
    assert_eq!(www_auth_param(h, "error"), None);
}

#[test]
fn pkce_is_s256() {
    use base64::Engine;
    use sha2::Digest;
    let (v, c) = pkce();
    assert!(v.len() >= 43);
    assert_eq!(
        c,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(v.as_bytes()))
    );
}

/// Live check against a real server (discovery + registration, no login):
/// `MCP_DISCOVER_URL=https://… cargo test live_discovery -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn live_discovery() {
    let url = std::env::var("MCP_DISCOVER_URL").expect("set MCP_DISCOVER_URL");
    let http = crate::http::client();
    let www = probe(&http, &url).await;
    println!("WWW-Authenticate: {www:?}");
    let meta = discover(&http, &url, www.as_deref()).await.unwrap();
    println!("{meta:#?}");
    if let Some(reg) = &meta.registration_endpoint {
        let r = register(
            &http,
            reg,
            "http://127.0.0.1:53682/callback",
            meta.scopes.as_deref(),
        )
        .await;
        println!(
            "registration: {:?}",
            r.map(|(id, secret)| (id, secret.is_some()))
        );
    }
}
