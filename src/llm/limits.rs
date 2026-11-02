//! Hosted quota: exhausted limits vs. transient rate limits, plus the
//! account limit snapshot ChatGPT-plan requests report.

use serde_json::Value;

pub use chanto_core::limits::*;

/// Read the `x-codex-primary-*` / `x-codex-secondary-*` headers that ChatGPT-plan
/// requests carry. Absent headers (API-key auth, other providers) yield `None`
/// rather than a fabricated zero.
pub(crate) fn snapshot_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Snapshot> {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let window = |prefix: &str| -> Option<Window> {
        let used = Window::percent(get(&format!("x-codex-{prefix}-used-percent"))?)?;
        Some(Window {
            used_percent: used,
            window_minutes: get(&format!("x-codex-{prefix}-window-minutes"))
                .and_then(|v| v.trim().parse().ok()),
            resets_at: get(&format!("x-codex-{prefix}-reset-at"))
                .and_then(|v| v.trim().parse().ok()),
        })
    };
    let snapshot = Snapshot {
        primary: window("primary"),
        secondary: window("secondary"),
        plan_type: get("x-codex-plan-type").map(str::to_owned),
    };
    (!snapshot.is_empty()).then_some(snapshot)
}

#[derive(Debug)]
pub(crate) struct QuotaExhausted(String, Option<i64>);

impl QuotaExhausted {
    /// When a usage limit (not a billing quota) resets, in Unix seconds, if the provider said.
    pub(crate) fn resets_at(&self) -> Option<i64> {
        self.1
    }
}

impl std::fmt::Display for QuotaExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// When a usage limit resets: the error's `resets_at` (Unix seconds) or
/// `resets_in_seconds`, else a `Retry-After` of seconds or an HTTP date.
fn reset_time(error: &Value, retry_after: Option<&str>, now: i64) -> Option<i64> {
    let num = |k: &str| {
        error
            .get(k)
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()))
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    if let Some(at) = num("resets_at").or_else(|| num("resetsAt")) {
        return Some(at as i64);
    }
    if let Some(secs) = num("resets_in_seconds").or_else(|| num("resetsInSeconds")) {
        return Some(now + secs as i64);
    }
    let r = retry_after?.trim();
    match r.parse::<f64>() {
        Ok(secs) if secs.is_finite() && secs > 0.0 => Some(now + secs.ceil() as i64),
        Ok(_) => None,
        Err(_) => chrono::DateTime::parse_from_rfc2822(r)
            .ok()
            .map(|d| d.timestamp()),
    }
}

impl std::error::Error for QuotaExhausted {}

pub(crate) fn quota_error(
    error: &Value,
    chatgpt: bool,
    retry_after: Option<&str>,
) -> Option<QuotaExhausted> {
    let code = [error["code"].as_str(), error["type"].as_str()]
        .into_iter()
        .flatten()
        .find(|code| matches!(*code, "insufficient_quota" | "usage_limit_reached"))?;
    let label = match (chatgpt, code) {
        (true, "usage_limit_reached") => "ChatGPT usage limit reached",
        (true, _) => "ChatGPT quota exhausted",
        (false, "usage_limit_reached") => "Hosted usage limit reached",
        (false, _) => "Hosted quota exhausted",
    };
    // Preserve provider details, including reset fields, without assuming that
    // every provider exposes the same account-limit schema.
    let mut message = format!("{label}: {error}");
    if let Some(retry_after) = retry_after {
        message.push_str(&format!("\nRetry-After: {retry_after}"));
    }
    if code == "insufficient_quota" {
        message.push_str(
            "\nCheck the provider's quota and billing, or select another provider/model.",
        );
    } else {
        message.push_str(
            "\nWait for the provider's usage limit to reset, or select another provider/model.",
        );
    }
    if chatgpt {
        message.push_str(" OpenAI API quota is separate from your ChatGPT plan.");
    }
    // A billing quota doesn't reset on a clock; a plan's usage window does.
    let resets = (code == "usage_limit_reached")
        .then(|| reset_time(error, retry_after, now_secs()))
        .flatten();
    Some(QuotaExhausted(message, resets))
}

#[cfg(test)]
mod tests;
