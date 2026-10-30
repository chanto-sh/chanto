//! The text tool protocol, for models without native tool calls: the tools
//! go into the system prompt, calls and results into tagged text. The CLI's
//! `tool_protocol = "text"` and the browser build's small local models both
//! use it. Also the argument repair those models need.

use crate::message::{Message, Role, ToolSpec};
use serde_json::Value;

/// Neutralize the text protocol's control tags inside untrusted tool output.
/// A file or web page whose bytes contain `</tool_response>` or `<tool_call>`
/// would otherwise forge the framing a non-native-tool model parses — closing
/// the result block early, or smuggling in a call the model may echo back and
/// so get executed. A backslash after the `<` leaves the text readable but no
/// longer matching the parser (`extract_text_tool_calls`) or the block merge.
fn neutralize_protocol_tags(s: &str) -> std::borrow::Cow<'_, str> {
    const TAGS: [&str; 6] = [
        "<tool_call",
        "</tool_call",
        "<function_call",
        "</function_call",
        "<tool_response",
        "</tool_response",
    ];
    if !TAGS.iter().any(|t| s.contains(t)) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = s.to_string();
    for t in TAGS {
        out = out.replace(t, &format!("<\\{}", &t[1..]));
    }
    std::borrow::Cow::Owned(out)
}

/// Render the conversation for models without native tool calling: tool
/// schemas go into the system prompt, calls/results become tagged text.
pub fn to_text_protocol(msgs: &[Message], tools: &[ToolSpec]) -> Vec<Message> {
    let mut out = Vec::with_capacity(msgs.len());
    for m in msgs {
        match m.role {
            Role::System => {
                let mut s = String::clone(&m.content);
                s.push_str("\n\n# Tools\nYou can call these tools. To call one, output exactly:\n<tool_call>\n{\"name\": \"ToolName\", \"arguments\": {...}}\n</tool_call>\nYou may output several tool_call blocks in one response. Then stop and wait: results arrive in <tool_response> blocks. When the task is done, reply normally without any tool_call.\n\nAvailable tools (JSON schemas):\n");
                for t in tools {
                    s.push_str(&format!("{}\n", serde_json::json!({"name": t.name, "description": t.description, "parameters": t.parameters})));
                }
                out.push(Message::system(s));
            }
            Role::Assistant if !m.tool_calls.is_empty() => {
                let mut s = String::clone(&m.content);
                for c in &m.tool_calls {
                    s.push_str(&format!(
                        "\n<tool_call>\n{}\n</tool_call>",
                        serde_json::json!({"name": c.name, "arguments": c.arguments})
                    ));
                }
                out.push(Message::assistant(s.trim_start()));
            }
            Role::Tool => {
                let name = m.tool_name.clone().unwrap_or_default();
                let block = format!(
                    "<tool_response name=\"{name}\">\n{}\n</tool_response>",
                    neutralize_protocol_tags(&m.content)
                );
                // Merge consecutive tool results into one user message.
                match out.last_mut() {
                    Some(prev)
                        if prev.role == Role::User
                            && prev.content.starts_with("<tool_response") =>
                    {
                        std::sync::Arc::make_mut(&mut prev.content).push('\n');
                        std::sync::Arc::make_mut(&mut prev.content).push_str(&block);
                    }
                    _ => out.push(Message::user(block)),
                }
            }
            _ => out.push(m.clone()),
        }
    }
    out
}

/// Local models often use near-miss argument names; map common aliases onto
/// the schema's required properties.
pub fn normalize_args(schema: &Value, args: &Value) -> Value {
    let mut args = match args {
        Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        Value::Null => Value::Object(Default::default()),
        a => a.clone(),
    };
    let Some(obj) = args.as_object_mut() else {
        return args;
    };
    const ALIASES: &[(&str, &[&str])] = &[
        (
            "file_path",
            &["path", "file", "filename", "filepath", "file_name"],
        ),
        ("command", &["cmd", "script", "shell"]),
        ("pattern", &["regex", "query", "glob_pattern"]),
        (
            "old_string",
            &["old", "old_str", "search", "find", "old_text"],
        ),
        (
            "new_string",
            &["new", "new_str", "replace", "replacement", "new_text"],
        ),
        ("content", &["contents", "text", "data"]),
        ("url", &["link", "href"]),
        ("query", &["q", "search", "search_query"]),
        ("prompt", &["task", "instructions"]),
    ];
    let props = schema["properties"].as_object();
    for (canon, alts) in ALIASES {
        let wanted = props.map(|p| p.contains_key(*canon)).unwrap_or(false);
        if !wanted || obj.contains_key(*canon) {
            continue;
        }
        let in_schema = |k: &str| props.map(|p| p.contains_key(k)).unwrap_or(false);
        if let Some(alt) = alts.iter().find(|a| obj.contains_key(**a) && !in_schema(a)) {
            let v = obj.remove(*alt).unwrap();
            obj.insert(canon.to_string(), v);
        }
    }
    // Coerce stringified numbers/booleans to the schema's types.
    if let Some(p) = props {
        for (k, spec) in p {
            if let Some(Value::String(sv)) = obj.get(k) {
                let coerced = match spec["type"].as_str() {
                    Some("integer") | Some("number") => sv.trim().parse::<f64>().ok().map(|n| {
                        if spec["type"] == "integer" {
                            serde_json::json!(n as i64)
                        } else {
                            serde_json::json!(n)
                        }
                    }),
                    Some("boolean") => match sv.as_str() {
                        "true" => Some(Value::Bool(true)),
                        "false" => Some(Value::Bool(false)),
                        _ => None,
                    },
                    Some("array") | Some("object") => serde_json::from_str(sv).ok(),
                    _ => None,
                };
                if let Some(c) = coerced {
                    obj.insert(k.clone(), c);
                }
            }
        }
    }
    args
}

#[cfg(test)]
mod protocol_tests;
