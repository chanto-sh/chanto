//! Hosted quota: exhausted limits vs. transient rate limits, plus the
//! account limit snapshot ChatGPT-plan requests report.

use serde_json::Value;

/// One rate-limit window (the 5-hour rolling window or the weekly one).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Window {
    pub used_percent: f64,
    pub window_minutes: Option<u64>,
    /// Unix seconds; absent when the provider omits it.
    pub resets_at: Option<i64>,
}

impl Window {
    /// `0.0`, `82.4`, `82%` → 82.4. Percentages outside 0–100 are clamped so a
    /// provider quirk can't render a nonsense bar.
    fn percent(s: &str) -> Option<f64> {
        let v: f64 = s.trim().trim_end_matches('%').parse().ok()?;
        v.is_finite().then(|| v.clamp(0.0, 100.0))
    }

    /// Short human label for the window: `5h`, `weekly`, `7d`, `30m`.
    pub fn label(&self) -> String {
        match self.window_minutes {
            Some(300) => "5h".into(),
            Some(10_080) => "weekly".into(),
            Some(m) if m % 1440 == 0 => format!("{}d", m / 1440),
            Some(m) if m % 60 == 0 => format!("{}h", m / 60),
            Some(m) => format!("{m}m"),
            None => "limit".into(),
        }
    }

    pub fn left_percent(&self) -> f64 {
        (100.0 - self.used_percent).max(0.0)
    }
}

/// The account's current limits, as reported by the provider.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
    pub plan_type: Option<String>,
}

impl Snapshot {
    pub fn is_empty(&self) -> bool {
        self.primary.is_none() && self.secondary.is_none()
    }

    fn windows(&self) -> impl Iterator<Item = (&'static str, &Window)> {
        [
            ("primary", self.primary.as_ref()),
            ("secondary", self.secondary.as_ref()),
        ]
        .into_iter()
        .filter_map(|(k, w)| w.map(|w| (k, w)))
    }

    /// Compact status-line form: `5h 82% left · weekly 36% left`.
    pub fn summary(&self) -> String {
        self.windows()
            .map(|(_, w)| format!("{} {:.0}% left", w.label(), w.left_percent()))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// Multi-line form for `/usage`, with reset times when known.
    pub fn detail(&self, now: i64) -> String {
        let mut out = Vec::new();
        if let Some(p) = &self.plan_type {
            out.push(format!("Plan: {p}"));
        }
        for (_, w) in self.windows() {
            let mut line = format!(
                "{}: {:.1}% used ({:.1}% left)",
                w.label(),
                w.used_percent,
                w.left_percent()
            );
            if let Some(at) = w.resets_at {
                let secs = at - now;
                line.push_str(&format!(", resets {}", reset_in(secs)));
            }
            out.push(line);
        }
        out.join("\n")
    }
}

/// The warning to show for a usage window, once per threshold (75% and 90%
/// used), given the highest threshold already warned about for it. Returns the
/// new threshold and the message. A window that drops back under 75% (it reset)
/// starts over: the caller stores 0.
pub fn warning(w: &Window, warned: u8, now: i64) -> Option<(u8, String)> {
    let level = if w.used_percent >= 90.0 {
        90
    } else if w.used_percent >= 75.0 {
        75
    } else {
        return None;
    };
    if level <= warned {
        return None;
    }
    let reset = w
        .resets_at
        .map(|at| format!(", resets {}", reset_in(at - now)))
        .unwrap_or_default();
    Some((
        level,
        format!(
            "Heads up: {:.0}% of your {} limit is left{reset}. Run /usage for details.",
            w.left_percent(),
            w.label(),
        ),
    ))
}

fn reset_in(secs: i64) -> String {
    if secs <= 0 {
        return "now".into();
    }
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("in {d}d {h}h")
    } else if h > 0 {
        format!("in {h}h {m}m")
    } else {
        format!("in {m}m")
    }
}

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

/// Parse a usage payload: some deployments report limits in the response body
/// (or a `rate_limits` event) rather than in headers. Field names have drifted
/// upstream, so accept the documented spellings and ignore unrecognized ones.
pub(crate) fn snapshot_from_json(v: &Value) -> Option<Snapshot> {
    // The payload nests the windows either at the top level or under rate_limits.
    let root = v.get("rate_limits").unwrap_or(v);
    let num = |v: &Value, keys: &[&str]| -> Option<f64> {
        keys.iter()
            .find_map(|k| v.get(*k))
            .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(Window::percent)))
    };
    let parse_window = |key: &str| -> Option<Window> {
        let w = root.get(key)?;
        if w.is_null() {
            return None;
        }
        Some(Window {
            used_percent: num(w, &["used_percent", "usedPercent"])?.clamp(0.0, 100.0),
            window_minutes: num(
                w,
                &["window_minutes", "windowMinutes", "window_duration_mins"],
            )
            .map(|m| m as u64),
            resets_at: num(w, &["resets_at", "resetsAt"]).map(|s| s as i64),
        })
    };
    let snapshot = Snapshot {
        primary: parse_window("primary"),
        secondary: parse_window("secondary"),
        plan_type: root
            .get("plan_type")
            .or_else(|| v.get("plan_type"))
            .and_then(|p| p.as_str())
            .map(str::to_owned),
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
mod tests {
    use super::*;
    use crate::llm::mock::{serve, sse};
    use crate::llm::{ChatRequest, Message, Provider, ProviderKind};
    use serde_json::json;

    fn provider(base: &str) -> Provider {
        Provider::new(
            ProviderKind::OpenaiResponses,
            base,
            Some("test-key".into()),
            400_000,
        )
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "gpt-5".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            think: None,
            effort: None,
            temperature: None,
            max_tokens: None,
            json: false,
        }
    }

    #[tokio::test]
    async fn quota_http_errors_are_not_retried() {
        for error in [
            json!({"message": "You exceeded your current quota, please check your plan and billing details.", "type": "insufficient_quota", "code": "insufficient_quota"}),
            json!({"type": "usage_limit_reached", "message": "You've hit your usage limit", "plan_type": "plus", "resets_at": 1791800000, "resets_in_seconds": 3600}),
            json!({"type": "rate_limit_error", "code": "insufficient_quota", "message": "quota exhausted"}),
        ] {
            let (base, seen) = serve(vec![(429, json!({"error": error}).to_string())]).await;
            let err = provider(&base)
                .post_retrying(&base, &json!({}), &[])
                .await
                .unwrap_err();
            assert!(err.downcast_ref::<QuotaExhausted>().is_some(), "{err:#}");
            let message = err.to_string();
            assert!(message.contains(&error.to_string()), "{message}");
            assert!(message.contains("Retry-After: 0"));
            assert_eq!(seen.lock().unwrap().bodies.len(), 1);
        }
    }

    #[tokio::test]
    async fn transient_and_unknown_http_errors_keep_existing_behavior() {
        for body in [
            json!({"error": {"code": "rate_limit_exceeded", "message": "slow down"}}).to_string(),
            "{}".into(),
            "not json".into(),
        ] {
            let (base, seen) = serve(vec![(429, body), (200, "ok".into())]).await;
            provider(&base)
                .post_retrying(&base, &json!({}), &[])
                .await
                .unwrap();
            assert_eq!(seen.lock().unwrap().bodies.len(), 2);
        }
        let body = json!({"error": {"code": "unknown_problem", "message": "usage_limit_reached is mentioned, not the code"}}).to_string();
        let (base, seen) = serve(vec![(400, body.clone())]).await;
        let err = provider(&base)
            .post_retrying(&base, &json!({}), &[])
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<QuotaExhausted>().is_none());
        assert_eq!(err.to_string(), format!("API error 400: {body}"));
        assert_eq!(seen.lock().unwrap().bodies.len(), 1);
    }

    #[tokio::test]
    async fn streamed_quota_failures_preserve_details() {
        let error = json!({"code": "usage_limit_reached", "message": "You've hit your usage limit. Try again after the reset.", "resets_at": 1791800000});
        for event in [
            json!({"type": "response.failed", "response": {"error": error}}),
            json!({"type": "error", "code": "usage_limit_reached", "message": "You've hit your usage limit. Try again after the reset.", "resets_at": 1791800000}),
            json!({"type": "error", "error": error}),
        ] {
            let (base, seen) = serve(vec![(200, sse(&[("error", event)]))]).await;
            let err = provider(&base)
                .chat(&request(), &mut |_| {})
                .await
                .unwrap_err();
            assert!(err.downcast_ref::<QuotaExhausted>().is_some(), "{err:#}");
            assert!(err.to_string().contains("1791800000"));
            assert!(err.to_string().contains("Try again after the reset"));
            assert!(err.to_string().contains("Retry-After: 0"));
            assert_eq!(seen.lock().unwrap().bodies.len(), 1);
        }
    }

    #[tokio::test]
    async fn unknown_stream_error_is_preserved() {
        let event =
            json!({"type": "error", "code": "new_error", "message": "specific explanation"});
        let (base, _) = serve(vec![(200, sse(&[("error", event)]))]).await;
        let err = provider(&base)
            .chat(&request(), &mut |_| {})
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<QuotaExhausted>().is_none());
        assert_eq!(err.to_string(), "API error new_error: specific explanation");
    }

    #[tokio::test]
    async fn capacity_errors_keep_the_provider_explanation() {
        let message = "Selected model is at capacity. Please try a different model.";
        let error = json!({"code": "model_capacity", "message": message});
        let body = json!({"error": error}).to_string();
        let (base, _) = serve(vec![(503, body); 5]).await;
        let err = provider(&base)
            .post_retrying(&base, &json!({}), &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains(message), "{err:#}");
        assert!(err.downcast_ref::<QuotaExhausted>().is_none());
        for event in [
            json!({"type": "error", "error": error}),
            json!({"type": "response.failed", "response": {"error": error}}),
        ] {
            let (base, _) = serve(vec![(200, sse(&[("error", event)]))]).await;
            let err = provider(&base)
                .chat(&request(), &mut |_| {})
                .await
                .unwrap_err();
            assert!(err.to_string().contains(message), "{err:#}");
            assert!(err.downcast_ref::<QuotaExhausted>().is_none());
        }
    }

    #[tokio::test]
    async fn nested_stream_errors_keep_the_provider_message() {
        // Providers nest the details under `error`; the text must survive even
        // when the code is one we don't have special handling for.
        for (event, expected) in [
            (
                json!({"type": "error", "error": {"type": "rate_limit_exceeded", "message": "Rate limit reached for gpt-6.1-sol"}}),
                "rate_limit_exceeded: Rate limit reached for gpt-6.1-sol",
            ),
            (
                json!({"type": "error", "error": "Usage limit reached for this plan"}),
                "Usage limit reached for this plan",
            ),
        ] {
            let (base, _) = serve(vec![(200, sse(&[("error", event)]))]).await;
            let err = provider(&base)
                .chat(&request(), &mut |_| {})
                .await
                .unwrap_err();
            assert!(err.to_string().contains(expected), "{err:#}");
        }
    }

    #[test]
    fn retry_after_and_chatgpt_context_are_preserved_verbatim() {
        let error = json!({"type": "usage_limit_reached", "resets_in_seconds": 3600});
        for retry_after in ["3600", "Wed, 21 Oct 2026 07:28:00 GMT"] {
            let err = quota_error(&error, true, Some(retry_after)).unwrap();
            assert!(err.to_string().contains("ChatGPT usage limit reached"));
            assert!(err.to_string().contains(retry_after));
            assert!(err.to_string().contains("3600"));
            assert!(err.to_string().contains("separate from your ChatGPT plan"));
        }
    }

    #[test]
    fn usage_limits_know_when_they_reset() {
        let now = 1_000;
        assert_eq!(
            reset_time(&json!({"resets_at": 5000}), None, now),
            Some(5000)
        );
        assert_eq!(
            reset_time(&json!({"resets_in_seconds": 60}), Some("10"), now),
            Some(1060)
        );
        assert_eq!(reset_time(&json!({}), Some("30"), now), Some(1030));
        assert_eq!(
            reset_time(&json!({}), Some("Wed, 21 Oct 2026 07:28:00 GMT"), now),
            Some(1792567680)
        );
        assert_eq!(reset_time(&json!({}), Some("0"), now), None);
        assert_eq!(reset_time(&json!({}), None, now), None);
        let e = quota_error(
            &json!({"type": "usage_limit_reached", "resets_at": 1791800000}),
            true,
            None,
        )
        .unwrap();
        assert_eq!(e.resets_at(), Some(1791800000));
        // Billing quotas don't reset on their own.
        let e = quota_error(
            &json!({"code": "insufficient_quota", "resets_at": 1791800000}),
            false,
            Some("60"),
        )
        .unwrap();
        assert_eq!(e.resets_at(), None);
    }

    fn headers(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[tokio::test]
    async fn usage_headers_are_read_from_a_real_response() {
        // The header path must work end to end, not just against a hand-built map.
        let (base, _) = serve(vec![(
            200,
            sse(&[(
                "response.completed",
                json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
            )]),
        )])
        .await;
        let p = provider(&base);
        let resp = p.chat(&request(), &mut |_| {}).await.unwrap();
        // The mock sends no limit headers, so nothing is fabricated.
        assert_eq!(resp.rate_limits, None);
    }

    #[test]
    fn codex_headers_are_parsed() {
        let s = snapshot_from_headers(&headers(&[
            ("x-codex-primary-used-percent", "18.0"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1791800000"),
            ("x-codex-secondary-used-percent", "64%"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-plan-type", "plus"),
        ]))
        .unwrap();
        let p = s.primary.as_ref().unwrap();
        assert_eq!(
            (p.used_percent, p.window_minutes, p.resets_at),
            (18.0, Some(300), Some(1791800000))
        );
        assert_eq!(p.label(), "5h");
        assert_eq!(p.left_percent(), 82.0);
        assert_eq!(s.secondary.as_ref().unwrap().label(), "weekly");
        assert_eq!(s.plan_type.as_deref(), Some("plus"));
        assert_eq!(s.summary(), "5h 82% left · weekly 36% left");
    }

    #[test]
    fn absent_or_junk_headers_yield_nothing() {
        // API-key auth and non-OpenAI providers send no such headers: that is
        // "unknown", never "0% used".
        assert_eq!(snapshot_from_headers(&headers(&[])), None);
        assert_eq!(
            snapshot_from_headers(&headers(&[("x-codex-plan-type", "plus")])),
            None
        );
        // An unusable percentage is dropped rather than shown as zero.
        assert_eq!(
            snapshot_from_headers(&headers(&[("x-codex-primary-used-percent", "n/a")])),
            None
        );
        // Out-of-range values are clamped so a bar can't invert.
        let s =
            snapshot_from_headers(&headers(&[("x-codex-primary-used-percent", "140")])).unwrap();
        assert_eq!(s.primary.unwrap().used_percent, 100.0);
    }

    #[test]
    fn usage_json_accepts_both_nestings() {
        let flat = json!({"primary": {"used_percent": 25.0, "window_minutes": 300, "resets_at": 1791800000}, "plan_type": "pro"});
        let nested = json!({"rate_limits": {"primary": {"used_percent": 25.0, "window_minutes": 300, "resets_at": 1791800000}, "plan_type": "pro"}});
        for body in [flat, nested] {
            let s = snapshot_from_json(&body).unwrap();
            assert_eq!(s.primary.as_ref().unwrap().used_percent, 25.0);
            assert_eq!(s.plan_type.as_deref(), Some("pro"));
            assert!(s.secondary.is_none());
        }
        // A weekly-only plan reports `secondary: null`, which must not panic.
        let weekly_only =
            json!({"primary": {"used_percent": 10.0, "window_minutes": 10080}, "secondary": null});
        let s = snapshot_from_json(&weekly_only).unwrap();
        assert!(s.secondary.is_none());
        assert_eq!(s.primary.unwrap().label(), "weekly");
        assert_eq!(snapshot_from_json(&json!({})), None);
        assert_eq!(snapshot_from_json(&json!({"primary": {}})), None);
    }

    #[test]
    fn detail_renders_resets_and_missing_times() {
        let s = Snapshot {
            primary: Some(Window {
                used_percent: 90.0,
                window_minutes: Some(300),
                resets_at: Some(3600),
            }),
            secondary: None,
            plan_type: Some("plus".into()),
        };
        let d = s.detail(0);
        assert!(d.contains("Plan: plus"));
        assert!(
            d.contains("5h: 90.0% used (10.0% left), resets in 1h 0m"),
            "{d}"
        );
        // No reset time means no invented countdown.
        let s = Snapshot {
            primary: Some(Window {
                used_percent: 50.0,
                window_minutes: Some(300),
                resets_at: None,
            }),
            ..Default::default()
        };
        assert!(!s.detail(0).contains("resets"));
        assert_eq!(s.detail(0), "5h: 50.0% used (50.0% left)");
    }

    #[test]
    fn window_labels_cover_the_shapes_providers_send() {
        let label = |m: Option<u64>| {
            Window {
                used_percent: 0.0,
                window_minutes: m,
                resets_at: None,
            }
            .label()
        };
        assert_eq!(label(Some(300)), "5h");
        assert_eq!(label(Some(10_080)), "weekly");
        assert_eq!(label(Some(1440)), "1d");
        assert_eq!(label(Some(60)), "1h");
        assert_eq!(label(Some(30)), "30m");
        assert_eq!(label(None), "limit");
    }

    #[test]
    fn usage_warnings_show_once_per_threshold() {
        let w = |used: f64| Window {
            used_percent: used,
            window_minutes: Some(300),
            resets_at: Some(1_000 + 3_600),
        };
        assert!(warning(&w(60.0), 0, 1_000).is_none());
        let (level, msg) = warning(&w(80.0), 0, 1_000).unwrap();
        assert_eq!(level, 75);
        assert!(
            msg.contains("Heads up: 20% of your 5h limit is left")
                && msg.contains("resets")
                && msg.contains("/usage"),
            "{msg}"
        );
        assert!(warning(&w(85.0), 75, 1_000).is_none(), "75% already shown");
        assert_eq!(warning(&w(93.0), 75, 1_000).unwrap().0, 90);
        assert!(warning(&w(99.0), 90, 1_000).is_none());
    }
}
