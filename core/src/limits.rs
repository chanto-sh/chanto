//! Account rate limits a provider reports (ChatGPT plans): the windows, how
//! full they are, and the warnings Chanto shows as they fill. Times are passed
//! in, so this reads no clock.

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
    pub fn percent(s: &str) -> Option<f64> {
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

pub fn reset_in(secs: i64) -> String {
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

/// Parse a usage payload: some deployments report limits in the response body
/// (or a `rate_limits` event) rather than in headers. Field names have drifted
/// upstream, so accept the documented spellings and ignore unrecognized ones.
pub fn snapshot_from_json(v: &Value) -> Option<Snapshot> {
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
