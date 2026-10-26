//! Rendering time, with a thread-local frozen clock for visual fixtures.
use super::*;

#[cfg(test)]
thread_local! {
    static FROZEN: std::cell::Cell<Option<(Instant, i64)>> = const { std::cell::Cell::new(None) };
}

pub(super) fn elapsed(start: Instant) -> Duration {
    #[cfg(test)]
    if let Some((now, _)) = FROZEN.get() {
        return now.saturating_duration_since(start);
    }
    start.elapsed()
}

pub(super) fn timestamp() -> i64 {
    #[cfg(test)]
    if let Some((_, now)) = FROZEN.get() {
        return now;
    }
    chrono::Utc::now().timestamp()
}

pub(super) fn local_time(at: i64) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let at = chrono::DateTime::from_timestamp(at, 0)?;
    #[cfg(test)]
    if FROZEN.get().is_some() {
        return Some(at.fixed_offset());
    }
    Some(at.with_timezone(&chrono::Local).fixed_offset())
}

#[cfg(test)]
pub(super) struct Frozen(Option<(Instant, i64)>);

#[cfg(test)]
impl Frozen {
    pub(super) fn new(now: Instant, timestamp: i64) -> Self {
        Self(FROZEN.replace(Some((now, timestamp))))
    }
}

#[cfg(test)]
impl Drop for Frozen {
    fn drop(&mut self) {
        FROZEN.set(self.0);
    }
}
