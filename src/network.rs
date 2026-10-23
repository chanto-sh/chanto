//! One loopback proxy per process, with authenticated, command-scoped policies.
use crate::config::SandboxConfig;
use anyhow::{Context, Result, bail};
use base64::Engine;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct Grants {
    once: HashSet<String>,
    always: HashSet<String>,
}
static GRANTS: OnceLock<Mutex<HashMap<PathBuf, Grants>>> = OnceLock::new();

pub fn grant(root: &Path, domain: &str, always: bool, cfg: &SandboxConfig) -> Result<()> {
    if always {
        let path = root.join(".chanto/config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let mut doc: toml_edit::DocumentMut = text.parse()?;
        if !doc.contains_key("sandbox") {
            doc["sandbox"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        if doc["sandbox"].get("allowed_domains").is_none() {
            let mut arr = toml_edit::Array::new();
            for d in cfg.network_domains() {
                arr.push(d);
            }
            doc["sandbox"]["allowed_domains"] = toml_edit::value(arr);
        }
        let arr = doc["sandbox"]["allowed_domains"]
            .as_array_mut()
            .context("sandbox.allowed_domains must be an array")?;
        if !arr.iter().any(|v| v.as_str() == Some(domain)) {
            arr.push(domain);
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, doc.to_string())?;
    }
    let mut grants = GRANTS.get_or_init(Default::default).lock().unwrap();
    let g = grants.entry(root.to_path_buf()).or_default();
    if always {
        g.always.insert(domain.into());
    } else {
        g.once.insert(domain.into());
    }
    Ok(())
}

pub fn domain_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let pattern = pattern.trim_end_matches('.');
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.parse::<std::net::IpAddr>().is_err()
            && host.len() > suffix.len() + 1
            && host.ends_with(suffix)
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
    } else {
        pattern == host
    }
}

struct Policy {
    allowed: Vec<String>,
    denied: Vec<String>,
    blocked: Mutex<HashSet<String>>,
}
impl Policy {
    fn allows(&self, host: &str) -> bool {
        !self.denied.iter().any(|p| domain_matches(p, host))
            && self.allowed.iter().any(|p| domain_matches(p, host))
    }
}

struct Proxy {
    port: u16,
    policies: Arc<Mutex<HashMap<String, Arc<Policy>>>>,
}
static PROXY: OnceLock<std::result::Result<Proxy, String>> = OnceLock::new();
fn proxy() -> Result<&'static Proxy> {
    PROXY
        .get_or_init(|| {
            let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .map_err(|e| e.to_string())?;
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
            let port = listener.local_addr().map_err(|e| e.to_string())?.port();
            let policies = Arc::new(Mutex::new(HashMap::new()));
            let shared = policies.clone();
            // Dedicated runtime survives the runtime that started the first command.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            std::thread::Builder::new()
                .name("sandbox-proxy".into())
                .spawn(move || {
                    runtime.block_on(async move {
                        let listener = TcpListener::from_std(listener).unwrap();
                        let slots = Arc::new(tokio::sync::Semaphore::new(128));
                        while let Ok((client, _)) = listener.accept().await {
                            let permit = slots.clone().acquire_owned().await.unwrap();
                            let shared = shared.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                let _ = serve(client, shared, port).await;
                            });
                        }
                    })
                })
                .map_err(|e| e.to_string())?;
            Ok(Proxy { port, policies })
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("sandbox filtering proxy unavailable: {e}"))
}

/// The process's filtering proxy port (started on first use).
pub fn proxy_port() -> Result<u16> {
    Ok(proxy()?.port)
}

/// Serve the same proxy (same leases) on another address too: containers on
/// a Linux bridge reach the host at the bridge's gateway, not on loopback.
/// Returns that listener's port; one listener per address per process.
pub fn listen_on(ip: std::net::IpAddr) -> Result<u16> {
    static EXTRA: OnceLock<Mutex<HashMap<std::net::IpAddr, u16>>> = OnceLock::new();
    let p = proxy()?;
    let mut extra = EXTRA.get_or_init(Default::default).lock().unwrap();
    if let Some(port) = extra.get(&ip) {
        return Ok(*port);
    }
    let listener = std::net::TcpListener::bind((ip, 0))
        .with_context(|| format!("binding the network proxy on {ip}"))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let shared = p.policies.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("sandbox-proxy-extra".into())
        .spawn(move || {
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).unwrap();
                let slots = Arc::new(tokio::sync::Semaphore::new(128));
                while let Ok((client, _)) = listener.accept().await {
                    let permit = slots.clone().acquire_owned().await.unwrap();
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = serve(client, shared, port).await;
                    });
                }
            })
        })?;
    extra.insert(ip, port);
    Ok(port)
}

pub struct Lease {
    key: String,
    policy: Arc<Policy>,
    pub port: u16,
}
impl Lease {
    pub fn new(cfg: &SandboxConfig, root: &Path) -> Result<Self> {
        let p = proxy()?;
        let mut allowed = cfg.network_domains();
        if let Some(g) = GRANTS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .get_mut(root)
        {
            allowed.extend(g.always.iter().cloned());
            allowed.extend(g.once.drain());
        }
        let policy = Arc::new(Policy {
            allowed,
            denied: cfg.denied_domains.clone(),
            blocked: Mutex::new(HashSet::new()),
        });
        let key = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:", uuid::Uuid::new_v4().simple()));
        p.policies
            .lock()
            .unwrap()
            .insert(key.clone(), policy.clone());
        Ok(Self {
            key,
            policy,
            port: p.port,
        })
    }
    pub fn url(&self) -> String {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&self.key)
            .unwrap();
        format!(
            "http://{}@127.0.0.1:{}",
            String::from_utf8(decoded).unwrap(),
            self.port
        )
    }
    /// The proxy URL as seen from elsewhere (a container's relay at `authority`).
    pub fn url_at(&self, authority: &str) -> String {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&self.key)
            .unwrap();
        format!("http://{}@{authority}", String::from_utf8(decoded).unwrap())
    }
    pub fn take_blocked(&self) -> Vec<String> {
        let mut domains: Vec<_> = self.policy.blocked.lock().unwrap().drain().collect();
        domains.sort();
        domains
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(p) = proxy() {
            p.policies.lock().unwrap().remove(&self.key);
        }
    }
}

async fn response(client: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    client.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
    Ok(())
}

pub(crate) fn authority(s: &str, default_port: u16) -> Result<(String, u16)> {
    let url = reqwest::Url::parse(&format!("http://{s}"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("invalid proxy authority");
    }
    let host = url
        .host_str()
        .context("missing host")?
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.is_empty() {
        bail!("empty host");
    }
    Ok((host, url.port().unwrap_or(default_port)))
}

async fn serve(
    mut client: TcpStream,
    policies: Arc<Mutex<HashMap<String, Arc<Policy>>>>,
    proxy_port: u16,
) -> Result<()> {
    let mut bytes = Vec::new();
    // Read exactly the header, leaving any body or TLS bytes in the socket.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while !bytes.ends_with(b"\r\n\r\n") {
            if bytes.len() >= 32 * 1024 {
                bail!("proxy header too large");
            }
            bytes.push(client.read_u8().await?);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let header = std::str::from_utf8(&bytes)?;
    let mut lines = header.split("\r\n");
    let request: Vec<_> = lines.next().unwrap_or("").split_whitespace().collect();
    if request.len() != 3 || !matches!(request[2], "HTTP/1.0" | "HTTP/1.1") {
        bail!("invalid proxy request");
    }
    let headers: Vec<_> = lines
        .filter(|l| !l.is_empty())
        .map(|l| l.split_once(':').context("invalid header"))
        .collect::<Result<_>>()?;
    // Reject whitespace-prefixed headers / obsolete folding and control bytes
    // so our Host interpretation agrees with the upstream HTTP parser.
    for (name, value) in &headers {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || value.bytes().any(|b| (b < 32 && b != b'\t') || b == 127)
        {
            bail!("invalid HTTP header");
        }
    }
    let auth: Vec<_> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("proxy-authorization"))
        .collect();
    let policy = if auth.len() == 1 {
        let key = auth[0].1.trim().strip_prefix("Basic ").unwrap_or("");
        policies.lock().unwrap().get(key).cloned()
    } else {
        None
    };
    let Some(policy) = policy else {
        return response(
            &mut client,
            "407 Proxy Authentication Required",
            "command proxy credentials required\n",
        )
        .await;
    };
    let connect = request[0] == "CONNECT";
    let hosts: Vec<_> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
        .collect();
    let (host, port, path) = if connect {
        let (host, port) = authority(request[1], 443)?;
        (host, port, String::new())
    } else {
        if hosts.len() != 1 {
            bail!("HTTP proxy requires one Host header");
        }
        let (host, port) = authority(hosts[0].1.trim(), 80)?;
        let path = if request[1].starts_with('/') {
            request[1].to_string()
        } else {
            let url = reqwest::Url::parse(request[1])?;
            if url.scheme() != "http"
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                bail!("invalid HTTP target");
            }
            let target = authority(
                url.as_str()
                    .split('/')
                    .nth(2)
                    .context("missing authority")?,
                80,
            )?;
            if target != (host.clone(), port) {
                bail!("Host and request target disagree");
            }
            format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            )
        };
        (host, port, path)
    };
    if !policy.allows(&host) || port == proxy_port {
        policy.blocked.lock().unwrap().insert(host.clone());
        return response(&mut client, "403 Forbidden", &format!("network access to {host} was blocked by the sandbox allowlist\nAdd \"{host}\" to [sandbox] allowed_domains in .chanto/config.toml.\n")).await;
    }
    let mut upstream = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        TcpStream::connect((host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => {
            return response(
                &mut client,
                "502 Bad Gateway",
                "upstream connection failed\n",
            )
            .await;
        }
    };
    if connect {
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    } else {
        let lengths: Vec<_> = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .collect();
        let transfers: Vec<_> = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
            .collect();
        if lengths.len() > 1
            || transfers.len() > 1
            || (!lengths.is_empty() && !transfers.is_empty())
        {
            bail!("ambiguous HTTP body framing");
        }
        let length = lengths
            .first()
            .map(|(_, v)| v.trim().parse::<u64>())
            .transpose()?;
        let chunked = if let Some((_, v)) = transfers.first() {
            if !v.trim().eq_ignore_ascii_case("chunked") {
                bail!("unsupported transfer encoding");
            }
            true
        } else {
            false
        };
        let mut forwarded = format!("{} {path} {}\r\n", request[0], request[2]);
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("proxy-authorization")
                || name.eq_ignore_ascii_case("proxy-connection")
                || name.eq_ignore_ascii_case("connection")
            {
                continue;
            }
            forwarded.push_str(&format!("{name}:{value}\r\n"));
        }
        forwarded.push_str("Connection: close\r\n\r\n");
        upstream.write_all(forwarded.as_bytes()).await?;
        // Forward exactly one framed request. Never tunnel plain HTTP: a second
        // request could use a denied virtual host on the same upstream socket.
        let (mut client_read, mut client_write) = client.split();
        let (mut upstream_read, mut upstream_write) = upstream.split();
        let body = async {
            if chunked {
                loop {
                    let mut line = Vec::new();
                    while !line.ends_with(b"\r\n") {
                        if line.len() >= 8192 {
                            bail!("chunk header too large");
                        }
                        line.push(client_read.read_u8().await?);
                    }
                    let size = u64::from_str_radix(
                        std::str::from_utf8(&line)?
                            .trim()
                            .split(';')
                            .next()
                            .unwrap(),
                        16,
                    )?;
                    upstream_write.write_all(&line).await?;
                    if size == 0 {
                        // Trailers aren't needed for package clients; reject them
                        // rather than permitting unvalidated routing headers.
                        let mut end = [0; 2];
                        client_read.read_exact(&mut end).await?;
                        if end != *b"\r\n" {
                            bail!("HTTP trailers unsupported");
                        }
                        upstream_write.write_all(&end).await?;
                        break;
                    }
                    let n =
                        tokio::io::copy(&mut (&mut client_read).take(size), &mut upstream_write)
                            .await?;
                    if n != size {
                        bail!("short chunk body");
                    }
                    let mut end = [0; 2];
                    client_read.read_exact(&mut end).await?;
                    if end != *b"\r\n" {
                        bail!("invalid chunk ending");
                    }
                    upstream_write.write_all(&end).await?;
                }
            } else if let Some(length) = length {
                let n = tokio::io::copy(&mut (&mut client_read).take(length), &mut upstream_write)
                    .await?;
                if n != length {
                    bail!("short HTTP body");
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let response = async {
            tokio::io::copy(&mut upstream_read, &mut client_write).await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(body, response)?;
        return Ok(());
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkPolicy;

    fn cfg(allowed: &[&str], denied: &[&str]) -> SandboxConfig {
        SandboxConfig {
            network: Some(NetworkPolicy::Allowlist),
            allowed_domains: Some(allowed.iter().map(|s| s.to_string()).collect()),
            denied_domains: denied.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn exact_wildcard_deny_and_ip() {
        let p = Policy {
            allowed: vec!["github.com".into(), "*.npmjs.org".into()],
            denied: vec!["evil.npmjs.org".into()],
            blocked: Mutex::default(),
        };
        assert!(p.allows("github.com"));
        assert!(p.allows("registry.npmjs.org"));
        assert!(p.allows("a.b.npmjs.org"));
        for host in [
            "npmjs.org",
            "evil.npmjs.org",
            "notnpmjs.org",
            "github.com.evil",
            "127.0.0.1",
            "::1",
        ] {
            assert!(!p.allows(host), "{host}");
        }
        assert!(domain_matches("GitHub.COM.", "github.com"));
        assert!(!domain_matches("*.0.0.1", "127.0.0.1"));
        assert_eq!(authority("[::1]:8080", 80).unwrap(), ("::1".into(), 8080));
        assert!(authority("user@github.com", 80).is_err());
    }

    async fn server() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(socket.read_u8().await.unwrap());
                    }
                    assert!(
                        !String::from_utf8_lossy(&header)
                            .to_ascii_lowercase()
                            .contains("proxy-authorization")
                    );
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture").await.unwrap();
                });
            }
        });
        port
    }

    async fn request(lease: &Lease, request: &str) -> String {
        let mut client = TcpStream::connect(("127.0.0.1", lease.port)).await.unwrap();
        client
            .write_all(
                format!(
                    "{request}\r\nProxy-Authorization: Basic {}\r\n\r\n",
                    lease.key
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.read_to_string(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        response
    }

    #[tokio::test]
    async fn plain_http_allows_exact_host_and_ip_when_listed() {
        let port = server().await;
        let lease = Lease::new(
            &cfg(&["127.0.0.1", "localhost"], &[]),
            Path::new("/proxy-http-test"),
        )
        .unwrap();
        for host in ["127.0.0.1", "localhost"] {
            let out = request(
                &lease,
                &format!("GET http://{host}:{port}/path?q=1 HTTP/1.1\r\nHost: {host}:{port}"),
            )
            .await;
            assert!(out.starts_with("HTTP/1.1 200"), "{out}");
            assert!(out.ends_with("fixture"));
        }
        assert!(lease.take_blocked().is_empty());
    }

    #[tokio::test]
    async fn http_and_connect_denials_are_reported_even_without_client_output() {
        let port = server().await;
        for config in [cfg(&[], &[]), cfg(&["127.0.0.1"], &["127.0.0.1"])] {
            let lease = Lease::new(&config, Path::new("/proxy-deny-test")).unwrap();
            for req in [
                format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}"),
                format!("CONNECT 127.0.0.1:{port} HTTP/1.1"),
            ] {
                let out = request(&lease, &req).await;
                assert!(out.starts_with("HTTP/1.1 403"), "{out}");
                assert!(
                    out.contains(
                        "network access to 127.0.0.1 was blocked by the sandbox allowlist"
                    )
                );
                assert!(out.contains("allowed_domains"));
                assert_eq!(lease.take_blocked(), ["127.0.0.1"]);
            }
        }
    }

    #[tokio::test]
    async fn connect_tunnels_allowed_host() {
        let port = server().await;
        let lease =
            Lease::new(&cfg(&["127.0.0.1"], &[]), Path::new("/proxy-connect-test")).unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", lease.port)).await.unwrap();
        client
            .write_all(
                format!(
                    "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nProxy-Authorization: Basic {}\r\n\r\n",
                    lease.key
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(client.read_u8().await.unwrap());
        }
        assert!(header.starts_with(b"HTTP/1.1 200"));
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        assert!(out.ends_with("fixture"));
    }

    #[tokio::test]
    async fn policies_are_isolated_and_host_mismatch_cannot_bypass() {
        let port = server().await;
        let a = Lease::new(&cfg(&["127.0.0.1"], &[]), Path::new("/proxy-isolation")).unwrap();
        let b = Lease::new(&cfg(&[], &[]), Path::new("/proxy-isolation")).unwrap();
        assert!(
            request(&b, &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}"))
                .await
                .contains("403 Forbidden")
        );
        assert!(a.take_blocked().is_empty());
        assert_eq!(b.take_blocked(), ["127.0.0.1"]);
        let out = request(
            &a,
            &format!("GET http://evil.invalid:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}"),
        )
        .await;
        assert!(!out.contains("fixture"));
        let mut client = TcpStream::connect(("127.0.0.1", a.port)).await.unwrap();
        client
            .write_all(b"CONNECT github.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("407 Proxy Authentication Required"));
    }

    #[test]
    fn grants_are_once_or_persisted_preserving_config() {
        let root = std::env::temp_dir().join(format!("chanto-net-grant-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".chanto")).unwrap();
        std::fs::write(
            root.join(".chanto/config.toml"),
            "# keep me\nmodel = 'test'\n[sandbox]\nnetwork = 'allowlist'\nallowed_domains = []\n",
        )
        .unwrap();
        let config = cfg(&[], &[]);
        grant(&root, "once.invalid", false, &config).unwrap();
        let first = Lease::new(&config, &root).unwrap();
        assert!(first.policy.allows("once.invalid"));
        assert!(
            !Lease::new(&config, &root)
                .unwrap()
                .policy
                .allows("once.invalid")
        );
        grant(&root, "saved.invalid", true, &config).unwrap();
        grant(&root, "saved.invalid", true, &config).unwrap();
        let text = std::fs::read_to_string(root.join(".chanto/config.toml")).unwrap();
        assert!(text.contains("# keep me"));
        let saved: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            saved["sandbox"]["allowed_domains"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            Lease::new(&config, &root)
                .unwrap()
                .policy
                .allows("saved.invalid")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod framing_tests {
    use super::*;
    use crate::config::NetworkPolicy;

    #[tokio::test]
    async fn plain_http_does_not_forward_a_pipelined_denied_host() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut upstream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(upstream.read_u8().await.unwrap());
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), upstream.read_u8())
                    .await
                    .is_err(),
                "a second request must never reach the upstream"
            );
            upstream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let config = SandboxConfig {
            network: Some(NetworkPolicy::Allowlist),
            allowed_domains: Some(vec!["127.0.0.1".into()]),
            ..Default::default()
        };
        let lease = Lease::new(&config, Path::new("/proxy-framing")).unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", lease.port)).await.unwrap();
        client.write_all(format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nProxy-Authorization: Basic {}\r\n\r\nGET / HTTP/1.1\r\nHost: denied.invalid\r\n\r\n", lease.key).as_bytes()).await.unwrap();
        let mut out = String::new();
        let read = client.read_to_string(&mut out).await;
        match read {
            Ok(_) => assert!(out.ends_with("ok"), "{out}"),
            // Closing with unread pipelined bytes can reset the client socket.
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn plain_http_streams_content_length_and_chunked_bodies() {
        for (framing, encoded, expected) in [
            ("Content-Length: 4", "data", "data"),
            (
                "Transfer-Encoding: chunked",
                "4\r\ndata\r\n0\r\n\r\n",
                "4\r\ndata\r\n0\r\n\r\n",
            ),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut upstream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(upstream.read_u8().await.unwrap());
                }
                let mut body = vec![0; expected.len()];
                upstream.read_exact(&mut body).await.unwrap();
                assert_eq!(body, expected.as_bytes());
                upstream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await
                    .unwrap();
            });
            let config = SandboxConfig {
                network: Some(NetworkPolicy::Allowlist),
                allowed_domains: Some(vec!["127.0.0.1".into()]),
                ..Default::default()
            };
            let lease = Lease::new(&config, Path::new("/proxy-bodies")).unwrap();
            let mut client = TcpStream::connect(("127.0.0.1", lease.port)).await.unwrap();
            client.write_all(format!("POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{framing}\r\nProxy-Authorization: Basic {}\r\n\r\n{encoded}", lease.key).as_bytes()).await.unwrap();
            let mut out = String::new();
            client.read_to_string(&mut out).await.unwrap();
            assert!(out.ends_with("ok"));
            server.await.unwrap();
        }
    }
}
