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
fn bundled_roots_build_a_client() {
    let roots = super::bundled_roots();
    assert!(roots.len() > 100, "{} roots", roots.len());
    reqwest::Client::builder()
        .tls_certs_only(roots)
        .build()
        .expect("client from bundled roots");
}
