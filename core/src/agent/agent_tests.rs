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
            Event::Text(t) => format!("text {t}"),
            Event::Reasoning(t) => format!("reasoning {t}"),
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
            "call Read",
            "Read ok 1\tfn main() {",
            "call Edit",
            "Edit ok The file /p/src/main.rs has been updated (1 replacement). Snippet of the result:",
            "call Grep",
            "Grep ok src/main.rs:2:    println!(\"hello\");",
            "text Fixed the typo in src/main.rs.",
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
