//! HTTP clients. Every client trusts the system's certificate store (so
//! corporate and custom roots keep working). Where that store is empty, as in
//! slim containers without `ca-certificates`, clients trust Mozilla's roots
//! bundled into the binary instead of failing to start.
//!
//! The TLS settings are built once and shared by every client: on Linux the
//! system's verifier reads and parses the whole certificate store each time
//! it's created, which every client build used to repeat.

use std::sync::{Arc, OnceLock};

/// A client builder with the certificate roots this machine can use.
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_backend_preconfigured(tls_config().clone())
}

/// A client with default settings (in place of `reqwest::Client::new`, which
/// panics when no roots load).
pub fn client() -> reqwest::Client {
    builder().build().expect("http client")
}

/// The client model requests go through, shared by every provider so its
/// setup (TLS roots, the system's proxy settings) is paid once per process.
/// A server on this machine gets one that never looks a proxy up: traffic to
/// loopback can't usefully go through one. Over plain HTTP it never uses TLS
/// either, so it doesn't load the certificate store at all.
pub fn model_client(base_url: &str) -> reqwest::Client {
    static REMOTE: OnceLock<reqwest::Client> = OnceLock::new();
    static LOCAL: OnceLock<reqwest::Client> = OnceLock::new();
    static LOCAL_PLAIN: OnceLock<reqwest::Client> = OnceLock::new();
    let local = loopback_url(base_url);
    let plain = local && base_url.starts_with("http://");
    let slot = match (local, plain) {
        (true, true) => &LOCAL_PLAIN,
        (true, false) => &LOCAL,
        _ => &REMOTE,
    };
    slot.get_or_init(|| {
        let b = if plain {
            reqwest::Client::builder().tls_backend_preconfigured(no_roots_tls())
        } else {
            builder()
        };
        let b = b
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(300))
            // Plans used from third-party tools (Kimi Code) ask clients to identify themselves truthfully.
            .user_agent(concat!("chanto/", env!("CARGO_PKG_VERSION")));
        if local { b.no_proxy() } else { b }
            .build()
            .expect("http client")
    })
    .clone()
}

/// Whether `url` points at this machine (`localhost` or a loopback address).
fn loopback_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

/// The crypto provider reqwest itself would use: an installed default, else
/// aws-lc-rs.
fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

/// The TLS settings every client shares, built on first use: what reqwest
/// builds for a default client (TLS 1.2 and 1.3, the platform's certificate
/// verifier, HTTP/2 offered before HTTP/1.1), except that where the system
/// has no certificates, Mozilla's bundled roots stand in.
fn tls_config() -> &'static rustls::ClientConfig {
    static CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let provider = crypto_provider();
        let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> =
            match rustls_platform_verifier::Verifier::new(provider.clone()) {
                Ok(v) => Arc::new(v),
                Err(_) => rustls::client::WebPkiServerVerifier::builder_with_provider(
                    Arc::new(bundled_roots()),
                    provider.clone(),
                )
                .build()
                .expect("verifier from bundled roots"),
            };
        with_alpn(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("tls versions")
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth(),
        )
    })
}

/// TLS settings that trust no certificate: for a client that only speaks
/// plain HTTP, so building it doesn't load the certificate store. Should such
/// a server ever redirect to HTTPS, that request fails rather than going out
/// unchecked.
fn no_roots_tls() -> rustls::ClientConfig {
    with_alpn(
        rustls::ClientConfig::builder_with_provider(crypto_provider())
            .with_safe_default_protocol_versions()
            .expect("tls versions")
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth(),
    )
}

/// HTTP/2 offered before HTTP/1.1, as reqwest offers them itself.
fn with_alpn(mut config: rustls::ClientConfig) -> rustls::ClientConfig {
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

fn bundled_roots() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
    roots
}

#[cfg(test)]
mod tests;
