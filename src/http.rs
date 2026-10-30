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

/// The client model requests go through, shared by every provider so its
/// setup (TLS roots, the system's proxy settings) is paid once per process.
/// A server on this machine gets one that never looks a proxy up: traffic to
/// loopback can't usefully go through one.
pub fn model_client(base_url: &str) -> reqwest::Client {
    static REMOTE: OnceLock<reqwest::Client> = OnceLock::new();
    static LOCAL: OnceLock<reqwest::Client> = OnceLock::new();
    let local = loopback_url(base_url);
    let slot = if local { &LOCAL } else { &REMOTE };
    slot.get_or_init(|| {
        let b = builder()
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

/// Whether a client with only the system's roots can't be built; checked once.
/// macOS and Windows check certificates against the system's own trust store,
/// which is always there, so only elsewhere can it come up empty, and only
/// there is a throwaway client built to find out.
fn system_roots_missing() -> bool {
    if cfg!(any(target_os = "macos", windows)) {
        return false;
    }
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
}
