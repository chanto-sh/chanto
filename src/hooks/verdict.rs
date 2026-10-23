//! Prompt and agent hooks: a hook whose decision is made by a model call
//! (or a read-only subagent) that returns an ok/block verdict, rather than
//! by a shell command's exit code. The core fire loop asks `supported`
//! which events this can run on, `evaluate` to get the verdict, and
//! `apply_verdict` to fold it into the HookResult.

use super::*;

/// Events whose hooks may be `prompt` hooks; `agent` hooks run on the same
/// ones except PermissionRequest (as in Claude Code).
const MODEL_HOOK_EVENTS: &[&str] = &[
    "PermissionDenied",
    "PostToolBatch",
    "PostToolUse",
    "PostToolUseFailure",
    "PreToolUse",
    "Stop",
    "SubagentStop",
    "TaskCompleted",
    "TaskCreated",
    "TeammateIdle",
    "UserPromptExpansion",
    "UserPromptSubmit",
    "PermissionRequest",
];

/// Whether a handler of this type runs on `event`, per Claude Code's table.
/// SessionStart (at launch) and Setup fire before MCP servers are available.
pub(super) fn supported(kind: &str, event: &str, payload: &Value) -> bool {
    match kind {
        "prompt" => MODEL_HOOK_EVENTS.contains(&event),
        "agent" => MODEL_HOOK_EVENTS.contains(&event) && event != "PermissionRequest",
        "http" => !matches!(event, "SessionStart" | "Setup"),
        "mcp_tool" => {
            !(event == "Setup"
                || (event == "SessionStart"
                    && matches!(
                        payload["source"].as_str(),
                        Some("startup" | "resume") | None
                    )))
        }
        _ => true,
    }
}

/// A prompt or agent hook's answer.
#[derive(Debug, PartialEq)]
pub(super) struct Verdict {
    pub(super) ok: bool,
    pub(super) reason: String,
    pub(super) impossible: bool,
}

/// The prompt a prompt or agent hook sends: `$ARGUMENTS` is the hook input
/// (`\$` keeps a literal `$`); without it the input is appended.
pub(super) fn hook_prompt(prompt: &str, payload: &Value) -> String {
    let input = serde_json::to_string_pretty(payload).unwrap_or_default();
    let escaped = prompt.replace("\\$", "\u{0}");
    let text = if escaped.contains("$ARGUMENTS") {
        escaped.replace("$ARGUMENTS", &input)
    } else {
        format!("{escaped}\n\n{input}")
    };
    text.replace('\u{0}', "$")
}

/// Read `{"ok": ..., "reason": ..., "impossible": ...}` from a model's reply
/// (the first JSON object in it). None if there isn't one.
pub(super) fn parse_verdict(text: &str) -> Option<Verdict> {
    let start = text.find('{')?;
    let v: Value = serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Value>()
        .next()?
        .ok()?;
    let ok = v["ok"].as_bool()?;
    Some(Verdict {
        ok,
        reason: v["reason"].as_str().unwrap_or("").to_string(),
        impossible: v["impossible"].as_bool().unwrap_or(false),
    })
}

const VERDICT_SYSTEM: &str = "You evaluate a hook in a coding agent: decide whether the condition or rule below holds for the \
hook input. Respond with only a JSON object: {\"ok\": true} to allow, or {\"ok\": false, \"reason\": \"why, addressed to the agent\"} \
to block. With ok false, add \"impossible\": true only if the condition can never be satisfied.";

/// Run a prompt hook (one model call) or an agent hook (a read-only
/// subagent, up to 50 turns) and read its verdict.
pub(super) async fn evaluate(
    env: &Env,
    h: &HookConfig,
    payload: &Value,
) -> Result<Verdict, String> {
    let prompt = hook_prompt(&h.prompt, payload);
    let model = h
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| env.model());
    let limit = timeout_for(h, payload);
    let reply = if h.kind == "agent" {
        let arc = session(env).ok_or("no session to run the agent hook in")?;
        // Boxed: the verifier's own turn goes through the hook code again.
        let run = crate::agent::run_hook_agent(arc, model, format!("{VERDICT_SYSTEM}\n\n{prompt}"));
        match tokio::time::timeout(limit, run).await {
            Err(_) => return Err("timed out".into()),
            Ok(r) => r.map_err(|e| format!("{e:#}"))?,
        }
    } else {
        env.accounting.check().map_err(|e| e.to_string())?;
        let (provider, name) = env.resolve(&model);
        let req = crate::llm::ChatRequest {
            model: name,
            messages: vec![
                crate::llm::Message::system(VERDICT_SYSTEM),
                crate::llm::Message::user(&prompt),
            ],
            tools: vec![],
            think: Some(false),
            effort: None,
            temperature: Some(0.0),
            max_tokens: Some(1000),
            json: true,
        };
        match tokio::time::timeout(limit, provider.chat(&req, &mut |_| {})).await {
            Err(_) => return Err("timed out".into()),
            Ok(r) => r.map_err(|e| format!("{e:#}"))?.text,
        }
    };
    parse_verdict(&reply).ok_or_else(|| {
        format!(
            "the model's reply has no {{\"ok\": ...}} verdict: {}",
            crate::util::one_line(&reply, 200)
        )
    })
}

/// What `ok: false` from a prompt or agent hook does on `event` (Claude
/// Code's table). Agent hooks act as prompt hooks with `continueOnBlock`.
pub(super) fn apply_verdict(res: &mut HookResult, event: &str, h: &HookConfig, v: Verdict) {
    if v.ok {
        return;
    }
    let reason = if v.reason.is_empty() {
        format!("blocked by {} hook", h.kind)
    } else {
        v.reason
    };
    let keep_going = h.kind == "agent" || h.continue_on_block;
    match event {
        "Stop" | "SubagentStop" => {
            if !(v.impossible && h.kind == "prompt") {
                res.block = Some(reason);
            }
        }
        "PreToolUse" if keep_going => res.decision = Some(PermDecision::Deny(reason)),
        "PreToolUse" => res.halt = Some(reason),
        "PostToolUse" | "TeammateIdle" if keep_going => res.block = Some(reason),
        "PostToolUse" | "TeammateIdle" => res.halt = Some(reason),
        // The decision is made elsewhere (or already made).
        "PermissionRequest" | "PermissionDenied" => {}
        _ => res.block = Some(reason),
    }
}
