//! Provider safety stops must remain visible even when no answer was sent.

use super::*;

impl ChatResponse {
    pub(crate) fn safety_stopped(&self) -> bool {
        matches!(
            self.finish_reason.as_deref(),
            Some("content_filter" | "refusal")
        )
    }

    pub(super) fn show_safety_notice(&mut self, on_delta: &mut (dyn FnMut(Delta) + Send)) {
        if !self.safety_stopped() {
            return;
        }
        // A filtered response can contain an unfinished tool call. Neither
        // native calls nor tool-shaped text from this response should run.
        self.tool_calls.clear();
        let notice = match self.finish_reason.as_deref() {
            Some("content_filter") => {
                "ⓘ This response was stopped by the provider's safety filter."
            }
            _ if self.text.trim().is_empty() => {
                "ⓘ The provider declined to respond to this request."
            }
            _ => return,
        };
        let text = if self.text.trim().is_empty() {
            notice.to_string()
        } else {
            format!("\n\n{notice}")
        };
        self.text.push_str(&text);
        on_delta(Delta::Text(text));
    }
}

#[cfg(test)]
#[path = "safety_tests.rs"]
mod tests;
