use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn chunks_overlap_and_cover_the_file() {
    let text: String = (1..=100).map(|i| format!("line{i}\n")).collect();
    let chunks = chunks_of("f.rs", &text);
    assert!(chunks.len() >= 3);
    // A line beyond the first window appears in a later chunk.
    assert!(chunks.iter().any(|c| c.text.contains("line50")));
    // Windows advance (no infinite loop) and stay in order.
    for w in chunks.windows(2) {
        assert!(w[0].start < w[1].start);
    }
    // The overlap means a line can appear in two chunks.
    let shared = chunks.iter().filter(|c| c.text.contains("line40")).count();
    assert!(shared >= 1);
    for c in &chunks {
        assert!(c.start >= 1 && c.end >= c.start);
    }
}

#[test]
fn a_short_file_is_one_chunk() {
    let chunks = chunks_of("f.rs", "one\ntwo\n");
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].start, chunks[0].end), (1, 2));
}

#[test]
fn vectors_round_trip_and_normalize() {
    let v = normalize(vec![3.0, 0.0, 4.0]);
    assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    let back = dec(&enc(&v));
    assert_eq!(back, v);
    // Orthogonal vectors score zero.
    assert!(cosine(&normalize(vec![1.0, 0.0]), &normalize(vec![0.0, 1.0])).abs() < 1e-6);
}

#[test]
fn rank_returns_the_closest_first() {
    let mk = |p: &str, v: Vec<f32>| Entry {
        path: p.into(),
        start: 1,
        end: 2,
        mtime: 0,
        size: 0,
        vec: enc(&normalize(v)),
    };
    let entries = vec![
        mk("far.rs", vec![0.0, 1.0]),
        mk("near.rs", vec![1.0, 0.1]),
        mk("empty.rs", vec![]),
    ];
    let hits = rank(&normalize(vec![1.0, 0.0]), &entries, 5);
    // The empty vector is skipped; the aligned one wins.
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].1.path, "near.rs");
    assert!(hits[0].0 > hits[1].0);
}

#[test]
fn endpoint_defaults_to_local_ollama() {
    let ep = endpoint(&EmbeddingsConfig::default()).unwrap();
    assert_eq!(ep.model, "nomic-embed-text");
    assert_eq!(ep.base, "http://localhost:11434");
    assert!(matches!(ep.kind, Kind::Ollama));
    let ep = endpoint(&EmbeddingsConfig {
        provider: Some("openai".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(ep.base, "https://api.openai.com/v1");
    assert!(matches!(ep.kind, Kind::OpenAi));
    assert!(
        endpoint(&EmbeddingsConfig {
            provider: Some("nope".into()),
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
fn project_keys_differ_by_path() {
    let a = project_key(Path::new("/tmp/a"));
    let b = project_key(Path::new("/tmp/b"));
    assert_ne!(a, b);
    assert_eq!(a, project_key(Path::new("/tmp/a")));
    assert_eq!(a.len(), 16);
}

/// A one-shot-per-request HTTP server that answers every POST with `body`,
/// and counts how many requests it saw. Hand-rolled so the tests need no
/// mock server crate.
async fn serve(status: u16, body: &'static str) -> (String, Arc<AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            count.fetch_add(1, Ordering::SeqCst);
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            tokio::spawn(async move {
                let mut buf = [0u8; 16 * 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), hits)
}

fn ep_at(kind: Kind, base: String) -> Endpoint {
    Endpoint {
        kind,
        model: "mock".into(),
        base,
        key: Some("sk-test".into()),
        batch: 4,
    }
}

#[tokio::test]
async fn embeds_over_the_ollama_route() {
    let (base, hits) = serve(200, r#"{"embeddings":[[1.0,0.0],[0.0,1.0]]}"#).await;
    let ep = ep_at(Kind::Ollama, base);
    let v = embed(&ep, &["a".into(), "b".into()]).await.unwrap();
    assert_eq!(v, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn embeds_over_the_openai_route_in_index_order() {
    // `data` deliberately arrives out of order.
    let (base, _) = serve(
        200,
        r#"{"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]}"#,
    )
    .await;
    let ep = ep_at(Kind::OpenAi, base);
    let v = embed(&ep, &["a".into(), "b".into()]).await.unwrap();
    assert_eq!(v, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
}

#[tokio::test]
async fn a_failing_endpoint_reports_the_status() {
    let (base, _) = serve(500, "boom").await;
    let ep = ep_at(Kind::OpenAi, base);
    let err = embed(&ep, &["a".into()]).await.unwrap_err().to_string();
    assert!(err.contains("500"), "{err}");
    // And an unreachable one explains itself rather than panicking.
    let ep = ep_at(Kind::Ollama, "http://127.0.0.1:1".into());
    let err = embed(&ep, &["a".into()]).await.unwrap_err().to_string();
    assert!(err.contains("cannot reach"), "{err}");
}

#[tokio::test]
async fn sync_embeds_once_then_reuses_the_cache() {
    // The cache lives under the Chanto home, which other tests move.
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("chanto-sem-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "pub fn go() {}\npub fn stop() {}\n").unwrap();
    let (base, hits) = serve(200, r#"{"embeddings":[[1.0,0.0]]}"#).await;
    let ep = ep_at(Kind::Ollama, base);
    let first = sync(&dir, &ep, false).await.unwrap();
    assert_eq!(first.entries.len(), 1, "one file is one chunk");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // Nothing changed: the stored vector is reused, so no new request.
    let again = sync(&dir, &ep, false).await.unwrap();
    assert_eq!(again.entries.len(), 1);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "cache must avoid re-embedding"
    );
}

#[tokio::test]
async fn sync_re_embeds_an_edited_file() {
    let dir = std::env::temp_dir().join(format!("chanto-sem-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let f = dir.join("src/a.rs");
    std::fs::write(&f, "pub fn go() {}\n").unwrap();
    let (base, hits) = serve(200, r#"{"embeddings":[[1.0,0.0]]}"#).await;
    let ep = ep_at(Kind::Ollama, base);
    sync(&dir, &ep, false).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    std::fs::write(&f, "pub fn go() {}\npub fn extra() {}\n").unwrap();
    let edited = sync(&dir, &ep, false).await.unwrap();
    assert_eq!(edited.entries.len(), 1);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "a changed file is re-embedded"
    );
}

/// Each queued chunk is written back to its own slot. A first build, or a
/// branch switch, queues every chunk at once, so a slot that doubles as
/// the queue grows past the vector it is written into.
#[tokio::test]
async fn sync_embeds_every_chunk_it_queues() {
    let dir = std::env::temp_dir().join(format!("chanto-sem-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(dir.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    let (base, _) = serve(200, r#"{"embeddings":[[1.0,0.0],[0.0,1.0]]}"#).await;
    let mut ep = ep_at(Kind::Ollama, base);
    ep.batch = 2;
    let index = sync(&dir, &ep, false).await.unwrap();
    assert_eq!(index.entries.len(), 2);
    assert!(index.entries.iter().all(|e| !e.vec.is_empty()));
}
