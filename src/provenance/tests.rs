use super::unsupported;

#[test]
fn accepts_supported_numbers_and_identifiers() {
    assert!(
        unsupported(
            "Set MAX_RETRIES to 1,000, edit src/main.rs and call load_config().",
            "MAX_RETRIES=1000 in src/main.rs; load_config()",
        )
        .is_empty()
    );
}

#[test]
fn reports_unsupported_numbers_and_identifiers() {
    assert_eq!(
        unsupported(
            "The service reads APP_SECRET from .env and uses migrate_db() in 2024.",
            "APP_KEY is in config.toml; call create_db() in 2025.",
        ),
        ["2024", "APP_SECRET", ".env", "migrate_db"],
    );
}

#[test]
fn does_not_match_numbers_as_substrings() {
    assert_eq!(unsupported("The value is 42.", "142"), ["42"]);
}
