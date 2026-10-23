//! Public URLs, in one place: moving to a new domain is an edit here (plus the
//! installers, the Homebrew formula and the docs, which can't share these).

/// The website: downloads, docs, release metadata, prices.
#[macro_export]
macro_rules! site_url {
    () => {
        "https://chanto.sh"
    };
}

/// The hosted cloud API's default address.
#[macro_export]
macro_rules! api_url {
    () => {
        "https://api.chanto.sh"
    };
}
