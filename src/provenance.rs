//! Deterministic checks for factual identifiers and numeric claims in final answers.

use regex::Regex;
use std::sync::OnceLock;

fn patterns() -> &'static [Regex; 4] {
    static PATTERNS: OnceLock<[Regex; 4]> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            Regex::new(r"\d[\d,_]*(?:\.\d+)?%?").unwrap(),
            Regex::new(r"\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+\b").unwrap(),
            Regex::new(r"\.[\w.-]+\b|(?:[\w.-]+/)+[\w./-]+|\b[\w.-]+\.(?:env|rs|py|toml|json|md|ya?ml|jsx?|tsx?|go|sql|sh|txt|csv|lock|html)\b").unwrap(),
            Regex::new(r"\b[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*\s*\(").unwrap(),
        ]
    })
}

fn claims(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (index, re) in patterns().iter().enumerate() {
        for m in re.find_iter(text) {
            if index == 0 {
                let word_or_dot = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
                let before = text[..m.start()].chars().next_back();
                let after = text[m.end()..].chars().next();
                let after_decimal = after == Some('.')
                    && text[m.end() + 1..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_digit());
                if before.is_some_and(word_or_dot)
                    || after.is_some_and(|c| c.is_alphanumeric() || c == '_')
                    || after_decimal
                {
                    continue;
                }
            }
            let mut claim = m.as_str().trim().to_string();
            if index == 0 {
                claim = claim.trim_end_matches('%').replace([',', '_'], "");
            } else if claim.ends_with('(') {
                claim.pop();
                claim = claim.trim_end().to_string();
            }
            if !claim.is_empty() && !out.contains(&claim) {
                out.push(claim);
            }
        }
    }
    out
}

/// Claims not present in this turn's tool output. Numbers compare independently
/// of digit grouping and a trailing percent sign; identifiers compare exactly.
pub(crate) fn unsupported(answer: &str, tool_output: &str) -> Vec<String> {
    let evidence = claims(tool_output);
    claims(answer)
        .into_iter()
        .filter(|claim| {
            let numeric = claim.chars().next().is_some_and(|c| c.is_ascii_digit());
            if numeric {
                let normalized = claim.trim_end_matches('%').replace([',', '_'], "");
                !evidence.iter().any(|e| {
                    e.chars().next().is_some_and(|c| c.is_ascii_digit())
                        && e.trim_end_matches('%').replace([',', '_'], "") == normalized
                })
            } else {
                !tool_output.contains(claim)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
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
}
