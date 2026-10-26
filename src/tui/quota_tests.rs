use super::app_tests::*;
use super::*;
use crate::llm::limits::{Snapshot, Window};
use chrono::TimeZone;

fn quota(used: f64, reset: Option<i64>) -> Snapshot {
    Snapshot {
        primary: Some(Window {
            used_percent: used,
            window_minutes: Some(300),
            resets_at: reset,
        }),
        secondary: None,
        plan_type: None,
    }
}

#[test]
fn reset_labels_distinguish_today_later_dates_and_stale_data() {
    let now = chrono::Local
        .with_ymd_and_hms(2026, 10, 25, 21, 0, 0)
        .single()
        .unwrap();
    let today = chrono::Local
        .with_ymd_and_hms(2026, 10, 25, 22, 9, 0)
        .single()
        .unwrap();
    let later = chrono::Local
        .with_ymd_and_hms(2026, 10, 27, 22, 9, 0)
        .single()
        .unwrap();
    assert_eq!(
        quota_reset(today.timestamp(), now.timestamp()).as_deref(),
        Some("resets at 10:09 PM")
    );
    assert_eq!(
        quota_reset(later.timestamp(), now.timestamp()).as_deref(),
        Some("resets Oct 27 at 10:09 PM")
    );
    assert_eq!(
        quota_reset(now.timestamp(), now.timestamp()).as_deref(),
        Some("reset due")
    );
    assert!(quota_reset(i64::MAX, now.timestamp()).is_none());
}

#[tokio::test]
async fn quota_footer_keeps_the_percentage_on_narrow_screens() {
    let mut t = test_app();
    t.limits = Some(quota(73.0, Some(chrono::Utc::now().timestamp() + 3600)));
    let wide = t.render(180, 30).last().unwrap().clone();
    assert!(
        wide.contains("5h limit: 27% left") && wide.contains("resets") && wide.contains("/usage"),
        "{wide}"
    );
    let narrow = t.render(40, 30).last().unwrap().clone();
    assert!(narrow.contains("5h 27% left"), "{narrow}");
    assert!(!narrow.contains("resets"), "{narrow}");
    assert!(unicode_width::UnicodeWidthStr::width(narrow.as_str()) <= 40);
    t.limits = Some(quota(73.0, None));
    let footer = t.render(180, 30).last().unwrap().clone();
    assert!(!footer.contains("resets"), "{footer}");
}

#[tokio::test]
async fn quota_footer_surfaces_the_more_exhausted_window() {
    let mut t = test_app();
    let mut limits = quota(10.0, None);
    limits.secondary = Some(Window {
        used_percent: 95.0,
        window_minutes: Some(10080),
        resets_at: None,
    });
    t.limits = Some(limits);
    let footer = t.render(180, 30).last().unwrap().clone();
    assert!(footer.contains("⚠ weekly limit: 5% left"), "{footer}");
}

#[tokio::test]
async fn quota_heads_up_is_once_per_threshold_and_rearms_after_a_reset() {
    let mut t = test_app();
    let count = |t: &TestApp| {
        t.blocks
            .iter()
            .filter(|b| matches!(&b.kind,Kind::Note(text,_) if text.contains("Heads up:")))
            .count()
    };
    let past = chrono::Utc::now().timestamp() - 60;
    t.on_agent(Event::Limits(quota(74.0, Some(past))));
    assert_eq!(count(&t), 0);
    t.on_agent(Event::Limits(quota(75.0, Some(past))));
    t.on_agent(Event::Limits(quota(80.0, Some(past))));
    assert_eq!(count(&t), 1);
    t.on_agent(Event::Limits(quota(90.0, Some(past))));
    t.on_agent(Event::Limits(quota(99.0, Some(past))));
    assert_eq!(count(&t), 2);
    t.on_agent(Event::Limits(quota(80.0, Some(past + 7200))));
    assert_eq!(
        count(&t),
        3,
        "a new window re-arms warnings even if first observed above 75%"
    );
    t.on_agent(Event::Limits(quota(81.0, Some(past + 7201))));
    assert_eq!(
        count(&t),
        3,
        "small provider timing changes must not repeat the warning"
    );
    assert!(t.blocks.iter().any(|b|matches!(&b.kind,Kind::Note(text,_) if text.contains("25% of your 5h limit is left") && text.contains("/usage"))));
}
