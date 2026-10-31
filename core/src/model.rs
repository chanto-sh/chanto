//! The model wire, without the network: what a request to a model looks like,
//! how a streamed reply is split and decoded, and how errors are read. The CLI
//! sends it with reqwest; the browser build with `fetch`.

use crate::message::*;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;

/// Reasoning effort levels, lowest first. Each provider maps them onto what it accepts.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The requested effort, if thinking is on, capped at `top` (the highest level the API takes).
pub fn effort_for(req: &ChatRequest, top: &str) -> Option<&'static str> {
    if req.think == Some(false) {
        return None;
    }
    let want = EFFORTS
        .iter()
        .position(|e| Some(*e) == req.effort.as_deref())?;
    let cap = EFFORTS
        .iter()
        .position(|e| *e == top)
        .unwrap_or(EFFORTS.len() - 1);
    Some(EFFORTS[want.min(cap)])
}

/// Splits a byte stream into lines without rescanning: a 10 MB line arriving
/// in 16 KB reads is scanned once, not once per read (which took a minute).
#[derive(Default)]
pub struct LineBuf {
    buf: Vec<u8>,
    /// The start of the next unread line.
    start: usize,
    /// Bytes already known to hold no newline.
    scanned: usize,
}

impl LineBuf {
    pub fn push(&mut self, bytes: &[u8]) {
        // Compact once per read, rather than shifting the unread chunk for
        // every line. Retain capacity for the next network read.
        if self.start > 0 {
            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(self.buf.len() - self.start);
            self.scanned -= self.start;
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete line, with its `\n`.
    pub fn next_line(&mut self) -> Option<&[u8]> {
        match self.buf[self.scanned..].iter().position(|&b| b == b'\n') {
            Some(off) => {
                let start = self.start;
                self.scanned += off + 1;
                self.start = self.scanned;
                Some(&self.buf[start..self.start])
            }
            None => {
                self.scanned = self.buf.len();
                None
            }
        }
    }

    /// What's left after the stream ended: a last line without its `\n`.
    pub fn rest(&mut self) -> Vec<u8> {
        self.buf.drain(..self.start);
        self.start = 0;
        self.scanned = 0;
        std::mem::take(&mut self.buf)
    }
}

/// Default for `stream_idle_timeout`: how long a model stream may go silent
/// once it has started.
pub const STREAM_IDLE_DEFAULT: Duration = Duration::from_secs(120);

/// The model's response stream went silent after it had started.
#[derive(Debug)]
pub struct StreamStalled(pub Duration);

impl std::fmt::Display for StreamStalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the model server stopped sending mid-response: nothing for {}s (stream timed out; `stream_idle_timeout` in config sets the limit, 0 turns it off)",
            self.0.as_secs_f64()
        )
    }
}

impl std::error::Error for StreamStalled {}

/// How long to wait before retry number `attempt` (from 0): the server's
/// `retry-after` seconds (at most a minute), else exponential backoff.
pub fn retry_wait(retry_after: Option<&str>, attempt: u32) -> Duration {
    retry_after
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite())
        .map(|s| Duration::from_secs_f64(s.clamp(0.0, 60.0)))
        .unwrap_or_else(|| Duration::from_millis(1000 << attempt.min(5)))
}

/// The server isn't listening (as opposed to slow or overloaded).
pub fn connection_refused(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("connection refused")
        || m.contains("actively refused")
        || m.contains("os error 61")
        || m.contains("os error 111")
        || m.contains("os error 10061")
}

/// The HTTP status in a model request error ("API error 503: …",
/// "Ollama error 404 Not Found: …", "… (HTTP 502)").
pub fn error_status(msg: &str) -> Option<u16> {
    ["API error ", "Ollama error ", "(HTTP "]
        .iter()
        .find_map(|pat| {
            let rest = &msg[msg.find(pat)? + pat.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            (digits.len() == 3).then(|| digits.parse().ok()).flatten()
        })
}

/// Whether a failed model request should move to a fallback model: the model
/// is overloaded, unavailable or the server failed; never auth, billing,
/// rate-limit or request errors. A local server that isn't
/// running also counts as unavailable.
pub fn warrants_fallback(msg: &str) -> bool {
    if connection_refused(msg) {
        return true;
    }
    let lower = msg.to_lowercase();
    let model_missing = lower.contains("model")
        && (lower.contains("not found")
            || lower.contains("does not exist")
            || lower.contains("not available")
            || lower.contains("unavailable"));
    match error_status(msg) {
        Some(500 | 502 | 503 | 504 | 529) => true,
        Some(404) => lower.contains("model"),
        Some(_) => false,
        None => lower.contains("overloaded") || model_missing,
    }
}

/// The innermost cause of an HTTP client error chain ("connecting to …: error
/// sending request …: tcp connect error: Connection refused (os error 61)").
pub fn root_cause(msg: &str) -> &str {
    let last = msg.rsplit(": ").next().unwrap_or(msg).trim();
    if last.is_empty() { msg } else { last }
}

/// What a model lookup changes: `(context size, vision)` from the configured
/// size, and a warning when that size exceeds what the model was trained on.
/// An OpenAI-compatible server that reports its window gets all of it (it only
/// sets when to compact), unless the user set one; Ollama keeps the default,
/// since there num_ctx is memory the server allocates.
pub fn model_info_effect(
    (ctx_len, vision): (u64, bool),
    explicit_ctx: bool,
    serving_kind: ProviderKind,
    num_ctx: u32,
) -> (u32, bool, Option<String>) {
    let mut n = num_ctx;
    if !explicit_ctx && serving_kind == ProviderKind::Openai && ctx_len > n as u64 {
        n = ctx_len.min(u32::MAX as u64) as u32;
    }
    let mut warning = None;
    if ctx_len > 0 && n as u64 > ctx_len {
        warning = Some(format!(
            "num_ctx {n} exceeds the model's trained context {ctx_len}; clamping"
        ));
        n = ctx_len as u32;
    }
    (n, vision, warning)
}

/// The model's hidden reasoning started repeating itself; retrying without
/// thinking beats waiting for it to hit the output limit.
#[derive(Debug)]
pub struct ReasoningLoop {
    pub chars: usize,
}

impl std::fmt::Display for ReasoningLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the model's reasoning started repeating itself after {} characters",
            self.chars
        )
    }
}

impl std::error::Error for ReasoningLoop {}

/// Spots a loop in streamed reasoning: two checks in a row (every 2000
/// characters) where most 5-word sequences of the last 300 words already
/// appeared earlier. Catches loops of any length; normal reasoning stays
/// under a third.
#[derive(Default)]
pub struct ReasoningWatch {
    checked: usize,
    strikes: u8,
}

impl ReasoningWatch {
    pub fn looping(&mut self, text: &str) -> bool {
        if text.len() < self.checked + 2000 {
            return false;
        }
        self.checked = text.len();
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.len() < 500 {
            return false;
        }
        let (head, tail) = words.split_at(words.len() - 300);
        let earlier: std::collections::HashSet<&[&str]> = head.windows(5).collect();
        let total = tail.len() - 4;
        let repeated = tail.windows(5).filter(|w| earlier.contains(w)).count();
        if repeated as f64 >= 0.8 * total as f64 {
            self.strikes += 1;
        } else {
            self.strikes = 0;
        }
        self.strikes >= 2
    }
}

pub fn sniff_mime(b64: &str) -> &'static str {
    if b64.starts_with("iVBOR") {
        "image/png"
    } else if b64.starts_with("R0lG") {
        "image/gif"
    } else if b64.starts_with("UklG") {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

/// The `(token, logprob)` pairs of one entry of a `logprobs` list (the
/// chosen token and its `top_logprobs` alternatives; the same shape in
/// OpenAI's and Ollama's responses).
pub fn top_logprobs(entry: &Value) -> Vec<(String, f64)> {
    let pair = |t: &Value| Some((t["token"].as_str()?.to_string(), t["logprob"].as_f64()?));
    let mut out: Vec<(String, f64)> = entry["top_logprobs"]
        .as_array()
        .map(|a| a.iter().filter_map(pair).collect())
        .unwrap_or_default();
    if out.is_empty()
        && let Some(p) = pair(entry)
    {
        out.push(p);
    }
    out
}

/// The first output token's probabilities, read as labels.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelProbs {
    /// Each label's absolute probability: the sum over the alternatives that
    /// read as it. Mass on anything else (prose, a label split into short
    /// pieces) counts for no label.
    pub mass: Vec<f64>,
    /// The label the single most likely token reads as, if it reads as one.
    pub top: Option<usize>,
}

/// The labels' probabilities among the top alternatives. A token counts for
/// a label only when it is that whole label, ignoring case and surrounding
/// blanks ("ALLOW", " allow"). A piece of one ("Al", "All", "Allo") could as
/// well begin prose ("All of this…"), so it counts for none: that keeps a
/// label's mass from being overstated, never understated in its rival's
/// favour. `None` when no alternative is a label.
pub fn label_probs_from(top: &[(String, f64)], labels: &[&str]) -> Option<LabelProbs> {
    let label_of = |token: &str| {
        let t = token.trim();
        labels.iter().position(|l| l.eq_ignore_ascii_case(t))
    };
    let mut mass = vec![0.0; labels.len()];
    for (token, lp) in top {
        if let Some(i) = label_of(token) {
            mass[i] += lp.exp();
        }
    }
    let best = top
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    (mass.iter().sum::<f64>() > 0.0).then(|| LabelProbs {
        top: best.and_then(|(t, _)| label_of(t)),
        mass,
    })
}

/// Best-effort fix for truncated JSON emitted by small models (unclosed
/// strings/brackets).
/// Marks tool-call arguments that weren't valid JSON and were repaired
/// (closed brackets, strings): they were probably cut off, so the agent
/// won't run an edit with them. Removed before the call is recorded.
pub const REPAIRED: &str = "__repaired";

/// The start of the error when the Ollama provider gets an OpenAI-style reply.
pub const OPENAI_SHAPED: &str = "This looks like an OpenAI-compatible server; set provider = \"openai\" (or kind = \"openai\" in its [providers.<name>] section) and point base_url at its /v1 endpoint";

/// A line of Ollama's `/api/chat` NDJSON stream (sent to the OpenAI provider).
pub fn ollama_shaped(line: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|v| {
        v["done"].is_boolean()
            && (v["message"].is_object() || v.get("response").is_some())
            && v.get("choices").is_none()
    })
}

/// Tool-call arguments as sent: parsed, or repaired and marked with
/// [`REPAIRED`], or kept as `__raw` when even repair fails.
pub fn parse_tool_args(raw: &str) -> Value {
    if let Ok(v) = serde_json::from_str(raw) {
        return v;
    }
    match serde_json::from_str::<Value>(&repair_json(raw)) {
        Ok(Value::Object(mut o)) => {
            o.insert(REPAIRED.into(), json!(true));
            Value::Object(o)
        }
        Ok(v) => v,
        Err(_) => json!({ "__raw": raw }),
    }
}

pub fn repair_json(s: &str) -> String {
    let mut out = String::from(s.trim());
    let mut stack = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    for c in out.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    if in_str {
        out.push('"');
    }
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    out
}

/// Some local models emit tool calls as text (Hermes/Qwen `<tool_call>` tags or
/// bare JSON) instead of using the native tool-call channel. Extract them.
pub fn extract_text_tool_calls(text: &str, known: &[String]) -> (String, Vec<ToolCall>) {
    let mut calls = Vec::new();
    let mut remaining = String::new();
    let mut rest = text;
    let tags = [
        ("<tool_call>", "</tool_call>"),
        ("<function_call>", "</function_call>"),
    ];
    loop {
        let found = tags
            .iter()
            .filter_map(|(o, c)| rest.find(o).map(|i| (i, *o, *c)))
            .min_by_key(|x| x.0);
        let Some((start, open, close)) = found else {
            remaining.push_str(rest);
            break;
        };
        remaining.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        let (body, next) = match after.find(close) {
            Some(e) => (&after[..e], &after[e + close.len()..]),
            None => (after, ""),
        };
        if let Some(c) = parse_call_json(body.trim(), known) {
            calls.push(c);
        } else {
            remaining.push_str(&rest[start..start + open.len()]);
            remaining.push_str(body);
        }
        rest = next;
        if rest.is_empty() {
            break;
        }
    }
    if calls.is_empty() {
        // Bare JSON objects as the entire message: one call, or several one
        // after another, as small models write them. Any prose, or an object
        // that isn't a call to a known tool, and none of it counts.
        let t = text.trim();
        let t = t
            .strip_prefix("```json")
            .or_else(|| t.strip_prefix("```"))
            .map(|s| s.trim_end_matches("```").trim())
            .unwrap_or(t);
        if let Some(objects) = json_objects(t) {
            let parsed: Vec<ToolCall> = objects
                .iter()
                .filter_map(|o| parse_call_json(o, known))
                .collect();
            if !parsed.is_empty() && parsed.len() == objects.len() {
                return (String::new(), parsed);
            }
        }
    }
    (remaining.trim().to_string(), calls)
}

/// `text` as consecutive top-level JSON objects separated only by
/// whitespace (or code fences), or None if anything else is there.
fn json_objects(text: &str) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        rest = rest
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_start();
        if rest.is_empty() {
            break;
        }
        if !rest.starts_with('{') {
            return None;
        }
        let (mut depth, mut in_str, mut escaped, mut end) = (0usize, false, false, None);
        for (i, ch) in rest.char_indices() {
            if in_str {
                match ch {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_str = false,
                    _ => {}
                }
                continue;
            }
            match ch {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        out.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    (!out.is_empty()).then_some(out)
}

/// The byte offset where a text-protocol tool call begins in a reply, or
/// `text.len()` when the reply is all prose. Only a tag followed by JSON counts
/// as a call, so a `<tool_call>` mentioned inline — in backticks, or in a
/// sentence about the protocol — does not hide the rest of the answer.
pub fn tool_call_start(text: &str) -> usize {
    let mut best = text.len();
    for tag in ["<tool_call", "<function_call"] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(tag) {
            let i = from + rel;
            let rest = &text[i + tag.len()..];
            let after = rest.strip_prefix('>').unwrap_or(rest).trim_start();
            if after.starts_with('{') || after.starts_with('[') {
                best = best.min(i);
                break;
            }
            from = i + tag.len();
        }
    }
    best
}

pub fn parse_call_json(body: &str, known: &[String]) -> Option<ToolCall> {
    let (v, repaired): (Value, bool) = match serde_json::from_str(body) {
        Ok(v) => (v, false),
        Err(_) => (serde_json::from_str(&repair_json(body)).ok()?, true),
    };
    let name = v["name"]
        .as_str()
        .or_else(|| v["function"]["name"].as_str())?
        .to_string();
    if !known.iter().any(|k| k == &name) {
        return None;
    }
    let args = v
        .get("arguments")
        .or_else(|| v.get("parameters"))
        .or_else(|| v["function"].get("arguments"))
        .cloned()
        .unwrap_or(json!({}));
    let mut args = match args {
        Value::String(s) => match parse_tool_args(&s) {
            v if v.get("__raw").is_some() => json!({}),
            v => v,
        },
        a => a,
    };
    if let (true, Some(o)) = (repaired, args.as_object_mut()) {
        o.insert(REPAIRED.into(), json!(true));
    }
    Some(ToolCall {
        id: new_call_id(),
        name,
        arguments: args,
    })
}

/// Rough token estimate (≈ 3.5 chars/token) for budgeting when the server
/// doesn't report usage.
pub fn estimate_tokens(msgs: &[Message]) -> u64 {
    let chars: usize = msgs
        .iter()
        .map(|m| {
            m.content.len()
                + m.tool_calls
                    .iter()
                    .map(|c| c.arguments.to_string().len() + c.name.len())
                    .sum::<usize>()
                + m.images.len() * 2000
        })
        .sum();
    (chars as f64 / 3.5) as u64
}

/// A `/chat/completions` body: the request's own fields, then its messages.
pub struct OpenaiBody<'a> {
    pub fields: &'a Value,
    pub messages: OpenaiMessages<'a>,
}

impl Serialize for OpenaiBody<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let fields = self.fields.as_object();
        let mut map = s.serialize_map(Some(fields.map_or(0, |f| f.len()) + 1))?;
        for (k, v) in fields.into_iter().flatten() {
            map.serialize_entry(k, v)?;
        }
        map.serialize_entry("messages", &self.messages)?;
        map.end()
    }
}

/// The conversation as `/chat/completions` messages. Replayed provider items
/// (a call's `extra_content`, a reply's `reasoning_content`) go back only to
/// the provider and model that produced them.
pub struct OpenaiMessages<'a> {
    pub msgs: &'a [Message],
    pub model: &'a str,
}

impl Serialize for OpenaiMessages<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        struct Content<'a>(&'a Message);
        impl Serialize for Content<'_> {
            fn serialize<S: serde::Serializer>(
                &self,
                s: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                let m = self.0;
                if m.images.is_empty() {
                    return s.serialize_str(&m.content);
                }
                let mut parts = s.serialize_seq(Some(m.images.len() + 1))?;
                parts.serialize_element(&json!({"type": "text", "text": m.content}))?;
                for img in &m.images {
                    parts.serialize_element(&json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{img}", sniff_mime(img))}}))?;
                }
                parts.end()
            }
        }
        struct Calls<'a>(&'a [ToolCall], Option<&'a ProviderItems>);
        impl Serialize for Calls<'_> {
            fn serialize<S: serde::Serializer>(
                &self,
                s: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                struct Function<'a>(&'a ToolCall);
                impl Serialize for Function<'_> {
                    fn serialize<S: serde::Serializer>(
                        &self,
                        s: S,
                    ) -> std::result::Result<S::Ok, S::Error> {
                        let mut f = s.serialize_map(Some(2))?;
                        f.serialize_entry("name", &self.0.name)?;
                        f.serialize_entry("arguments", &self.0.arguments.to_string())?;
                        f.end()
                    }
                }
                let mut seq = s.serialize_seq(Some(self.0.len()))?;
                for c in self.0 {
                    struct Call<'a>(&'a ToolCall, Option<&'a Value>);
                    impl Serialize for Call<'_> {
                        fn serialize<S: serde::Serializer>(
                            &self,
                            s: S,
                        ) -> std::result::Result<S::Ok, S::Error> {
                            let mut m = s.serialize_map(None)?;
                            m.serialize_entry("id", &self.0.id)?;
                            m.serialize_entry("type", "function")?;
                            m.serialize_entry("function", &Function(self.0))?;
                            if let Some(e) = self.1 {
                                m.serialize_entry("extra_content", &e["extra_content"])?;
                            }
                            m.end()
                        }
                    }
                    let extra = self
                        .1
                        .and_then(|p| p.items.iter().find(|i| i["id"] == c.id.as_str()));
                    seq.serialize_element(&Call(c, extra))?;
                }
                seq.end()
            }
        }
        let mut seq = s.serialize_seq(Some(self.msgs.len()))?;
        for m in self.msgs {
            struct Msg<'a>(&'a Message, Option<&'a ProviderItems>);
            impl Serialize for Msg<'_> {
                fn serialize<S: serde::Serializer>(
                    &self,
                    s: S,
                ) -> std::result::Result<S::Ok, S::Error> {
                    let (m, extra) = (self.0, self.1);
                    let mut o = s.serialize_map(None)?;
                    o.serialize_entry("role", &m.role)?;
                    o.serialize_entry("content", &Content(m))?;
                    if !m.tool_calls.is_empty() {
                        o.serialize_entry("tool_calls", &Calls(&m.tool_calls, extra))?;
                    }
                    if let Some(id) = &m.tool_call_id {
                        o.serialize_entry("tool_call_id", id)?;
                    }
                    if let Some(r) = extra
                        .and_then(|p| p.items.iter().find_map(|i| i["reasoning_content"].as_str()))
                    {
                        o.serialize_entry("reasoning_content", r)?;
                    }
                    o.end()
                }
            }
            let extra = m
                .provider_items
                .as_ref()
                .filter(|p| p.kind == ProviderKind::Openai && p.model == self.model);
            seq.serialize_element(&Msg(m, extra))?;
        }
        seq.end()
    }
}

pub fn new_call_id() -> String {
    format!("call_{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

/// Decodes an OpenAI-compatible chat completion stream (Server-Sent Events)
/// into a [`ChatResponse`], passing text and reasoning to `on_delta` as they
/// arrive. Feed it the response body's chunks in order, then [`Self::finish`].
pub struct OpenaiDecoder {
    think_off: bool,
    model: String,
    out: ChatResponse,
    watch: ReasoningWatch,
    /// The reasoning came as `reasoning_content`, which these servers want back.
    replay_reasoning: bool,
    /// (wire index, id, name, args, extra_content), in order of first appearance.
    /// The wire index is a key, not a position: a server sending index 4e9
    /// mustn't make us allocate four billion slots.
    partial: Vec<(u64, String, String, String, Value)>,
    /// `[DONE]` arrived: the server finished, rather than the connection just closing.
    done: bool,
    lines: LineBuf,
}

impl OpenaiDecoder {
    pub fn new(req: &ChatRequest) -> Self {
        OpenaiDecoder {
            think_off: req.think == Some(false),
            model: req.model.clone(),
            out: ChatResponse::default(),
            watch: ReasoningWatch::default(),
            replay_reasoning: false,
            partial: Vec::new(),
            done: false,
            lines: LineBuf::default(),
        }
    }

    /// Decode the next chunk of the body. True once the server sent `[DONE]`:
    /// the rest of the body is then ignored.
    pub fn feed(&mut self, chunk: &[u8], on_delta: &mut dyn FnMut(Delta)) -> anyhow::Result<bool> {
        self.lines.push(chunk);
        while let Some(line) = self.lines.next_line() {
            let line = String::from_utf8_lossy(line).into_owned();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                if line.starts_with('{') && ollama_shaped(line) {
                    anyhow::bail!(
                        "This looks like an Ollama server (it streams NDJSON with `message` and `done`, not OpenAI's Server-Sent Events); set provider = \"ollama\" (or kind = \"ollama\" in its [providers.<name>] section), or point base_url at its OpenAI endpoint (…/v1)"
                    );
                }
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                self.done = true;
                return Ok(true);
            }
            let v: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(e) = v.get("error") {
                anyhow::bail!("API error: {e}");
            }
            let out = &mut self.out;
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                out.usage.prompt_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
                out.usage.completion_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
                out.usage.cached_tokens = u["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .or_else(|| u["prompt_cache_hit_tokens"].as_u64())
                    .unwrap_or(0);
                out.usage.cost = u["cost"].as_f64();
            }
            let Some(choice) = v["choices"].get(0) else {
                continue;
            };
            let d = &choice["delta"];
            if let Some(t) = d["refusal"].as_str().filter(|t| !t.is_empty()) {
                out.text.push_str(t);
                on_delta(Delta::Text(t.to_string()));
                out.finish_reason = Some("refusal".into());
            }
            // Some servers send the same text under both names.
            let reasoning = ["reasoning_content", "reasoning"]
                .iter()
                .find_map(|k| d[*k].as_str().filter(|t| !t.is_empty()).map(|t| (*k, t)));
            if let Some((key, t)) = reasoning {
                self.replay_reasoning |= key == "reasoning_content";
                out.reasoning.push_str(t);
                on_delta(Delta::Reasoning(t.to_string()));
                if !self.think_off && self.watch.looping(&out.reasoning) {
                    return Err(ReasoningLoop {
                        chars: out.reasoning.len(),
                    }
                    .into());
                }
            }
            if let Some(t) = d["content"].as_str()
                && !t.is_empty()
            {
                out.text.push_str(t);
                on_delta(Delta::Text(t.to_string()));
            }
            if let Some(tcs) = d["tool_calls"].as_array() {
                let partial = &mut self.partial;
                for tc in tcs {
                    let slot = match tc["index"].as_u64() {
                        Some(i) => partial.iter().position(|p| p.0 == i),
                        // No index: a new id starts a new call, otherwise continue the last one.
                        None => match tc["id"].as_str() {
                            Some(id) if partial.last().map(|p| p.1 != id).unwrap_or(true) => None,
                            _ => partial.len().checked_sub(1),
                        },
                    };
                    let slot = slot.unwrap_or_else(|| {
                        let key = tc["index"]
                            .as_u64()
                            .unwrap_or_else(|| partial.iter().map(|p| p.0 + 1).max().unwrap_or(0));
                        partial.push((
                            key,
                            String::new(),
                            String::new(),
                            String::new(),
                            Value::Null,
                        ));
                        partial.len() - 1
                    });
                    let p = &mut partial[slot];
                    if let Some(id) = tc["id"].as_str() {
                        p.1 = id.to_string();
                    }
                    if let Some(n) = tc["function"]["name"].as_str() {
                        if p.2.is_empty() && !n.is_empty() {
                            on_delta(Delta::ToolCall(n.to_string()));
                        }
                        p.2.push_str(n);
                    }
                    match &tc["function"]["arguments"] {
                        Value::String(a) => p.3.push_str(a),
                        Value::Object(_) => p.3 = tc["function"]["arguments"].to_string(),
                        _ => {}
                    }
                    if let Some(e) = tc.get("extra_content").filter(|e| !e.is_null()) {
                        p.4 = e.clone();
                    }
                }
            }
            if let Some(f) = choice["finish_reason"].as_str()
                && (out.finish_reason.as_deref() != Some("refusal") || f == "content_filter")
            {
                out.finish_reason = Some(f.to_string());
            }
        }
        Ok(false)
    }

    /// The whole response, once the body has ended or sent `[DONE]`.
    pub fn finish(mut self) -> anyhow::Result<ChatResponse> {
        // The connection closed mid-call (no finish_reason, no [DONE]): the
        // arguments may be cut short, and repairing them would run, say, a
        // truncated Write. Fail the request instead.
        if !self.done
            && self.out.finish_reason.is_none()
            && self.partial.iter().any(|p| !p.2.is_empty())
        {
            anyhow::bail!(
                "the connection closed before the model finished its tool call (no finish_reason or [DONE])"
            );
        }
        self.partial.sort_by_key(|p| p.0);
        let mut extras = Vec::new();
        for (_, id, name, args, extra) in self.partial {
            if name.is_empty() {
                continue;
            }
            let id = if id.is_empty() { new_call_id() } else { id };
            if !extra.is_null() {
                extras.push(json!({"id": id, "extra_content": extra}));
            }
            let arguments = if args.trim().is_empty() {
                json!({})
            } else {
                parse_tool_args(&args)
            };
            self.out.tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        // DeepSeek's thinking mode (and servers like it) reject a follow-up
        // request unless the assistant turn carries its `reasoning_content` again.
        if self.replay_reasoning && !self.out.reasoning.is_empty() {
            extras.push(json!({"reasoning_content": self.out.reasoning}));
        }
        if !extras.is_empty() {
            self.out.provider_items = Some(ProviderItems {
                kind: ProviderKind::Openai,
                model: self.model,
                items: extras,
            });
        }
        Ok(self.out)
    }
}

#[cfg(test)]
mod wire_tests;
