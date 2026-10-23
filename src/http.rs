//! HTTP clients. Every client trusts the system's certificate store (so
//! corporate and custom roots keep working). Where that store is empty, as in
//! slim containers without `ca-certificates`, clients trust Mozilla's roots
//! bundled into the binary instead of failing to start.

use std::sync::OnceLock;

/// A client builder with the certificate roots this machine can use.
pub fn builder() -> reqwest::ClientBuilder {
    let b = reqwest::Client::builder();
    if system_roots_missing() {
        b.tls_certs_only(bundled_roots())
    } else {
        b
    }
}

/// A client with default settings (in place of `reqwest::Client::new`, which
/// panics when no roots load).
pub fn client() -> reqwest::Client {
    builder().build().expect("http client")
}

/// Whether a client with only the system's roots can't be built; checked once.
fn system_roots_missing() -> bool {
    static MISSING: OnceLock<bool> = OnceLock::new();
    *MISSING.get_or_init(|| reqwest::Client::builder().build().is_err())
}

fn bundled_roots() -> Vec<reqwest::Certificate> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_roots_build_a_client() {
        let roots = super::bundled_roots();
        assert!(roots.len() > 100, "{} roots", roots.len());
        reqwest::Client::builder()
            .tls_certs_only(roots)
            .build()
            .expect("client from bundled roots");
    }
}
