use super::*;

fn req() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        tools: vec![],
        think: None,
        effort: None,
        temperature: None,
        max_tokens: None,
        json: false,
    }
}

/// Feeds `body` to a decoder in `size`-byte chunks, returning the response
/// and the deltas it streamed.
fn decode(body: &str, size: usize) -> (anyhow::Result<ChatResponse>, Vec<Delta>) {
    let mut d = OpenaiDecoder::new(&req());
    let mut deltas = Vec::new();
    for chunk in body.as_bytes().chunks(size) {
        match d.feed(chunk, &mut |x| deltas.push(x)) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => return (Err(e), deltas),
        }
    }
    (d.finish(), deltas)
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

#[test]
fn decodes_the_same_whatever_the_chunk_boundaries() {
    let body = sse(&[
        json!({"choices":[{"delta":{"role":"assistant","content":"Hel"}}]}),
        json!({"choices":[{"delta":{"content":"lo é"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4}}}),
    ]) + "data: [DONE]\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"after\"}}]}\n\n";
    for size in [1, 2, 7, 64, body.len()] {
        let (r, deltas) = decode(&body, size);
        let r = r.unwrap();
        assert_eq!(r.text, "Hello é", "chunks of {size}");
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            (
                r.usage.prompt_tokens,
                r.usage.completion_tokens,
                r.usage.cached_tokens
            ),
            (7, 3, 4)
        );
        let streamed: String = deltas
            .iter()
            .map(|d| match d {
                Delta::Text(t) => t.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(streamed, "Hello é");
    }
}

#[test]
fn assembles_interleaved_tool_calls_in_wire_order() {
    let body = sse(&[
        json!({"choices":[{"delta":{"tool_calls":[{"index":4000000000u64,"id":"b","function":{"name":"Grep","arguments":"{\"pat"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"Read","arguments":""}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":4000000000u64,"function":{"arguments":"tern\":\"x\"}"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"file_path\":\"a.rs\"}"}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ]);
    let r = decode(&body, 5).0.unwrap();
    let calls: Vec<_> = r
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.clone()))
        .collect();
    assert_eq!(
        calls,
        vec![
            ("a", "Read", json!({"file_path": "a.rs"})),
            ("b", "Grep", json!({"pattern": "x"})),
        ]
    );
}

#[test]
fn calls_without_an_index_split_on_a_new_id() {
    let body = sse(&[
        json!({"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"Read","arguments":"{}"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"id":"b","function":{"name":"Glob","arguments":"{\"pattern\":"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"\"*.rs\"}"}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ]);
    let r = decode(&body, 64).0.unwrap();
    assert_eq!(r.tool_calls.len(), 2);
    assert_eq!(r.tool_calls[1].arguments, json!({"pattern": "*.rs"}));
}

#[test]
fn a_stream_that_ends_mid_call_fails_rather_than_repairing_it() {
    let call = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"Write","arguments":"{\"content\":\"half"}}]}}]});
    let e = decode(&sse(std::slice::from_ref(&call)), 16).0.unwrap_err();
    assert!(e.to_string().contains("connection closed"), "{e}");
    // The same call is fine once the server says it finished.
    let done = sse(&[call]) + "data: [DONE]\n\n";
    assert_eq!(decode(&done, 16).0.unwrap().tool_calls.len(), 1);
}

#[test]
fn an_ollama_body_or_an_error_fails_with_the_reason() {
    let ollama = "{\"model\":\"m\",\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"done\":false}\n";
    let e = decode(ollama, 64).0.unwrap_err();
    assert!(e.to_string().contains("Ollama server"), "{e}");
    let e = decode(&sse(&[json!({"error":{"message":"overloaded"}})]), 64)
        .0
        .unwrap_err();
    assert!(e.to_string().starts_with("API error"), "{e}");
}

#[test]
fn reasoning_content_is_kept_for_the_next_request() {
    let body = sse(&[
        json!({"choices":[{"delta":{"reasoning_content":"think"}}]}),
        json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
    ]);
    let (r, deltas) = decode(&body, 8);
    let r = r.unwrap();
    assert!(matches!(&deltas[0], Delta::Reasoning(t) if t == "think"));
    let items = r.provider_items.unwrap();
    assert_eq!(
        (items.kind, items.model.as_str()),
        (ProviderKind::Openai, "m")
    );
    assert_eq!(items.items, vec![json!({"reasoning_content": "think"})]);
    // Under the other name the server doesn't want it back.
    let body = sse(&[
        json!({"choices":[{"delta":{"reasoning":"think","content":"ok"},"finish_reason":"stop"}]}),
    ]);
    let r = decode(&body, 8).0.unwrap();
    assert_eq!(r.reasoning, "think");
    assert!(r.provider_items.is_none());
}

#[test]
fn a_refusal_stays_the_finish_reason() {
    let body = sse(&[
        json!({"choices":[{"delta":{"refusal":"I can't help with that."}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
    ]);
    let r = decode(&body, 64).0.unwrap();
    assert_eq!(r.text, "I can't help with that.");
    assert_eq!(r.finish_reason.as_deref(), Some("refusal"));
}

#[test]
fn looping_reasoning_stops_the_stream() {
    let para = "I should check the file again before answering the question. ";
    let events: Vec<Value> = (0..400)
        .map(|_| json!({"choices":[{"delta":{"reasoning_content": para}}]}))
        .collect();
    let e = decode(&sse(&events), 256).0.unwrap_err();
    assert!(e.downcast_ref::<ReasoningLoop>().is_some(), "{e}");
}

#[test]
fn fallback_triggers() {
    assert!(warrants_fallback(
        "API error 529: {\"type\":\"overloaded_error\"}"
    ));
    assert!(warrants_fallback("API error 503 Service Unavailable: busy"));
    assert!(warrants_fallback(
        "Ollama error 404 Not Found: {\"error\":\"model 'x' not found\"}"
    ));
    assert!(warrants_fallback(
        "Ollama error: model 'x' not found, try pulling it first"
    ));
    assert!(warrants_fallback(
        "connecting to http://localhost:11434: tcp connect error: Connection refused (os error 61)"
    ));
    assert!(warrants_fallback(
        "chanto cloud: upstream failed (HTTP 502)"
    ));
    assert!(!warrants_fallback("API error 429: rate limited"));
    assert!(!warrants_fallback("API error 401: invalid api key"));
    assert!(!warrants_fallback(
        "API error 400 Bad Request: model context length exceeded"
    ));
    assert!(!warrants_fallback("API error 413: request too large"));
    assert!(!warrants_fallback("error decoding response body"));
    assert_eq!(error_status("API error 404 Not Found: x"), Some(404));
}

#[test]
fn extracts_tagged_tool_calls() {
    let known = vec!["Read".to_string()];
    let (rest, calls) = extract_text_tool_calls(
        "Let me look.\n<tool_call>\n{\"name\": \"Read\", \"arguments\": {\"file_path\": \"a.rs\"}}\n</tool_call>",
        &known,
    );
    assert_eq!(rest, "Let me look.");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments["file_path"], "a.rs");
}

#[test]
fn ignores_unknown_tools() {
    let known = vec!["Read".to_string()];
    let (_, calls) = extract_text_tool_calls("{\"name\": \"Nope\", \"arguments\": {}}", &known);
    assert!(calls.is_empty());
}

#[test]
fn a_mention_of_the_tag_is_not_a_call() {
    // Writing *about* the protocol must not hide the rest of the answer.
    let prose = "the parser does full.split(\"<tool_call>\") and stops";
    assert_eq!(
        tool_call_start(prose),
        prose.len(),
        "inline mention is prose"
    );
    assert_eq!(
        tool_call_start("see <tool_call>{} in the docs"),
        4,
        "a block still counts"
    );
    let plain = "all prose, nothing to see";
    assert_eq!(tool_call_start(plain), plain.len());
}

#[test]
fn label_probs_count_only_whole_labels() {
    let top = |v: &[(&str, f64)]| -> Vec<(String, f64)> {
        v.iter().map(|(t, p)| (t.to_string(), p.ln())).collect()
    };
    let p = label_probs_from(
        &top(&[("allow", 0.6), (" BLOCK", 0.2), ("I", 0.2)]),
        &["ALLOW", "BLOCK"],
    )
    .unwrap();
    assert!(
        (p.mass[0] - 0.6).abs() < 1e-9 && (p.mass[1] - 0.2).abs() < 1e-9 && p.top == Some(0),
        "{p:?}"
    );
    // A prefix of a label could begin prose ("All of this…"): it counts
    // for no label, however likely.
    for piece in ["Al", "All", "Allo", "ALLOWED"] {
        let p =
            label_probs_from(&top(&[(piece, 0.95), ("ALLOW", 0.04)]), &["ALLOW", "BLOCK"]).unwrap();
        assert!(
            p.top.is_none() && (p.mass[0] - 0.04).abs() < 1e-9,
            "{piece}: {p:?}"
        );
    }
    // Absolute, not a share of the label mass: ALLOW at 0.7% stays 0.7%
    // when most of the probability went to prose.
    let p = label_probs_from(
        &top(&[("Sure", 0.95), ("ALLOW", 0.007)]),
        &["ALLOW", "BLOCK"],
    )
    .unwrap();
    assert!((p.mass[0] - 0.007).abs() < 1e-9 && p.top.is_none(), "{p:?}");
    // A label split into one-letter pieces counts for neither label.
    let p = label_probs_from(&top(&[("ALLOW", 0.5), ("B", 0.45)]), &["ALLOW", "BLOCK"]).unwrap();
    assert!((p.mass[0] - 0.5).abs() < 1e-9 && p.mass[1] == 0.0, "{p:?}");
    // Nothing that reads as a label: no answer, not a guess.
    assert!(label_probs_from(&top(&[("Sure", 0.9), ("I", 0.1)]), &["ALLOW", "BLOCK"]).is_none());
    let entry = json!({"token": "BLOCK", "logprob": -0.1, "top_logprobs": [{"token": "BLOCK", "logprob": -0.1}, {"token": "ALLOW", "logprob": -2.4}]});
    assert_eq!(top_logprobs(&entry).len(), 2);
    assert_eq!(
        top_logprobs(&json!({"token": "ALLOW", "logprob": -0.01})).len(),
        1
    );
}

#[test]
fn reasoning_watch_spots_paragraph_loops_only() {
    let para = "The Verse function builds each verse by first adding the introduction line, then the chain, and returns the joined string. ";
    let mut w = ReasoningWatch::default();
    let mut text = String::new();
    let mut fired = None;
    while text.len() < 40_000 && fired.is_none() {
        text.push_str(para);
        if w.looping(&text) {
            fired = Some(text.len());
        }
    }
    assert!(fired.is_some_and(|n| n < 12_000), "{fired:?}");
    // A long cycle (about 350 words) is a loop too.
    let cycle: String = (0..350).map(|i| format!("w{i} ")).collect();
    let mut w = ReasoningWatch::default();
    let mut text = String::new();
    let mut fired = None;
    while text.len() < 80_000 && fired.is_none() {
        text.push_str(&cycle[..cycle.len().min(400)]);
        text.push_str(&cycle[cycle.len().min(400)..]);
        if w.looping(&text) {
            fired = Some(text.len());
        }
    }
    assert!(fired.is_some(), "long cycle not caught");
    // Varied reasoning of the same length never trips it.
    let mut w = ReasoningWatch::default();
    let mut text = String::new();
    for i in 0..4000 {
        text.push_str(&format!(
            "step {i} checks value {} against {} ",
            i * 7 % 13,
            i * 3
        ));
        assert!(!w.looping(&text), "false positive at {}", text.len());
    }
}

#[test]
fn line_buf_splits_across_reads_and_scans_long_lines_once() {
    let mut l = LineBuf::default();
    l.push(b"data: a\ndata: b");
    assert_eq!(l.next_line(), Some(&b"data: a\n"[..]));
    assert_eq!(l.next_line(), None);
    l.push(b"c\n\nx");
    assert_eq!(l.next_line(), Some(&b"data: bc\n"[..]));
    assert_eq!(l.next_line(), Some(&b"\n"[..]));
    assert_eq!(l.next_line(), None);
    // A 20 MB line in 16 KB reads: linear, so this is instant. Node's WASI,
    // which runs the WebAssembly tests, exits without a word once memory
    // grows to about 40 MB, so there it is 2 MB.
    let reads = if cfg!(target_family = "wasm") {
        128
    } else {
        1280
    };
    let mut l = LineBuf::default();
    let t = std::time::Instant::now();
    for _ in 0..reads {
        l.push(&[b'y'; 16384]);
        assert_eq!(l.next_line(), None);
    }
    l.push(b"\n");
    assert_eq!(l.next_line().map(|v| v.len()), Some(reads * 16384 + 1));
    assert!(
        t.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn repairs_truncated_json() {
    let v: Value = serde_json::from_str(&repair_json("{\"a\": [1, 2, {\"b\": \"x")).unwrap();
    assert_eq!(v["a"][2]["b"], "x");
}

#[test]
fn several_bare_calls_count_only_when_they_are_the_whole_reply() {
    let known = vec!["Read".to_string(), "Edit".to_string()];
    let reply = "{\"name\":\"Read\",\"arguments\":{\"file_path\":\"a.py\"}}\n{\"name\":\"Edit\",\"arguments\":{\"file_path\":\"a.py\",\"old_string\":\"{x}\",\"new_string\":\"y\"}}";
    let (rest, calls) = extract_text_tool_calls(reply, &known);
    assert_eq!(rest, "");
    let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Read", "Edit"]);
    assert_eq!(
        calls[1].arguments["old_string"], "{x}",
        "braces inside strings"
    );
    // Fenced, too.
    let (_, calls) = extract_text_tool_calls(&format!("```json\n{reply}\n```"), &known);
    assert_eq!(calls.len(), 2);
    // Prose around them, or an object that isn't a known call: nothing runs.
    for text in [
        format!("Here is how I would do it:\n{reply}"),
        format!("{reply}\n{{\"name\":\"Bash\",\"arguments\":{{}}}}"),
        "{\"note\": \"not a call\"}".to_string(),
    ] {
        let (_, calls) = extract_text_tool_calls(&text, &known);
        assert!(calls.is_empty(), "{text}");
    }
}
