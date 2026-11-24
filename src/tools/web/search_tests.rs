//! WebFetch and WebSearch against a loopback server standing in for the sites
//! and engines. Search engines and the redirect policy see hostile input here:
//! pages they fetch are not under our control.

use super::*;
use crate::agent::tests::{make_env_with, tmpdir};
use crate::permissions::Mode;
use crate::test_support::isolated;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio_util::sync::CancellationToken;

type Seen = std::sync::Arc<Mutex<Vec<String>>>;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    base
}

fn ctx(f: impl FnOnce(&mut crate::config::Config)) -> Ctx {
    let dir = tmpdir("web");
    Ctx {
        env: make_env_with("http://127.0.0.1:9", &dir, Mode::Bypass, 32768, f),
        cancel: CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    }
}

fn html(body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html")], body)
}

async fn site() -> String {
    let hits: Seen = Default::default();
    let app = Router::new()
        .route(
            "/page",
            get(|| async { html("<html><head><script>evil()</script></head><body><h1>Title</h1><p>Words &amp; more</p></body></html>") }),
        )
        .route("/text", get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "plain body") }))
        .route("/sniffed", get(|| async { ([(header::CONTENT_TYPE, "application/x")], "  <p>looks like html</p>") }))
        .route("/gone", get(|| async { StatusCode::NOT_FOUND }))
        .route("/here", get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/page")]) }))
        .route("/away", get(|| async { (StatusCode::FOUND, [(header::LOCATION, "http://elsewhere.invalid/x")]) }))
        .route("/bare", get(|| async { StatusCode::FOUND }))
        .route("/loop", get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop")]) }))
        .route(
            "/huge",
            get(|| async {
                let chunk = "x".repeat(1 << 20);
                let stream = futures::stream::iter(
                    (0..64).map(move |_| Ok::<_, std::convert::Infallible>(chunk.clone())),
                );
                ([(header::CONTENT_TYPE, "text/plain")], axum::body::Body::from_stream(stream))
            }),
        )
        .route(
            "/counted",
            get({
                let hits = hits.clone();
                move || {
                    let hits = hits.clone();
                    async move {
                        hits.lock().unwrap().push("hit".into());
                        format!("hit {}", hits.lock().unwrap().len())
                    }
                }
            }),
        );
    serve(app).await
}

#[tokio::test]
async fn fetching_converts_html_and_reports_failures() {
    let base = site().await;
    let (status, md) = fetch_markdown(&format!("{base}/page")).await.unwrap();
    assert_eq!(status, "200 OK");
    assert!(md.contains("Title") && md.contains("Words & more"), "{md}");
    assert!(!md.contains("evil()"), "scripts are dropped: {md}");
    assert_eq!(
        fetch_markdown(&format!("{base}/text")).await.unwrap().1,
        "plain body"
    );
    assert!(
        !fetch_markdown(&format!("{base}/sniffed"))
            .await
            .unwrap()
            .1
            .contains("<p>"),
        "HTML by its first byte, whatever the header says"
    );

    let e = fetch_markdown(&format!("{base}/gone"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("HTTP 404 Not Found fetching"), "{e}");
    assert!(fetch_markdown("http://127.0.0.1:1/refused").await.is_err());
}

#[tokio::test]
async fn redirects_are_followed_only_within_the_host() {
    let base = site().await;
    let (_, md) = fetch_markdown(&format!("{base}/here")).await.unwrap();
    assert!(md.contains("Title"), "{md}");

    // Another host is reported to the model, never fetched for it.
    let (status, md) = fetch_markdown(&format!("{base}/away")).await.unwrap();
    assert_eq!(status, "302 Found");
    assert!(
        md.starts_with("The URL redirected to a different host: http://elsewhere.invalid/x"),
        "{md}"
    );
    let e = fetch_markdown(&format!("{base}/loop")).await.unwrap_err();
    assert!(format!("{e:?}").contains("too many redirects"), "{e:?}");
    let (_, md) = fetch_markdown(&format!("{base}/bare")).await.unwrap();
    assert!(md.contains("different host: ?"), "{md}");
}

#[tokio::test]
async fn downloads_are_capped_and_results_cached() {
    let base = site().await;
    let (_, md) = fetch_markdown(&format!("{base}/huge")).await.unwrap();
    assert!(md.len() <= (8 << 20) + (1 << 20), "kept {} bytes", md.len());
    assert!(md.len() > 8 << 20);

    let url = format!("{base}/counted");
    let (status, first) = fetch_markdown(&url).await.unwrap();
    assert_eq!((status.as_str(), first.as_str()), ("200 OK", "hit 1"));
    let (status, second) = fetch_markdown(&url).await.unwrap();
    assert_eq!((status.as_str(), second.as_str()), ("cached", "hit 1"));
}

#[tokio::test]
async fn the_fetch_tool_marks_content_untrusted_and_limits_it() {
    let base = site().await;
    let tool = WebFetch;
    let small = ctx(|c| c.web.fetch_max_chars = Some(5));
    let out = tool
        .run(json!({"url": format!("{base}/text")}), &small)
        .await
        .unwrap();
    assert!(
        out.content
            .contains(&format!("<web_content url=\"{base}/text\">"))
    );
    assert!(out.content.contains("untrusted data"));
    assert!(
        !out.content.contains("plain body"),
        "truncated to 5: {}",
        out.content
    );

    // With a prompt the relevant part is kept instead of the start.
    let out = tool
        .run(
            json!({"url": format!("{base}/text"), "prompt": "body"}),
            &small,
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("body"), "{}", out.content);

    let out = tool
        .run(json!({"url": format!("{base}/gone")}), &ctx(|_| {}))
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.starts_with("Fetch failed:"),
        "{}",
        out.content
    );
    assert!(tool.run(json!({}), &ctx(|_| {})).await.is_err());
    assert_eq!(tool.kind(), ToolKind::Network);
}

#[test]
fn relevant_chunks_are_chosen_by_the_prompts_terms() {
    let filler = "Unrelated filler text about nothing in particular.\n\n".repeat(60);
    let md = format!("Intro paragraph.\n\n{filler}The zebra migrates every spring.\n\n{filler}");
    let out = select_relevant(&md, "where does the zebra go", 2500);
    assert!(out.len() < 2700, "{}", out.len());
    assert!(out.contains("Intro paragraph."), "the intro stays: {out}");
    assert!(out.contains("zebra migrates"), "{out}");
    assert!(out.contains("…\n\n"), "gaps are marked: {out}");
    // No useful terms: only the intro, not the whole page.
    let intro_only = select_relevant(&md, "the and", 2500);
    assert!(!intro_only.contains("zebra"), "{intro_only}");
    assert!(intro_only.starts_with("[Long page (6 KB)"), "{intro_only}");
}

#[test]
fn search_links_are_decoded_defensively() {
    assert_eq!(
        decode_ddg_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.test%2Fp%3Fq%3D1&rut=x"),
        "https://a.test/p?q=1"
    );
    assert_eq!(decode_ddg_href("//b.test/x"), "https://b.test/x");
    assert_eq!(decode_ddg_href("https://c.test/"), "https://c.test/");
    assert_eq!(decode_ddg_href("x?uddg=%FF%FE"), "", "not UTF-8: dropped");
    assert_eq!(
        html_unescape("&lt;a&gt; &amp; &quot;b&quot; &#39;c&#x27;&nbsp;"),
        "<a> & \"b\" 'c' "
    );
    assert_eq!(strip_tags("a <b>bold</b> &amp; c"), "a bold & c");
}

const DDG_PAGE: &str = r#"<html><body>
<div class="result result--ad"><a class="result__a" href="https://ad.test/">Ad</a></div>
<div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fone.test%2F&x=1">One  Title</a>
  <a class="result__snippet">First   snippet</a></div>
<div class="result"><a class="result__a" href="https://two.test/">Two</a></div>
<div class="result"><span>no link</span></div>
</body></html>"#;

const BING_PAGE: &str = r#"<html><body><ol>
<li class="b_algo"><h2><a href="https://bing-one.test/">Bing One</a></h2><p>Bing snippet</p></li>
</ol></body></html>"#;

/// Every engine, in one child process: the pacing and blocked state are global.
#[tokio::test]
async fn engines_parse_their_answers_and_auto_falls_back() {
    if isolated("tools::web::search_tests::engines_parse_their_answers_and_auto_falls_back") {
        return;
    }
    let seen: Seen = Default::default();
    let log = |seen: &Seen, line: String| seen.lock().unwrap().push(line);
    let (s1, s2, s3) = (seen.clone(), seen.clone(), seen.clone());
    let ddg_page = std::sync::Arc::new(Mutex::new(DDG_PAGE.to_string()));
    let page = ddg_page.clone();
    let app = Router::new()
        .route(
            "/res/v1/web/search",
            get(move |h: axum::http::HeaderMap| async move {
                log(&s1, format!("brave {:?}", h.get("x-subscription-token").unwrap()));
                Json(json!({"web": {"results": [
                    {"title": "B", "url": "https://brave.test/", "description": "a <b>bold</b> one"},
                    {}
                ]}}))
            }),
        )
        .route(
            "/search",
            post(move |Json(b): Json<Value>| async move {
                log(&s2, format!("tavily {}", b["api_key"]));
                Json(json!({"results": [{"title": "T", "url": "https://tavily.test/", "content": "tav"}]}))
            })
            .get(|q: axum::extract::RawQuery| async move {
                let q = q.0.unwrap_or_default();
                if q.contains("format=json") {
                    Json(json!({"results": [{"title": "S", "url": "https://sx.test/", "content": "sx"}]})).into_response()
                } else {
                    html(BING_PAGE).into_response()
                }
            }),
        )
        .route(
            "/html/",
            post(move || {
                let page = page.clone();
                let s3 = s3.clone();
                async move {
                    log(&s3, "ddg".into());
                    let body = page.lock().unwrap().clone();
                    ([(header::CONTENT_TYPE, "text/html")], body)
                }
            }),
        );
    let base = serve(app).await;
    // SAFETY: the test runs alone in its own child process.
    unsafe { std::env::set_var("CHANTO_TEST_WEB_BASE", &base) };
    let web = |f: &dyn Fn(&mut crate::config::WebConfig)| {
        let mut c = crate::config::WebConfig {
            search_min_gap_ms: Some(0),
            ..Default::default()
        };
        f(&mut c);
        c
    };

    let r = search(
        &web(&|c| {
            c.search_backend = Some("brave".into());
            c.brave_api_key = Some("brave-key".into());
        }),
        "q",
    )
    .await
    .unwrap();
    assert_eq!(r[0].url, "https://brave.test/");
    assert_eq!(r[0].snippet, "a bold one");
    let e = search(&web(&|c| c.search_backend = Some("brave".into())), "q")
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "no brave_api_key");

    let r = search(
        &web(&|c| {
            c.search_backend = Some("tavily".into());
            c.tavily_api_key = Some("tav-key".into());
        }),
        "q",
    )
    .await
    .unwrap();
    assert_eq!(r[0].snippet, "tav");
    let e = search(&web(&|c| c.search_backend = Some("tavily".into())), "q")
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "no tavily_api_key");

    let r = search(
        &web(&|c| {
            c.search_backend = Some("searxng".into());
            c.searxng_url = Some(format!("{base}/"));
        }),
        "q",
    )
    .await
    .unwrap();
    assert_eq!(r[0].url, "https://sx.test/");
    let e = search(&web(&|c| c.search_backend = Some("searxng".into())), "q")
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "no searxng_url");

    // DuckDuckGo: ads and link-less results are dropped, links decoded, text tidied.
    let r = search(&web(&|c| c.search_backend = Some("ddg".into())), "q")
        .await
        .unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].url, "https://one.test/");
    assert_eq!(r[0].title, "One Title");
    assert_eq!(r[0].snippet, "First snippet");
    assert_eq!(r[1].snippet, "");

    // Auto: a configured engine wins; a failing one falls through to the free ones.
    let r = search(&web(&|c| c.brave_api_key = Some("k".into())), "q")
        .await
        .unwrap();
    assert_eq!(r[0].url, "https://brave.test/");
    let r = search(&web(&|c| c.tavily_api_key = Some("k".into())), "q")
        .await
        .unwrap();
    assert_eq!(r[0].url, "https://tavily.test/");
    let r = search(&web(&|c| c.searxng_url = Some(base.clone())), "q")
        .await
        .unwrap();
    assert_eq!(r[0].url, "https://sx.test/");
    let r = search(&web(&|_| {}), "q").await.unwrap();
    assert_eq!(r[0].url, "https://one.test/");

    // No results from DuckDuckGo: Bing. A captcha page marks it blocked too.
    *ddg_page.lock().unwrap() = "<html>nothing here</html>".into();
    let r = search(&web(&|_| {}), "q").await.unwrap();
    assert_eq!(r[0].url, "https://bing-one.test/");
    assert_eq!(r[0].title, "Bing One");
    *ddg_page.lock().unwrap() = "<html>anomaly detected</html>".into();
    let e = search(&web(&|c| c.search_backend = Some("ddg".into())), "q")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("rate-limited"), "{e}");
    let e = search(&web(&|c| c.search_backend = Some("ddg".into())), "q")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("blocked a recent search"), "{e}");

    let log = seen.lock().unwrap().join("\n");
    assert!(
        log.contains("brave \"brave-key\"") && log.contains("tavily \"tav-key\""),
        "{log}"
    );
}

/// The tool filters by domain and caps the count, in a child for the same reason.
#[tokio::test]
async fn the_search_tool_filters_domains() {
    if isolated("tools::web::search_tests::the_search_tool_filters_domains") {
        return;
    }
    let page = r#"<html><body>
<div class="result"><a class="result__a" href="https://docs.rust-lang.org/a">A</a></div>
<div class="result"><a class="result__a" href="https://spam.test/b">B</a></div>
<div class="result"><a class="result__a" href="https://other.test/c">C</a></div>
</body></html>"#;
    let queries: Seen = Default::default();
    let q2 = queries.clone();
    let app = Router::new().route(
        "/html/",
        post(move |form: String| {
            let q2 = q2.clone();
            async move {
                q2.lock().unwrap().push(form);
                html(page)
            }
        }),
    );
    let base = serve(app).await;
    // SAFETY: the test runs alone in its own child process.
    unsafe { std::env::set_var("CHANTO_TEST_WEB_BASE", &base) };
    let ctx = ctx(|c| {
        c.web.search_backend = Some("ddg".into());
        c.web.search_min_gap_ms = Some(0);
    });
    let tool = WebSearch;
    let out = tool
        .run(
            json!({"query": "rust", "blocked_domains": ["spam.test"]}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        out.content.contains("1. A\n") && out.content.contains("2. C\n"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("spam.test"), "{}", out.content);
    assert_eq!(out.display.as_deref(), Some("2 results"));

    let out = tool
        .run(json!({"query": "rust", "max_results": 1}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.display.as_deref(), Some("1 result"));

    // One allowed domain narrows the query itself.
    let out = tool
        .run(
            json!({"query": "rust", "allowed_domains": ["rust-lang.org"]}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.content.contains("docs.rust-lang.org") && !out.content.contains("other.test"));
    assert!(
        queries
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("site%3Arust-lang.org"),
        "{:?}",
        queries.lock().unwrap()
    );

    let out = tool
        .run(
            json!({"query": "x", "allowed_domains": ["none.test", "nil.test"]}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(out.content, "No results.");
    assert!(tool.run(json!({}), &ctx).await.is_err());
    assert_eq!(tool.label(&json!({"query": "q"})), "WebSearch(\"q\")");
    assert_eq!(tool.kind(), ToolKind::Network);
}
