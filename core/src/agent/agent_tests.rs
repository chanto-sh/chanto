use super::*;
use crate::fs::{Fs, MemFs};
use crate::tools::Workspace;
use std::cell::RefCell;
use std::path::Path;

/// A model that answers each request with the next scripted reply, as
/// Server-Sent Events, and remembers what it was sent.
struct Scripted {
    replies: RefCell<Vec<Vec<Value>>>,
    sent: RefCell<Vec<(String, Value)>>,
}

impl Transport for Scripted {
    fn post<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: Vec<u8>,
    ) -> LocalFuture<'a, anyhow::Result<ByteStream<'a>>> {
        Box::pin(async move {
            assert!(
                headers
                    .iter()
                    .any(|(k, v)| k == "authorization" && v == "Bearer k")
            );
            self.sent
                .borrow_mut()
                .push((url.to_string(), serde_json::from_slice(&body)?));
            let events = self.replies.borrow_mut().remove(0);
            let sse: String = events
                .iter()
                .map(|e| format!("data: {e}\n\n"))
                .chain(["data: [DONE]\n\n".to_string()])
                .collect();
            // In small pieces, as a network delivers it.
            let chunks: Vec<anyhow::Result<Vec<u8>>> =
                sse.as_bytes().chunks(7).map(|c| Ok(c.to_vec())).collect();
            let stream: ByteStream<'a> = Box::pin(futures::stream::iter(chunks));
            Ok(stream)
        })
    }
}

fn call(id: &str, name: &str, args: Value) -> Vec<Value> {
    vec![
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id, "function": {"name": name, "arguments": args.to_string()}}]}, "finish_reason": "tool_calls"}]}),
    ]
}

fn text(t: &str) -> Vec<Value> {
    vec![
        json!({"choices": [{"delta": {"content": t}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ]
}

/// A whole turn: read, edit, check, answer. It runs natively and as
/// WebAssembly (`cargo test --target wasm32-wasip1`), and both must reach the
/// same files, events and requests.
#[test]
fn a_turn_reads_edits_checks_and_answers() {
    let fs = MemFs::new();
    fs.insert(
        "/p/src/main.rs",
        "fn main() {\n    println!(\"helo\");\n}\n",
    );
    let model = Scripted {
        replies: RefCell::new(vec![
            call("c1", "Read", json!({"file_path": "src/main.rs"})),
            call(
                "c2",
                "Edit",
                json!({"file_path": "src/main.rs", "old_string": "helo", "new_string": "hello"}),
            ),
            call(
                "c3",
                "Grep",
                json!({"pattern": "hello", "output_mode": "content"}),
            ),
            text("Fixed the typo in src/main.rs."),
        ]),
        sent: RefCell::new(vec![]),
    };
    let mut agent = Agent::new(
        ModelConfig {
            base_url: "https://models.example/v1/".into(),
            model: "m".into(),
            api_key: Some("k".into()),
        },
        "You are a coding agent.",
    );
    let mut workspace = Workspace::new(&fs, "/p");
    let mut events = Vec::new();
    let answer = futures::executor::block_on(agent.run_turn(
        "fix the typo",
        &model,
        &mut workspace,
        &mut |e| events.push(e),
    ))
    .unwrap();

    assert_eq!(answer, "Fixed the typo in src/main.rs.");
    assert_eq!(
        fs.read(Path::new("/p/src/main.rs")).unwrap(),
        b"fn main() {\n    println!(\"hello\");\n}\n"
    );
    let summary: Vec<String> = events
        .iter()
        .map(|e| match e {
            Event::Request { step } => format!("request {step}"),
            Event::Reply { step, .. } => format!("reply {step}"),
            Event::Text { text } => format!("text {text}"),
            Event::Reasoning { text } => format!("reasoning {text}"),
            Event::ToolCall { name, .. } => format!("call {name}"),
            Event::ToolResult {
                name,
                content,
                is_error,
            } => {
                format!(
                    "{name} {} {}",
                    if *is_error { "failed" } else { "ok" },
                    content.lines().next().unwrap_or("")
                )
            }
        })
        .collect();
    assert_eq!(
        summary,
        [
            "request 1",
            "reply 1",
            "call Read",
            "Read ok 1\tfn main() {",
            "request 2",
            "reply 2",
            "call Edit",
            "Edit ok The file /p/src/main.rs has been updated (1 replacement). Snippet of the result:",
            "request 3",
            "reply 3",
            "call Grep",
            "Grep ok src/main.rs:2:    println!(\"hello\");",
            "request 4",
            "text Fixed the typo in src/main.rs.",
            "reply 4",
        ]
    );
    let sent = model.sent.borrow();
    assert_eq!(sent.len(), 4);
    assert!(
        sent.iter()
            .all(|(url, _)| url == "https://models.example/v1/chat/completions")
    );
    let first = &sent[0].1;
    assert_eq!(
        (first["model"].as_str(), first["stream"].as_bool()),
        (Some("m"), Some(true))
    );
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["Read", "Write", "Edit", "Glob", "Grep", "Ls"]);
    // The last request carries the whole turn: system, user, then each call
    // and its result.
    let roles: Vec<&str> = sent[3].1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        [
            "system",
            "user",
            "assistant",
            "tool",
            "assistant",
            "tool",
            "assistant",
            "tool"
        ]
    );
    assert_eq!(sent[3].1["messages"][3]["tool_call_id"], "c1");
}

#[test]
fn a_model_that_never_answers_stops_at_the_step_limit() {
    let fs = MemFs::new();
    let model = Scripted {
        replies: RefCell::new(
            (0..3)
                .map(|i| call(&format!("c{i}"), "Ls", json!({})))
                .collect(),
        ),
        sent: RefCell::new(vec![]),
    };
    let mut agent = Agent::new(
        ModelConfig {
            base_url: "http://x/v1".into(),
            model: "m".into(),
            api_key: Some("k".into()),
        },
        "sys",
    );
    agent.max_steps = 3;
    let mut workspace = Workspace::new(&fs, "/p");
    let err =
        futures::executor::block_on(agent.run_turn("go", &model, &mut workspace, &mut |_| {}))
            .unwrap_err();
    assert!(
        err.to_string().contains("stopped after 3 model requests"),
        "{err}"
    );
}

/// A model without native tool calls (a small local one) reads the tools in
/// its system prompt and answers with `<tool_call>` blocks, near-miss
/// argument names included.
#[test]
fn text_tool_mode_reads_tagged_calls_and_repairs_their_arguments() {
    let fs = MemFs::new();
    fs.insert("/p/notes.txt", "remember the milk\n");
    let model = Scripted {
        replies: RefCell::new(vec![
            text(
                "Let me look.\n<tool_call>\n{\"name\": \"Read\", \"arguments\": {\"path\": \"notes.txt\"}}\n</tool_call>",
            ),
            text("It says to remember the milk."),
        ]),
        sent: RefCell::new(vec![]),
    };
    let mut agent = Agent::new(
        ModelConfig {
            base_url: "http://local/v1".into(),
            model: "small".into(),
            api_key: Some("k".into()),
        },
        "You are a coding agent.",
    );
    agent.text_tools = true;
    let mut workspace = Workspace::new(&fs, "/p");
    let mut results = Vec::new();
    let answer = futures::executor::block_on(agent.run_turn(
        "what do my notes say?",
        &model,
        &mut workspace,
        &mut |e| {
            if let Event::ToolResult { content, .. } = e {
                results.push(content)
            }
        },
    ))
    .unwrap();
    assert_eq!(answer, "It says to remember the milk.");
    assert_eq!(results, ["1\tremember the milk\n"]);
    let sent = model.sent.borrow();
    // No native tools: they are in the system prompt instead, and the
    // reply stops before any result the model would invent.
    assert!(sent[0].1.get("tools").is_none());
    assert_eq!(sent[0].1["stop"], json!(["<tool_response"]));
    let system = sent[0].1["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("<tool_call>") && system.contains("\"name\":\"Read\""),
        "{system}"
    );
    // The call and its result go back as tagged text, not tool messages.
    let msgs = sent[1].1["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert!(msgs[2]["content"].as_str().unwrap().contains("<tool_call>"));
    assert!(
        msgs[3]["content"]
            .as_str()
            .unwrap()
            .starts_with("<tool_response name=\"Read\">")
    );
}

#[test]
fn a_streamed_piece_of_the_stop_sequence_is_dropped() {
    assert_eq!(without_stop_fragment("{\"a\":1}\n\n<tool"), "{\"a\":1}");
    assert_eq!(without_stop_fragment("x <tool_response"), "x");
    assert_eq!(without_stop_fragment("a < b"), "a < b");
    assert_eq!(without_stop_fragment("<div>"), "<div>");
}
