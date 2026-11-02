//! The Browser tool against a fake Chrome on loopback: the `/json` endpoints
//! and a DevTools websocket that scripts page answers and records every call.
//! No real browser runs; `chrome_path` points at a path that can't start, or a
//! shim, so a failed attach can't launch one.

use super::*;
use crate::agent::tests::{make_env_with, tmpdir};
use crate::permissions::Mode;
use axum::Json;
use axum::extract::{Path, State};
use axum::routing::{get, put};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Fake {
    port: u16,
    ws_port: u16,
    calls: Arc<Mutex<Vec<Value>>>,
    tabs: Arc<Mutex<Vec<Value>>>,
    badge: Arc<Mutex<Option<String>>>,
    /// Close the page socket on the next call of this method, unanswered.
    drop_on: Arc<Mutex<Option<String>>>,
}

fn answer(req: &Value) -> Value {
    let params = &req["params"];
    match req["method"].as_str().unwrap_or("") {
        "Runtime.evaluate" => {
            let e = params["expression"].as_str().unwrap_or("");
            let value = |v: Value| json!({"result": {"result": {"value": v}}});
            if e.contains("TEXT_THROW") {
                json!({"result": {"exceptionDetails": {"text": "plain text"}}})
            } else if e.contains("THROW") {
                json!({"result": {"exceptionDetails": {"exception": {"description": "Error: thrown by page"}}}})
            } else if e == "document.readyState" {
                value(json!("complete"))
            } else if e == "navigator.userAgent" {
                value(json!("Mozilla HeadlessChrome/150.0 Safari"))
            } else if e == "document.documentElement.outerHTML" {
                value(json!(
                    "<html><body><h1>Big Title</h1><p>body text</p></body></html>"
                ))
            } else if e.contains("Interactive elements") {
                value(json!(
                    "URL: http://page/\nTitle: Fake\nScroll: 0/0\n\nInteractive elements:\n[1] button \"Go\""
                ))
            } else if e.contains("getBoundingClientRect") && e.contains("#missing") {
                value(Value::Null)
            } else if e.contains("getBoundingClientRect") {
                value(json!([11.0, 22.0]))
            } else if e.contains("selected ") {
                value(json!("selected Blue"))
            } else if e.contains("no matching interactive") {
                value(json!("[1] button \"go\""))
            } else if e == "null" {
                value(Value::Null)
            } else if e == "obj" {
                value(json!({"a": 1}))
            } else {
                value(json!("evaluated"))
            }
        }
        "Page.navigate" if params["url"].as_str().unwrap_or("").contains("unreachable") => {
            json!({"result": {"errorText": "net::ERR_NAME_NOT_RESOLVED"}})
        }
        "Page.captureScreenshot" => json!({"result": {"data": "SkVH"}}),
        "Fail.me" => json!({"error": {"message": "no such method"}}),
        "Fail.opaque" => json!({"error": {"code": 7}}),
        _ => json!({"result": {}}),
    }
}

/// Everything Chrome pushes without being asked, sent once `Page.enable` shows
/// the client is listening.
fn page_events() -> Vec<Value> {
    let event = |method: &str, params: Value| json!({"method": method, "params": params});
    vec![
        event(
            "Runtime.consoleAPICalled",
            json!({"type": "warning", "args": [
                {"value": "plain"}, {"value": 42}, {"description": "Object"}
            ]}),
        ),
        event(
            "Runtime.exceptionThrown",
            json!({"exceptionDetails": {"exception": {"description": "TypeError: x"}}}),
        ),
        event(
            "Runtime.exceptionThrown",
            json!({"exceptionDetails": {"text": "Uncaught"}}),
        ),
        event(
            "Log.entryAdded",
            json!({"entry": {"level": "error", "text": "blocked", "url": "http://a/"}}),
        ),
        event(
            "Network.responseReceived",
            json!({"type": "Document", "response": {"status": 404, "url": "http://a/x", "mimeType": "text/html"}}),
        ),
        event(
            "Network.loadingFailed",
            json!({"type": "Script", "errorText": "net::ERR_FAILED"}),
        ),
        event(
            "Page.javascriptDialogOpening",
            json!({"type": "alert", "message": "hello"}),
        ),
        event(
            "Page.javascriptDialogOpening",
            json!({"type": "confirm", "message": "sure?"}),
        ),
        event("Some.unknownEvent", json!({})),
    ]
}

impl Fake {
    async fn start() -> Fake {
        let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_port = ws.local_addr().unwrap().port();
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = http.local_addr().unwrap().port();
        let tab = |id: &str, title: &str| {
            json!({"id": id, "type": "page", "title": title, "url": format!("http://{id}/"),
                   "webSocketDebuggerUrl": format!("ws://127.0.0.1:{ws_port}/page/{id}")})
        };
        let fake = Fake {
            port,
            ws_port,
            calls: Default::default(),
            tabs: Arc::new(Mutex::new(vec![
                tab("one", "First"),
                json!({"id": "bg", "type": "service_worker"}),
                tab("two", "Second"),
            ])),
            badge: Default::default(),
            drop_on: Default::default(),
        };
        let f2 = fake.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = ws.accept().await {
                let fake = f2.clone();
                tokio::spawn(async move {
                    let Ok(ws) = tokio_tungstenite::accept_async(sock).await else {
                        return;
                    };
                    let (mut sink, mut stream) = ws.split();
                    while let Some(Ok(WsMsg::Text(text))) = stream.next().await {
                        // Junk and binary frames must not disturb the client.
                        let _ = sink.send(WsMsg::text("not json")).await;
                        let _ = sink.send(WsMsg::binary(vec![1u8])).await;
                        let req: Value = serde_json::from_str(&text).unwrap();
                        let method = req["method"].as_str().unwrap_or("").to_string();
                        if method == "Browser.setDockTile" {
                            *fake.badge.lock().unwrap() =
                                req["params"]["badgeLabel"].as_str().map(str::to_string);
                        }
                        fake.calls.lock().unwrap().push(req.clone());
                        if fake.drop_on.lock().unwrap().as_deref() == Some(&method) {
                            let _ = sink.send(WsMsg::Close(None)).await;
                            return;
                        }
                        if method == "Page.enable" {
                            for e in page_events() {
                                let _ = sink.send(WsMsg::text(e.to_string())).await;
                            }
                        }
                        let mut reply = answer(&req);
                        reply["id"] = req["id"].clone();
                        if sink.send(WsMsg::text(reply.to_string())).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let new_tab = move |State(f): State<Fake>| async move {
            let t = json!({"id": "new", "type": "page", "title": "New", "url": "about:blank",
                "webSocketDebuggerUrl": format!("ws://127.0.0.1:{}/page/new", f.ws_port)});
            f.tabs.lock().unwrap().push(t.clone());
            Json(t)
        };
        let app = axum::Router::new()
            .route(
                "/json/version",
                get(|State(f): State<Fake>| async move {
                    Json(json!({"webSocketDebuggerUrl": format!("ws://127.0.0.1:{}/browser", f.ws_port)}))
                }),
            )
            .route(
                "/json/list",
                get(|State(f): State<Fake>| async move {
                    Json(Value::Array(f.tabs.lock().unwrap().clone()))
                }),
            )
            .route("/json/new", put(new_tab))
            .route(
                "/json/activate/{id}",
                get(|State(f): State<Fake>, Path(id): Path<String>| async move {
                    f.calls.lock().unwrap().push(json!({"method": "activate", "id": id}));
                    "ok"
                }),
            )
            .route(
                "/json/close/{id}",
                get(|State(f): State<Fake>, Path(id): Path<String>| async move {
                    f.tabs.lock().unwrap().retain(|t| t["id"] != id.as_str());
                    "ok"
                }),
            )
            .with_state(fake.clone());
        tokio::spawn(async move { axum::serve(http, app).await });
        fake
    }

    fn config(&self, c: &mut crate::config::Config) {
        c.browser.port = Some(self.port);
        c.browser.chrome_path = Some("/nonexistent/chrome".into());
    }

    fn ctx(&self, vision: bool) -> Ctx {
        let dir = tmpdir("browser");
        let env = make_env_with("http://127.0.0.1:9", &dir, Mode::Bypass, 32768, |c| {
            self.config(c)
        });
        env.set_vision(vision);
        Ctx {
            env,
            cancel: CancellationToken::new(),
            depth: 0,
            call_id: None,
            cwd_override: None,
            reads: None,
        }
    }

    fn calls(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["method"] == method)
            .cloned()
            .collect()
    }

    fn evaluated(&self) -> Vec<String> {
        self.calls("Runtime.evaluate")
            .iter()
            .map(|c| c["params"]["expression"].as_str().unwrap().to_string())
            .collect()
    }
}

async fn run(ctx: &Ctx, args: Value) -> ToolOutput {
    Browser.run(args, ctx).await.unwrap()
}

#[tokio::test]
async fn starting_a_session_prepares_the_page_and_badges_the_browser() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let out = run(&ctx, json!({"action": "read_page"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("[1] button \"Go\""));
    assert_eq!(
        out.display.as_deref(),
        Some("URL: http://page/ · Title: Fake")
    );
    for method in [
        "Page.enable",
        "Runtime.enable",
        "Log.enable",
        "Network.enable",
        "Page.bringToFront",
    ] {
        assert_eq!(fake.calls(method).len(), 1, "{method}");
    }
    // A headless build presents the UA it would have with a window.
    let ua = fake.calls("Emulation.setUserAgentOverride");
    assert_eq!(ua[0]["params"]["userAgent"], "Mozilla Chrome/150.0 Safari");
    assert_eq!(fake.badge.lock().unwrap().as_deref(), Some("AI"));
    let guard = ctx.env.browser.lock().await;
    let session = guard.as_ref().unwrap();
    // The first *page*, skipping the service worker target.
    assert_eq!(session.target_id, "one");
    assert_eq!(session.port, fake.port);
}

#[tokio::test]
async fn an_empty_badge_is_not_sent() {
    let fake = Fake::start().await;
    let mut cfg = crate::config::BrowserConfig {
        port: Some(fake.port),
        chrome_path: Some("/nonexistent/chrome".into()),
        badge: Some(String::new()),
        ..Default::default()
    };
    BrowserSession::start(&cfg).await.unwrap();
    assert_eq!(*fake.badge.lock().unwrap(), None);
    cfg.badge = Some("Bot".into());
    BrowserSession::start(&cfg).await.unwrap();
    assert_eq!(fake.badge.lock().unwrap().as_deref(), Some("Bot"));
}

#[tokio::test]
async fn navigate_adds_a_scheme_and_reports_failures() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let out = run(&ctx, json!({"action": "navigate", "url": "example.test"})).await;
    assert!(
        out.content.contains("Interactive elements"),
        "{}",
        out.content
    );
    let out = run(
        &ctx,
        json!({"action": "navigate", "url": "http://kept.test"}),
    )
    .await;
    assert!(!out.is_error);
    run(&ctx, json!({"action": "navigate", "url": "about:blank"})).await;
    let urls: Vec<_> = fake
        .calls("Page.navigate")
        .iter()
        .map(|c| c["params"]["url"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        urls,
        ["https://example.test", "http://kept.test", "about:blank"]
    );

    let out = run(
        &ctx,
        json!({"action": "navigate", "url": "unreachable.test"}),
    )
    .await;
    assert!(out.is_error);
    assert_eq!(out.content, "Navigation failed: net::ERR_NAME_NOT_RESOLVED");
    assert!(
        Browser
            .run(json!({"action": "navigate"}), &ctx)
            .await
            .is_err()
    );
    assert!(Browser.run(json!({}), &ctx).await.is_err());
}

#[tokio::test]
async fn page_text_is_marked_untrusted_and_screenshots_need_vision() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let out = run(&ctx, json!({"action": "get_text"})).await;
    assert!(out.content.starts_with("<page_content>"));
    assert!(out.content.contains("Big Title") && out.content.contains("untrusted data"));

    let out = run(&ctx, json!({"action": "screenshot"})).await;
    assert_eq!(out.images, ["SkVH"]);

    let blind = fake.ctx(false);
    let out = run(&blind, json!({"action": "screenshot"})).await;
    assert!(out.images.is_empty());
    assert!(out.content.starts_with("[current model has no vision"));
    assert!(out.content.contains("Interactive elements"));
}

#[tokio::test]
async fn clicks_resolve_their_target_and_press_then_release() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let out = run(&ctx, json!({"action": "click", "ref": 3})).await;
    assert!(
        out.content.starts_with("Clicked at (11, 22)."),
        "{}",
        out.content
    );
    assert!(
        fake.evaluated()
            .iter()
            .any(|e| e.contains("[data-hx-ref=\\\"3\\\"]"))
    );
    let events = fake.calls("Input.dispatchMouseEvent");
    let kinds: Vec<_> = events.iter().map(|e| e["params"]["type"].clone()).collect();
    assert_eq!(kinds, ["mouseMoved", "mousePressed", "mouseReleased"]);
    assert_eq!(events[1]["params"]["clickCount"], 1);

    fake.calls.lock().unwrap().clear();
    run(
        &ctx,
        json!({"action": "click", "x": 5, "y": 6, "double": true}),
    )
    .await;
    let events = fake.calls("Input.dispatchMouseEvent");
    assert_eq!(events[1]["params"]["clickCount"], 2);
    assert_eq!(events[1]["params"]["x"], 5.0);
    assert!(
        fake.evaluated()
            .iter()
            .all(|e| !e.contains("scrollIntoView"))
    );

    // A ref model output may wrap in brackets or send as a string.
    let out = run(&ctx, json!({"action": "click", "ref": "[7]"})).await;
    assert!(!out.is_error);
    assert!(
        fake.evaluated()
            .iter()
            .any(|e| e.contains("[data-hx-ref=\\\"7\\\"]"))
    );

    let err = Browser
        .run(json!({"action": "click", "selector": "#missing"}), &ctx)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("element not found: #missing"),
        "{err}"
    );
    let err = Browser
        .run(json!({"action": "click"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("provide `ref`"), "{err}");
}

#[tokio::test]
async fn typing_focuses_inserts_and_can_submit() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let out = run(&ctx, json!({"action": "type", "text": "héllo"})).await;
    assert_eq!(out.content, "Typed 5 characters.");
    assert!(
        fake.calls("Input.dispatchMouseEvent").is_empty(),
        "nothing to focus"
    );
    assert_eq!(fake.calls("Input.insertText")[0]["params"]["text"], "héllo");

    let out = run(
        &ctx,
        json!({"action": "type", "text": "q", "selector": "#in", "submit": true}),
    )
    .await;
    assert!(out.content.starts_with("Typed text and pressed Enter."));
    assert!(fake.evaluated().iter().any(|e| e.contains("el.focus()")));
    assert_eq!(fake.calls("Input.dispatchMouseEvent").len(), 3);
    let keys = fake.calls("Input.dispatchKeyEvent");
    assert_eq!(keys[0]["params"]["key"], "Enter");
    assert_eq!(keys[0]["params"]["text"], "\r");
    assert_eq!(keys[1]["params"]["type"], "keyUp");
    assert!(Browser.run(json!({"action": "type"}), &ctx).await.is_err());
}

#[tokio::test]
async fn key_combos_set_modifiers_and_suppress_text() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let press = |combo: &'static str| run(&ctx, json!({"action": "key", "key": combo}));
    assert_eq!(press("ctrl+a").await.content, "Pressed ctrl+a.");
    let down = fake.calls("Input.dispatchKeyEvent")[0]["params"].clone();
    assert_eq!(down["modifiers"], 2);
    assert_eq!(down["code"], "KeyA");
    assert_eq!(down["windowsVirtualKeyCode"], 'A' as u32);
    assert!(
        down.get("text").is_none(),
        "ctrl+a must not type an a: {down}"
    );

    fake.calls.lock().unwrap().clear();
    press("shift+alt+Tab").await;
    let down = fake.calls("Input.dispatchKeyEvent")[0]["params"].clone();
    assert_eq!(down["modifiers"], 9);
    assert_eq!(down["type"], "rawKeyDown");
    assert_eq!(down["windowsVirtualKeyCode"], 9);

    fake.calls.lock().unwrap().clear();
    press("shift+5").await;
    let down = fake.calls("Input.dispatchKeyEvent")[0]["params"].clone();
    assert_eq!(down["code"], "Digit5");
    assert_eq!(down["text"], "5", "shift still types");

    fake.calls.lock().unwrap().clear();
    press("cmd+Esc").await;
    let down = fake.calls("Input.dispatchKeyEvent")[0]["params"].clone();
    assert_eq!(down["key"], "Escape");
    assert_eq!(down["modifiers"], 4);

    assert_eq!(key_info("space").0, " ");
    assert_eq!(key_info("?").1, "", "punctuation has no key code");
}

#[tokio::test]
async fn page_actions_quote_user_text_into_their_scripts() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let out = run(
        &ctx,
        json!({"action": "select", "selector": "#c", "value": "Bl'ue\"x"}),
    )
    .await;
    assert_eq!(out.content, "selected Blue");
    let script = fake
        .evaluated()
        .into_iter()
        .find(|e| e.contains("selected "))
        .unwrap();
    assert!(
        script.contains(r#""Bl'ue\"x""#),
        "value is a JSON string: {script}"
    );
    assert!(script.contains(r##"querySelector("#c")"##));
    assert!(
        Browser
            .run(json!({"action": "select", "selector": "#c"}), &ctx)
            .await
            .is_err()
    );
    assert!(
        Browser
            .run(json!({"action": "select", "value": "x"}), &ctx)
            .await
            .is_err()
    );

    let out = run(&ctx, json!({"action": "find", "text": "G\"o"})).await;
    assert_eq!(out.content, "[1] button \"go\"");
    assert!(
        fake.evaluated()
            .iter()
            .any(|e| e.contains(r#""G\"o".toLowerCase()"#))
    );

    fake.calls.lock().unwrap().clear();
    run(
        &ctx,
        json!({"action": "scroll", "direction": "up", "amount": 50}),
    )
    .await;
    assert!(
        fake.evaluated()
            .contains(&"window.scrollBy(0, -50)".to_string())
    );
    run(&ctx, json!({"action": "scroll"})).await;
    assert!(
        fake.evaluated()
            .contains(&"window.scrollBy(0, 700)".to_string())
    );

    for (action, js) in [
        ("back", "history.back()"),
        ("forward", "history.forward()"),
        ("reload", "location.reload()"),
    ] {
        let out = run(&ctx, json!({"action": action})).await;
        assert!(out.content.contains("Interactive"));
        assert!(fake.evaluated().contains(&js.to_string()), "{action}");
    }
}

#[tokio::test]
async fn evaluate_formats_results_and_surfaces_page_errors() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let eval = |s: &'static str| run(&ctx, json!({"action": "evaluate", "script": s}));
    assert_eq!(eval("x").await.content, "evaluated");
    assert_eq!(eval("null").await.content, "undefined");
    assert_eq!(eval("obj").await.content, "{\n  \"a\": 1\n}");
    let err = Browser
        .run(json!({"action": "evaluate", "script": "THROW"}), &ctx)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "JavaScript error: Error: thrown by page");
    let err = Browser
        .run(json!({"action": "evaluate", "script": "TEXT_THROW"}), &ctx)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "JavaScript error: plain text");
}

#[tokio::test]
async fn console_and_network_keep_what_the_page_reported() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let out = run(&ctx, json!({"action": "console"})).await;
    let lines: Vec<_> = out.content.lines().collect();
    assert_eq!(
        lines,
        [
            "[warning] plain 42 Object",
            "[exception] TypeError: x",
            "[exception] Uncaught",
            "[error] blocked http://a/",
            "[dialog] alert: hello (accepted)",
            "[dialog] confirm: sure? (dismissed automatically)",
        ]
    );
    // An alert is accepted and a confirm refused, or the page would block.
    let answers: Vec<_> = fake
        .calls("Page.handleJavaScriptDialog")
        .iter()
        .map(|c| c["params"]["accept"].clone())
        .collect();
    assert_eq!(answers, [json!(true), json!(false)]);

    let out = run(&ctx, json!({"action": "console", "text": "BLOCKED"})).await;
    assert_eq!(out.content, "[error] blocked http://a/");
    let out = run(&ctx, json!({"action": "console", "text": "absent"})).await;
    assert_eq!(out.content, "(no console entries)");
    let out = run(&ctx, json!({"action": "network"})).await;
    assert_eq!(
        out.content,
        "Document 404 http://a/x (text/html)\nFAILED Script net::ERR_FAILED"
    );
}

#[tokio::test]
async fn tabs_can_be_listed_opened_switched_and_closed() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let out = run(&ctx, json!({"action": "tabs"})).await;
    assert_eq!(
        out.content,
        "0: First — http://one/ (current)\n1: Second — http://two/"
    );
    assert_eq!(out.display.as_deref(), Some("2 tabs"));

    let out = run(&ctx, json!({"action": "switch_tab", "index": 1})).await;
    assert!(!out.is_error);
    assert_eq!(fake.calls("activate")[0]["id"], "two");
    assert_eq!(
        ctx.env.browser.lock().await.as_ref().unwrap().target_id,
        "two"
    );
    let err = Browser
        .run(json!({"action": "switch_tab", "index": 9}), &ctx)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "no tab 9");
    assert!(
        Browser
            .run(json!({"action": "switch_tab"}), &ctx)
            .await
            .is_err()
    );

    let out = run(&ctx, json!({"action": "new_tab", "url": "example.test"})).await;
    assert!(out.content.starts_with("Opened new tab."));
    // Navigated after attaching, so the first request carries the session's UA.
    let nav = fake.calls("Page.navigate");
    assert_eq!(nav[0]["params"]["url"], "https://example.test");
    assert_eq!(
        ctx.env.browser.lock().await.as_ref().unwrap().target_id,
        "new"
    );
    let before = fake.calls("Page.navigate").len();
    run(&ctx, json!({"action": "new_tab"})).await;
    assert_eq!(
        fake.calls("Page.navigate").len(),
        before,
        "a blank tab stays blank"
    );

    let out = run(&ctx, json!({"action": "close_tab", "index": 9})).await;
    assert!(out.is_error);
    assert!(
        out.content.contains("No tab with index 9"),
        "{}",
        out.content
    );
    // Closing another tab keeps this one; closing this one moves to the first.
    let out = run(&ctx, json!({"action": "close_tab", "index": 0})).await;
    assert_eq!(out.content, "Tab closed.");
    assert_eq!(
        ctx.env.browser.lock().await.as_ref().unwrap().target_id,
        "new"
    );
    run(&ctx, json!({"action": "close_tab"})).await;
    assert_eq!(
        ctx.env.browser.lock().await.as_ref().unwrap().target_id,
        "two"
    );
}

#[tokio::test]
async fn closing_the_last_tab_ends_the_session() {
    let fake = Fake::start().await;
    fake.tabs.lock().unwrap().truncate(1);
    let ctx = fake.ctx(true);
    let out = run(&ctx, json!({"action": "close_tab"})).await;
    assert_eq!(out.content, "Tab closed.");
    assert!(ctx.env.browser.lock().await.is_none());
}

#[tokio::test]
async fn a_browser_with_no_pages_gets_a_new_one() {
    let fake = Fake::start().await;
    fake.tabs.lock().unwrap().clear();
    let ctx = fake.ctx(true);
    assert!(!run(&ctx, json!({"action": "read_page"})).await.is_error);
    assert_eq!(
        ctx.env.browser.lock().await.as_ref().unwrap().target_id,
        "new"
    );
}

#[tokio::test]
async fn a_tab_without_a_debugger_url_is_refused() {
    let fake = Fake::start().await;
    fake.tabs.lock().unwrap()[0] = json!({"id": "x", "type": "page"});
    let ctx = fake.ctx(true);
    let err = Browser
        .run(json!({"action": "read_page"}), &ctx)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "no websocket url");
    let session = Cdp::connect("ws://127.0.0.1:1/none").await;
    let e = session.err().unwrap();
    assert!(
        e.to_string().contains("connecting to Chrome DevTools"),
        "{e}"
    );
}

#[tokio::test]
async fn protocol_errors_and_lost_connections_reach_the_caller() {
    let fake = Fake::start().await;
    let page = Cdp::connect(&format!("ws://127.0.0.1:{}/page/one", fake.ws_port))
        .await
        .unwrap();
    let e = page.call("Fail.me", json!({})).await.unwrap_err();
    assert_eq!(e.to_string(), "Fail.me: no such method");
    let e = page.call("Fail.opaque", json!({})).await.unwrap_err();
    assert!(e.to_string().contains("\"code\":7"), "{e}");
    assert!(page.alive.load(Ordering::Relaxed));

    *fake.drop_on.lock().unwrap() = Some("Page.reload".into());
    let e = page.call("Page.reload", json!({})).await.unwrap_err();
    assert_eq!(e.to_string(), "browser connection closed");
    for _ in 0..200 {
        if !page.alive.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!page.alive.load(Ordering::Relaxed));
}

#[tokio::test]
async fn a_dead_session_is_replaced_on_the_next_action() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    run(&ctx, json!({"action": "read_page"})).await;
    *fake.drop_on.lock().unwrap() = Some("Page.reload".into());
    {
        let guard = ctx.env.browser.lock().await;
        let page = &guard.as_ref().unwrap().page;
        assert!(page.call("Page.reload", json!({})).await.is_err());
        for _ in 0..200 {
            if !page.alive.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    *fake.drop_on.lock().unwrap() = None;
    let before = fake.calls("Page.enable").len();
    assert!(!run(&ctx, json!({"action": "read_page"})).await.is_error);
    assert_eq!(fake.calls("Page.enable").len(), before + 1);
}

#[tokio::test]
async fn wait_is_clamped_and_unknown_actions_are_errors() {
    let fake = Fake::start().await;
    let ctx = fake.ctx(true);
    let out = run(&ctx, json!({"action": "wait", "seconds": 0.0})).await;
    assert_eq!(out.content, "Waited 0.1s.");
    let out = run(&ctx, json!({"action": "explode"})).await;
    assert!(out.is_error);
    assert_eq!(out.content, "Unknown browser action `explode`");
}

#[test]
fn permissions_and_labels_follow_the_action() {
    assert_eq!(Browser.kind(), ToolKind::Browser);
    for a in OBSERVE {
        assert_eq!(
            Browser.kind_for(&json!({"action": a})),
            ToolKind::ReadOnly,
            "{a}"
        );
    }
    for a in [
        "navigate",
        "new_tab",
        "back",
        "forward",
        "reload",
        "switch_tab",
    ] {
        assert_eq!(
            Browser.kind_for(&json!({"action": a})),
            ToolKind::Network,
            "{a}"
        );
    }
    for a in ["click", "type", "evaluate", "", "close_tab"] {
        assert_eq!(
            Browser.kind_for(&json!({"action": a})),
            ToolKind::Browser,
            "{a}"
        );
    }
    assert_eq!(
        Browser.rule_subjects(&json!({"action": "click"})),
        ["click"]
    );
    let label = |v: Value| Browser.label(&v);
    assert_eq!(label(json!({"action": "tabs"})), "Browser(tabs)");
    assert_eq!(label(json!({})), "Browser(?)");
    assert_eq!(
        label(json!({"action": "navigate", "url": "u"})),
        "Browser(navigate: u)"
    );
    assert_eq!(
        label(json!({"action": "click", "ref": 2})),
        "Browser(click: ref 2)"
    );
    assert_eq!(
        label(json!({"action": "click", "selector": "#a"})),
        "Browser(click: #a)"
    );
    assert_eq!(
        label(json!({"action": "type", "text": "hi"})),
        "Browser(type: \"hi\")"
    );
    assert_eq!(
        label(json!({"action": "key", "key": "Tab"})),
        "Browser(key: Tab)"
    );
    assert_eq!(
        label(json!({"action": "evaluate", "script": "1"})),
        "Browser(evaluate: 1)"
    );
    let schema = Browser.schema();
    let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
    for a in actions {
        assert!(
            Browser.description().contains(a.as_str().unwrap()),
            "{a} is undocumented"
        );
    }
}

#[test]
fn a_configured_chrome_path_wins() {
    let cfg = crate::config::BrowserConfig {
        chrome_path: Some("/opt/my-chrome".into()),
        ..Default::default()
    };
    assert_eq!(find_chrome(&cfg).as_deref(), Some("/opt/my-chrome"));
}

#[tokio::test]
async fn a_chrome_that_cannot_start_is_reported() {
    let dir = tmpdir("browser-missing");
    let cfg = crate::config::BrowserConfig {
        chrome_path: Some(dir.join("no-chrome").display().to_string()),
        user_data_dir: Some(dir.join("profile").display().to_string()),
        ..Default::default()
    };
    let e = BrowserSession::start(&cfg).await.err().unwrap();
    assert!(e.to_string().contains("launching Chrome"), "{e:#}");
}

/// A running browser's profile records its port, so a new session attaches.
#[tokio::test]
async fn a_profile_with_a_live_port_is_reused() {
    let fake = Fake::start().await;
    let dir = tmpdir("browser-reuse");
    std::fs::write(
        dir.join("DevToolsActivePort"),
        format!("{}\n/x\n", fake.port),
    )
    .unwrap();
    let cfg = crate::config::BrowserConfig {
        chrome_path: Some("/nonexistent/chrome".into()),
        user_data_dir: Some(dir.display().to_string()),
        ..Default::default()
    };
    let s = BrowserSession::start(&cfg).await.unwrap();
    assert_eq!(s.port, fake.port);
}

/// A shim stands in for Chrome: it records its arguments and writes the port
/// file the way Chrome does, which the launcher waits for.
#[cfg(unix)]
#[tokio::test]
async fn launching_uses_a_private_profile_and_an_ephemeral_port() {
    let fake = Fake::start().await;
    let dir = tmpdir("browser-launch");
    let (bin, args, profile) = (dir.join("chrome"), dir.join("args"), dir.join("profile"));
    std::fs::create_dir_all(&profile).unwrap();
    // A stale file from an earlier run would be read as the new port.
    std::fs::write(profile.join("DevToolsActivePort"), "1\n").unwrap();
    crate::test_support::shim(
        &bin,
        &format!(
            "[ $# -eq 0 ] && exit 0\necho \"$@\" > '{}'\necho {} > '{}/DevToolsActivePort'\nexec sleep 30",
            args.display(),
            fake.port,
            profile.display()
        ),
    );
    let cfg = crate::config::BrowserConfig {
        chrome_path: Some(bin.display().to_string()),
        user_data_dir: Some(profile.display().to_string()),
        headless: Some(true),
        ..Default::default()
    };
    let s = BrowserSession::start(&cfg).await.unwrap();
    kill_launched();
    assert_eq!(s.port, fake.port);
    let args = std::fs::read_to_string(args).unwrap();
    assert!(args.contains("--remote-debugging-port=0"), "{args}");
    assert!(
        args.contains(&format!("--user-data-dir={}", profile.display())),
        "{args}"
    );
    assert!(args.contains("--headless=new"), "{args}");
    assert!(
        args.contains("--disable-blink-features=AutomationControlled"),
        "{args}"
    );
}
