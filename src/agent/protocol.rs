//! The model-facing tool protocol: argument normalization, the text tool-call
//! format for models without native tool calling, and running one call.

use super::*;

/// One tool call after authorization: the call, its tool, the auth result, and
/// any hook context to fold into that call's result.
pub(super) type PlannedCall = (
    ToolCall,
    Option<Arc<dyn Tool>>,
    std::result::Result<(), String>,
    Option<String>,
);

pub(crate) enum Auth {
    Allow,
    /// Allowed, but run this call instead (e.g. the user edited a proposed change in the IDE).
    /// Run this call instead (an edit the user changed), with a note for the model.
    AllowWith(ToolCall, Option<String>),
    Deny(String),
    Stop,
}

/// Whether a provider error reads as "this model can't take images", so the
/// caller should retry without them instead of failing the turn.
pub(super) fn looks_like_image_rejection(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    let mentions_image = m.contains("image") || m.contains("vision") || m.contains("modalit");
    let rejected = m.contains("not support")
        || m.contains("unsupported")
        || m.contains("does not accept")
        || m.contains("invalid")
        || m.contains("cannot")
        || m.contains("unexpected")
        || m.contains("400");
    mentions_image && rejected
}

/// How long an unanswered permission prompt waits in auto mode before it is
/// denied so the turn can keep moving. `None` means wait indefinitely.
pub(super) fn auto_deny_after(mode: Mode, secs: Option<u64>) -> Option<Duration> {
    (mode == Mode::Auto)
        .then(|| Duration::from_secs(secs.unwrap_or(75)))
        .filter(|d| !d.is_zero())
}

/// Strip images from every message; returns how many messages had them.
pub(super) fn drop_images(msgs: &mut [Message]) -> usize {
    let mut n = 0;
    for m in msgs {
        if !m.images.is_empty() {
            m.images.clear();
            n += 1;
        }
    }
    n
}

/// The allow rules a non-interactive denial suggests: the same ones "don't
/// ask again" would add for this call, formatted for the message.
pub(super) fn deny_hint_rules(name: &str, subjects: &[String]) -> String {
    let rules = always_rules(name, subjects);
    if rules.is_empty() {
        return format!("`{name}`");
    }
    rules
        .iter()
        .map(|r| format!("`{r}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The rules "don't ask again" adds: one per distinct part of a compound
/// command, since every part must match for a command to be allowed.
pub(super) fn always_rules(name: &str, subjects: &[String]) -> Vec<String> {
    match name {
        "Bash" => {
            let mut out: Vec<String> = Vec::new();
            for subject in subjects {
                let words: Vec<&str> = subject.split_whitespace().take(2).collect();
                let Some(&first) = words.first() else {
                    continue;
                };
                // Changing directory does nothing by itself; allow it as is.
                if matches!(first, "cd" | "pushd" | "popd") && subjects.len() > 1 {
                    continue;
                }
                let plain = |w: &str| !w.starts_with('-') && !w.contains('/') && !w.contains('.');
                let prefix = if words.len() == 2 && plain(words[1]) {
                    // A package manager's script is named by the word after
                    // `run`: `npm run test` gets its own rule, not all of `npm run`.
                    match (first, words[1], subject.split_whitespace().nth(2)) {
                        ("npm" | "pnpm" | "yarn" | "bun", "run", Some(script)) if plain(script) => {
                            format!("{first} run {script}")
                        }
                        _ => words.join(" "),
                    }
                } else {
                    first.to_string()
                };
                let rule = format!("Bash({prefix}:*)");
                if !out.contains(&rule) {
                    out.push(rule);
                }
            }
            if let (true, Some(first)) = (
                out.is_empty(),
                subjects.iter().find_map(|s| s.split_whitespace().next()),
            ) {
                out.push(format!("Bash({first}:*)"));
            }
            out
        }
        "WebFetch" => vec![format!(
            "WebFetch(domain:{})",
            subjects.first().cloned().unwrap_or_default()
        )],
        _ => vec![name.to_string()],
    }
}

pub(super) fn edit_preview(cwd: &Path, name: &str, args: &Value) -> String {
    let path = crate::util::resolve(cwd, args["file_path"].as_str().unwrap_or(""));
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let new = match name {
        "Write" => args["content"].as_str().unwrap_or("").to_string(),
        "Edit" => {
            let o = args["old_string"].as_str().unwrap_or("");
            let n = args["new_string"].as_str().unwrap_or("");
            if args["replace_all"].as_bool() == Some(true) {
                old.replace(o, n)
            } else {
                old.replacen(o, n, 1)
            }
        }
        _ => {
            let mut t = old.clone();
            for e in args["edits"].as_array().into_iter().flatten() {
                t = t.replacen(
                    e["old_string"].as_str().unwrap_or(""),
                    e["new_string"].as_str().unwrap_or(""),
                    1,
                );
            }
            t
        }
    };
    format!(
        "{}\n{}",
        crate::util::rel(cwd, &path),
        crate::ui::diff(&old, &new, 60)
    )
}

/// `optimize.stable_prefix`: move the per-project end of the system message
/// (from [`crate::context::PROJECT_CONTEXT`]) to the top of the first user
/// message. The system message is then the same in every session, and so is
/// the tool list rendered after it, so a provider or local server can reuse
/// its cache of both from the first call. Done per request, so the stored
/// conversation (and compaction, resume, rewind) is unchanged.
pub(super) fn move_project_context(msgs: &mut [Message]) {
    let Some(at) = msgs
        .first()
        .filter(|m| m.role == Role::System)
        .and_then(|m| m.content.find(crate::context::PROJECT_CONTEXT))
    else {
        return;
    };
    let Some(user) = msgs.iter().position(|m| m.role == Role::User) else {
        return;
    };
    let project = msgs[0].content.split_off(at);
    let m = &mut msgs[user];
    m.content = format!(
        "<system-reminder>\n{}\n</system-reminder>\n\n{}",
        project.trim(),
        m.content
    );
}

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
pub(super) fn to_text_protocol(msgs: &[Message], tools: &[llm::ToolSpec]) -> Vec<Message> {
    let mut out = Vec::with_capacity(msgs.len());
    for m in msgs {
        match m.role {
            Role::System => {
                let mut s = m.content.clone();
                s.push_str("\n\n# Tools\nYou can call these tools. To call one, output exactly:\n<tool_call>\n{\"name\": \"ToolName\", \"arguments\": {...}}\n</tool_call>\nYou may output several tool_call blocks in one response. Then stop and wait: results arrive in <tool_response> blocks. When the task is done, reply normally without any tool_call.\n\nAvailable tools (JSON schemas):\n");
                for t in tools {
                    s.push_str(&format!("{}\n", serde_json::json!({"name": t.name, "description": t.description, "parameters": t.parameters})));
                }
                out.push(Message::system(s));
            }
            Role::Assistant if !m.tool_calls.is_empty() => {
                let mut s = m.content.clone();
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
                        prev.content.push('\n');
                        prev.content.push_str(&block);
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
pub(super) fn normalize_args(schema: &Value, args: &Value) -> Value {
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

pub(super) async fn run_tool(
    tool: &dyn Tool,
    call: &ToolCall,
    ctx: &Ctx,
    parent: Option<String>,
) -> ToolOutput {
    run_tool_with(tool, call, ctx, parent, None).await
}

/// [`run_tool`]; `bash_before` is the shadow snapshot taken just before a
/// Bash command, to show what it changed.
pub(super) async fn run_tool_with(
    tool: &dyn Tool,
    call: &ToolCall,
    ctx: &Ctx,
    parent: Option<String>,
    bash_before: Option<String>,
) -> ToolOutput {
    let env = &ctx.env;
    let started = Instant::now();
    let call = &ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments: normalize_args(&tool.schema(), &call.arguments),
    };
    env.emit(Event::ToolStart {
        id: call.id.clone(),
        label: tool.label_in(&call.arguments, env),
        parent: parent.clone(),
    });
    let sub_ctx = Ctx {
        env: ctx.env.clone(),
        cancel: ctx.cancel.clone(),
        depth: ctx.depth,
        call_id: if parent.is_none() {
            Some(call.id.clone())
        } else {
            None
        },
        cwd_override: ctx.cwd_override.clone(),
        reads: ctx.reads.clone(),
    };
    let task_parent = if tool.name() == "Task" {
        Some(call.id.clone())
    } else {
        None
    };
    let fut = async {
        if let Some(pid) = task_parent {
            TASK_PARENT
                .scope(Some(pid), tool.run(call.arguments.clone(), &sub_ctx))
                .await
        } else {
            tool.run(call.arguments.clone(), &sub_ctx).await
        }
    };
    let mut out = tokio::select! {
        r = fut => match r {
            Ok(o) => o,
            Err(e) => ToolOutput::err(format!("Error: {e:#}")),
        },
        _ = ctx.cancel.cancelled() => ToolOutput::err("Interrupted by user"),
    };
    if let Some(tree) = bash_before {
        let e = env.clone();
        let changes = tokio::task::spawn_blocking(move || {
            crate::tools::bash::changed_files(&e.shadow().ok()?, &tree, &e.root)
        })
        .await
        .ok()
        .flatten();
        if let Some(c) = changes {
            out.content.push_str(&c.note);
            let shown = out.display.take().unwrap_or_default();
            out.display = Some(if shown.is_empty() {
                c.display
            } else {
                format!("{shown}\n{}", c.display)
            });
            for (path, old, new) in &c.texts {
                env.frontend.file_diff(&call.id, path, old.as_deref(), new);
            }
        }
    }
    let root = ctx.cwd_override.clone().unwrap_or_else(|| env.root.clone());
    crate::format::after_tool(
        env,
        &root,
        &ctx.cwd(),
        &tool.name(),
        &call.arguments,
        &mut out,
    )
    .await;
    crate::lsp::after_tool(env, &tool.name(), &call.arguments, &mut out).await;
    crate::ide::after_tool(env, &tool.name(), &call.arguments, &mut out).await;
    let hook = crate::hooks::post_tool_use(
        env,
        &tool.name(),
        &call.arguments,
        &out.content,
        out.is_error,
    )
    .await;
    if let Some(o) = hook.updated_output.clone() {
        // `updatedToolOutput`: the model sees this instead; the display keeps the original.
        if out.display.is_none() {
            out.display = Some(crate::util::one_line(&out.content, 120));
        }
        out.content = o;
    }
    if let Some(fb) = &hook.block {
        out.content.push_str(&format!(
            "\n\n<system-reminder>PostToolUse hook feedback: {fb}</system-reminder>"
        ));
    }
    if let Some(c) = hook.context_text() {
        out.content
            .push_str(&format!("\n\n<system-reminder>\n{c}\n</system-reminder>"));
    }
    if let Some(r) = hook.halt {
        *env.halt.lock().unwrap() = Some(r);
    }
    if !out.is_error {
        let nested = env.nested_instructions(&call.arguments);
        if !nested.is_empty() {
            let rules = std::path::Path::new(".claude").join("rules");
            let files = nested
                .iter()
                .map(|(p, _)| {
                    (
                        p.clone(),
                        if p.to_string_lossy().contains(&*rules.to_string_lossy()) {
                            "path_glob_match"
                        } else {
                            "nested_traversal"
                        },
                    )
                })
                .collect();
            let trigger = ["file_path", "notebook_path", "path"]
                .iter()
                .find_map(|k| call.arguments[*k].as_str())
                .map(|p| crate::util::resolve(&env.cwd(), p));
            crate::hooks::instructions_loaded(env, files, trigger);
        }
        for (p, text) in nested {
            out.content.push_str(&format!(
                "\n\n<system-reminder>\nInstructions from {} (applies to files in this directory; follow them):\n{}\n</system-reminder>",
                p.display(),
                text.trim()
            ));
        }
    }
    let display = out.display.clone().unwrap_or_else(|| {
        let first = out.content.lines().next().unwrap_or("").to_string();
        let n = out.content.lines().count();
        if n > 1 {
            format!("{first} (+{})", crate::util::plural(n - 1, "line"))
        } else {
            first
        }
    });
    env.accounting
        .tool_call(&call.name, started.elapsed().as_secs_f64(), !out.is_error)
        .await;
    env.emit(Event::ToolEnd {
        id: call.id.clone(),
        display,
        content: out.content.clone(),
        is_error: out.is_error,
        images: out.images.clone(),
        parent,
    });
    out
}

tokio::task_local! {
    pub(super) static TASK_PARENT: Option<String>;
}

pub(super) fn without_repair_marker(c: &ToolCall) -> ToolCall {
    let mut c = c.clone();
    if let Some(o) = c.arguments.as_object_mut() {
        o.remove(llm::REPAIRED);
    }
    c
}

/// Map Codex-style tool calls onto Chanto tools: `shell` (argv arrays),
/// `apply_patch` sent through the shell, and `update_plan`.
pub(super) fn codex_call(call: ToolCall) -> ToolCall {
    let a = &call.arguments;
    let (name, args) = match call.name.as_str() {
        "shell" | "container.exec" | "local_shell" | "exec_command" | "Bash" => {
            let cmd = match &a["command"] {
                Value::Array(v) => {
                    let argv: Vec<String> = v
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect();
                    match argv
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .as_slice()
                    {
                        ["apply_patch" | "applypatch", patch] => {
                            return mk(call.id, "apply_patch", json!({"input": patch}));
                        }
                        [
                            "bash" | "sh" | "zsh" | "/bin/bash" | "/bin/sh" | "/bin/zsh",
                            "-lc" | "-c",
                            script,
                        ] => script.to_string(),
                        _ => shell_words::join(&argv),
                    }
                }
                Value::String(s) => s.clone(),
                _ => return call,
            };
            if let Some(patch) = crate::tools::patch::from_shell(&cmd) {
                return mk(call.id, "apply_patch", json!({"input": patch}));
            }
            if call.name == "Bash" && a["command"].is_string() {
                return call;
            }
            let mut args = json!({"command": cmd});
            if let Some(t) = a
                .get("timeout_ms")
                .or(a.get("timeout"))
                .and_then(|t| t.as_u64())
            {
                args["timeout"] = json!(t);
            }
            if let Some(w) = a.get("workdir").and_then(|w| w.as_str()) {
                args["command"] = json!(format!("cd {} && {cmd}", shell_words::quote(w)));
            }
            ("Bash", args)
        }
        "update_plan" => {
            let todos: Vec<Value> = a["plan"]
                .as_array()
                .map(|p| {
                    p.iter()
                        .map(|s| json!({"content": s["step"], "status": s["status"]}))
                        .collect()
                })
                .unwrap_or_default();
            ("TodoWrite", json!({"todos": todos}))
        }
        _ => return call,
    };
    mk(call.id, name, args)
}

pub(super) fn mk(id: String, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id,
        name: name.into(),
        arguments,
    }
}
