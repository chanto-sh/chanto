#[test]
fn model_clients_are_shared_and_know_loopback() {
    for local in [
        "http://localhost:11434",
        "http://127.0.0.1:4000/v1",
        "http://[::1]:8080/v1",
    ] {
        assert!(super::loopback_url(local), "{local}");
    }
    for remote in [
        "https://api.openai.com/v1",
        "http://192.168.1.5:11434",
        "http://localhost.example.com",
        "not a url",
    ] {
        assert!(!super::loopback_url(remote), "{remote}");
    }
    // Built once: later providers reuse the client rather than repeating
    // its setup.
    super::model_client("http://localhost:1");
    let t = std::time::Instant::now();
    for _ in 0..100 {
        super::model_client("http://localhost:1");
    }
    assert!(
        t.elapsed() < std::time::Duration::from_millis(50),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn bundled_roots_build_a_verifier() {
    let roots = super::bundled_roots();
    assert!(roots.len() > 100, "{} roots", roots.len());
    rustls::client::WebPkiServerVerifier::builder_with_provider(
        std::sync::Arc::new(roots),
        super::crypto_provider(),
    )
    .build()
    .expect("verifier from bundled roots");
}

#[test]
fn clients_share_tls_settings_like_reqwests_own() {
    // Built once and shared: every client reuses the same settings rather
    // than loading the certificate store again.
    let config = super::tls_config();
    assert!(std::ptr::eq(config, super::tls_config()));
    // What reqwest offers for a default client.
    assert_eq!(
        config.alpn_protocols,
        [b"h2".to_vec(), b"http/1.1".to_vec()]
    );
    assert!(config.enable_sni);
    super::client();
    // A local plain-HTTP model server, a local HTTPS one and a remote one
    // each get a working client.
    for url in [
        "http://127.0.0.1:1/v1",
        "https://localhost:1/v1",
        "https://api.example.com/v1",
    ] {
        super::model_client(url);
    }
}

#[test]
fn the_plain_http_client_offers_the_same_protocols() {
    let config = super::no_roots_tls();
    assert_eq!(
        config.alpn_protocols,
        [b"h2".to_vec(), b"http/1.1".to_vec()]
    );
}
