//! Entry points for the fuzz targets in `fuzz/`, built only with the
//! `fuzzing` feature. They expose the crate-internal parsers that make
//! security decisions, so the fuzz crate can call them without those
//! functions becoming public API.

use std::borrow::Cow;

/// [`crate::permissions::normalize_shell`].
pub fn normalize_shell(cmd: &str) -> String {
    crate::permissions::normalize_shell(cmd)
}

/// [`crate::secrets::redact`].
pub fn redact(s: &str) -> Cow<'_, str> {
    crate::secrets::redact(s)
}

/// [`crate::secrets::redact_always`].
pub fn redact_always(s: &str) -> Cow<'_, str> {
    crate::secrets::redact_always(s)
}

/// [`crate::secrets::redact_with_labels`].
pub fn redact_with_labels(s: &str) -> (Cow<'_, str>, Vec<String>) {
    crate::secrets::redact_with_labels(s)
}

/// [`crate::secrets::contains_secret`].
pub fn contains_secret(s: &str) -> bool {
    crate::secrets::contains_secret(s)
}

/// Whether `chanto cloud login` accepts `url` as a server address
/// ([`crate::runs::cloud::check_url`]).
pub fn cloud_url_ok(url: &str) -> bool {
    crate::runs::cloud::check_url(url).is_ok()
}

/// [`crate::network::domain_matches`]: the sandbox proxy's allow-list match.
pub fn domain_matches(pattern: &str, host: &str) -> bool {
    crate::network::domain_matches(pattern, host)
}

/// The host and port the sandbox proxy reads from a `CONNECT` target or
/// `Host` header ([`crate::network::authority`]).
pub fn proxy_authority(s: &str, default_port: u16) -> Option<(String, u16)> {
    crate::network::authority(s, default_port).ok()
}

/// The host WebFetch rules match against ([`crate::tools::web::domain_of`]).
pub fn webfetch_domain(url: &str) -> String {
    crate::tools::web::domain_of(url)
}

/// Whether a file tool's `path` argument (resolved against `cwd`) is inside
/// the project at `root`: the project test of
/// [`crate::permissions::in_workspace`], with the root it would have after
/// [`crate::permissions::set_project_root`].
pub fn in_project(path: &str, cwd: &std::path::Path, root: &std::path::Path) -> bool {
    crate::permissions::workspace_path(std::path::Path::new(path), cwd)
        .starts_with(crate::permissions::normalize(root))
}

/// The absolute path a file tool opens for its `path` argument
/// ([`crate::util::resolve`]).
pub fn tool_path(path: &str, cwd: &std::path::Path) -> std::path::PathBuf {
    crate::util::resolve(cwd, path)
}
