//! URL and host checks behind network permissions.
//!
//! Each check is compared with the WHATWG URL parser (the `url` crate, which
//! reqwest uses and browsers agree with), since that parser decides where a
//! request really goes:
//! - `chanto cloud login` takes plain http only for a local server: an
//!   accepted `http://` URL must really name localhost or 127.0.0.1, not
//!   `http://localhost.evil.com` or `http://localhost@evil.com`.
//! - WebFetch and the browser ask before reaching cloud instance metadata:
//!   any URL the parser resolves to a metadata address must need approval.
//! - A `WebFetch(domain:…)` rule and the sandbox proxy's allow-list match
//!   only that domain and its subdomains, never `allowed.com.evil.com`.

#![no_main]

use chanto::permissions::{Rule, url_needs_approval};
use libfuzzer_sys::fuzz_target;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use url::{Host, Url};

const ALLOWED: &str = "allowed.example.com";

fn cwd() -> &'static Path {
    static CWD: OnceLock<PathBuf> = OnceLock::new();
    CWD.get_or_init(|| {
        let d = std::env::temp_dir().join("chanto-fuzz-cwd");
        let _ = std::fs::create_dir_all(&d);
        d
    })
}

fn is_metadata_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            (o[0] == 169 && o[1] == 254) || v4 == Ipv4Addr::new(100, 100, 100, 200)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_metadata_ip(IpAddr::V4(v4)),
            None => v6 == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254),
        },
    }
}

/// Whether the parser sends a request for `u` to instance metadata.
fn reaches_metadata(u: &Url) -> bool {
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    match u.host() {
        Some(Host::Ipv4(ip)) => is_metadata_ip(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => is_metadata_ip(IpAddr::V6(ip)),
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.');
            d == "metadata.google.internal" || d == "metadata"
        }
        None => false,
    }
}

fn within_allowed(host: &str) -> bool {
    host == ALLOWED || host.ends_with(&format!(".{ALLOWED}"))
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    if s.len() > 2048 {
        return;
    }
    let parsed = Url::parse(s).ok();

    // `chanto cloud login`: plain http only to this machine.
    if chanto::fuzzing::cloud_url_ok(s)
        && let Some(u) = &parsed
        && u.scheme() != "https"
    {
        let host = u.host_str().unwrap_or("");
        assert!(
            u.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1"),
            "cloud login accepts {s:?}, which sends the sign-in over {} to {host:?}",
            u.scheme()
        );
    }

    // WebFetch / Browser: instance metadata always asks.
    let needs = url_needs_approval(s, cwd());
    if let Some(u) = &parsed
        && reaches_metadata(u)
    {
        assert!(
            needs,
            "{s:?} reaches instance metadata at {:?} without approval",
            u.host_str()
        );
    }

    // `WebFetch(domain:allowed.example.com)` matches the host WebFetch requests.
    let domain = chanto::fuzzing::webfetch_domain(s);
    let rule = Rule::parse(&format!("WebFetch(domain:{ALLOWED})"));
    if rule.matches("WebFetch", &domain) {
        let host = parsed.as_ref().and_then(|u| u.host_str()).unwrap_or("");
        assert!(
            within_allowed(host),
            "rule for {ALLOWED} matches {s:?} (host {host:?})"
        );
    }

    // The sandbox proxy: an allow-listed host from a CONNECT target or Host
    // header is that domain or a subdomain of it.
    if let Some((host, _port)) = chanto::fuzzing::proxy_authority(s, 443) {
        for pattern in [ALLOWED, "*.allowed.example.com", "Allowed.Example.COM."] {
            if chanto::fuzzing::domain_matches(pattern, &host) {
                assert!(
                    within_allowed(&host),
                    "{pattern} matches {host:?} from {s:?}"
                );
            }
        }
    }
});
