//! SemanticSearch: vector search over the project's text.
//!
//! Grep finds the strings you already know. This finds the code that *means*
//! the same thing — "where do we retry after a failure?" without the word
//! "retry". It chunks the tree, embeds every chunk once into a cache under
//! `~/.chanto/cache/embeddings`, and answers later queries by cosine
//! similarity against those vectors. Unchanged files are never re-embedded,
//! so the first query in a repo pays for the index and the rest are cheap.
//!
//! Where the vectors come from is `[embeddings]` in the config: a local Ollama
//! by default, or any OpenAI-compatible `/v1/embeddings`.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_bool, arg_str, arg_u64};
use crate::config::{self, EmbeddingsConfig};
use crate::util::rel;
use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

/// Lines per chunk. Big enough to hold a function, small enough that its
/// embedding stays about one idea.
const CHUNK_LINES: usize = 40;
/// Lines shared between neighbouring chunks, so a definition that straddles a
/// boundary is still whole in one of them.
const OVERLAP: usize = 6;
const MAX_FILES: usize = 2000;
const MAX_CHUNKS: usize = 8000;
const MAX_FILE_BYTES: u64 = 256 << 10;
const DEFAULT_K: usize = 8;
/// Embeddings are a compressor: a file this big embeds slowly and dilutes the
/// chunk's meaning, so it is chunked like any other but not scanned forever.
const EMBED_TIMEOUT: Duration = Duration::from_secs(120);

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        crate::http::builder()
            .timeout(EMBED_TIMEOUT)
            .build()
            .unwrap_or_default()
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Ollama,
    OpenAi,
}

/// A resolved embedding endpoint.
struct Endpoint {
    kind: Kind,
    model: String,
    base: String,
    key: Option<String>,
    batch: usize,
}

fn endpoint(cfg: &EmbeddingsConfig) -> Result<Endpoint> {
    let kind = match cfg.provider.as_deref().map(str::trim).unwrap_or("") {
        "" | "ollama" => Kind::Ollama,
        "openai" | "openai-compatible" | "openai_compatible" => Kind::OpenAi,
        other => {
            return Err(anyhow!(
                "unknown embeddings provider {other:?}: use \"ollama\" or \"openai\""
            ));
        }
    };
    let model = cfg
        .model
        .clone()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| match kind {
            Kind::Ollama => "nomic-embed-text".into(),
            Kind::OpenAi => "text-embedding-3-small".into(),
        });
    let base = cfg
        .base_url
        .clone()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| match kind {
            Kind::Ollama => "http://localhost:11434".into(),
            Kind::OpenAi => "https://api.openai.com/v1".into(),
        });
    let key = cfg
        .api_key
        .clone()
        .filter(|k| !k.trim().is_empty())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok());
    Ok(Endpoint {
        kind,
        model,
        base: base.trim_end_matches('/').to_string(),
        key,
        batch: cfg.batch.unwrap_or(16).clamp(1, 128),
    })
}

/// A slice of a file, as `start..=end` (1-based lines).
struct Chunk {
    path: String,
    start: usize,
    end: usize,
    text: String,
}

/// Split a file into overlapping windows of `CHUNK_LINES`.
fn chunks_of(path: &str, text: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let end = (i + CHUNK_LINES).min(lines.len());
        let body = lines[i..end].join("\n");
        if !body.trim().is_empty() {
            out.push(Chunk {
                path: path.to_string(),
                start: i + 1,
                end,
                text: body,
            });
        }
        if end >= lines.len() {
            break;
        }
        i = end - OVERLAP;
    }
    out
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-9 {
        for x in &mut v {
            *x /= n;
        }
    }
    v
}

/// Cosine similarity of two unit vectors (a plain dot product).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn enc(v: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn dec(s: &str) -> Vec<f32> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map(|b| {
            b.as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    start: usize,
    end: usize,
    mtime: u64,
    size: u64,
    vec: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Index {
    model: String,
    entries: Vec<Entry>,
}

/// A stable, filesystem-safe name for a project root.
fn project_key(root: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in crate::util::Canonical::canonical(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .bytes()
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn index_path(root: &Path) -> PathBuf {
    config::home_dir()
        .join("cache")
        .join("embeddings")
        .join(format!("{}.json", project_key(root)))
}

/// One file's worth of chunks, with the metadata that decides whether its
/// vectors can be reused.
struct FileChunks {
    mtime: u64,
    size: u64,
    chunks: Vec<Chunk>,
}

fn is_text(bytes: &[u8]) -> bool {
    !bytes.iter().take(8000).any(|&b| b == 0)
}

/// Read the project into per-file chunks, skipping binaries and the huge
/// generated files (lockfiles, bundles) that would swamp the index.
fn collect(root: &Path) -> Vec<(String, FileChunks)> {
    let mut files: Vec<(String, FileChunks)> = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| {
            let n = e.file_name();
            n != ".git" && n != "node_modules" && n != "target" && n != ".venv"
        })
        .threads(4)
        .build_parallel();
    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    walker.run(|| {
        let out = out.clone();
        Box::new(move |entry| {
            let Ok(e) = entry else {
                return ignore::WalkState::Continue;
            };
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return ignore::WalkState::Continue;
            }
            if out.lock().unwrap().len() >= MAX_FILES {
                return ignore::WalkState::Quit;
            }
            let md = match e.metadata() {
                Ok(m) => m,
                Err(_) => return ignore::WalkState::Continue,
            };
            if md.len() > MAX_FILE_BYTES || md.len() == 0 {
                return ignore::WalkState::Continue;
            }
            let Ok(bytes) = std::fs::read(e.path()) else {
                return ignore::WalkState::Continue;
            };
            if !is_text(&bytes) {
                return ignore::WalkState::Continue;
            }
            let path = rel(root, e.path());
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let chunks = chunks_of(&path, &text);
            if chunks.is_empty() {
                return ignore::WalkState::Continue;
            }
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.lock().unwrap().push((
                path,
                FileChunks {
                    mtime,
                    size: md.len(),
                    chunks,
                },
            ));
            ignore::WalkState::Continue
        })
    });
    let mut all = match std::sync::Arc::try_unwrap(out) {
        Ok(m) => m.into_inner().unwrap(),
        Err(_) => Vec::new(),
    };
    all.sort_by(|a, b| a.0.cmp(&b.0));
    files.extend(all);
    files
}

/// Ask the endpoint for embeddings of `texts`, in batches.
async fn embed(ep: &Endpoint, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let mut out = Vec::with_capacity(texts.len());
    for batch in texts.chunks(ep.batch) {
        match ep.kind {
            Kind::Ollama => embed_ollama(ep, batch)
                .await?
                .into_iter()
                .for_each(|v| out.push(v)),
            Kind::OpenAi => embed_openai(ep, batch)
                .await?
                .into_iter()
                .for_each(|v| out.push(v)),
        }
    }
    Ok(out)
}

async fn embed_ollama(ep: &Endpoint, batch: &[String]) -> Result<Vec<Vec<f32>>> {
    let url = format!("{}/api/embed", ep.base);
    let resp = client()
        .post(&url)
        .json(&json!({"model": ep.model, "input": batch}))
        .send()
        .await
        .with_context(|| format!("cannot reach {url} — is Ollama running?"))?;
    if resp.status().is_success() {
        let body: Value = resp.json().await?;
        let arr = body["embeddings"].as_array().cloned().unwrap_or_default();
        if arr.len() == batch.len() {
            return Ok(arr.iter().map(floats).collect());
        }
    } else if resp.status() == reqwest::StatusCode::NOT_FOUND {
        // Ollama before 0.1.44 has no batched /api/embed: one text at a time.
        let mut out = Vec::with_capacity(batch.len());
        for t in batch {
            let url = format!("{}/api/embeddings", ep.base);
            let body: Value = client()
                .post(&url)
                .json(&json!({"model": ep.model, "prompt": t}))
                .send()
                .await?
                .json()
                .await?;
            out.push(floats(&body["embedding"]));
        }
        return Ok(out);
    } else {
        return Err(anyhow!(
            "{url} returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ));
    }
    Err(anyhow!(
        "expected {} embeddings from {url} for model {}; is it an embedding model? \
         (set [embeddings] model=…)",
        batch.len(),
        ep.model
    ))
}

async fn embed_openai(ep: &Endpoint, batch: &[String]) -> Result<Vec<Vec<f32>>> {
    let url = format!("{}/embeddings", ep.base);
    let mut req = client()
        .post(&url)
        .json(&json!({"model": ep.model, "input": batch}));
    if let Some(key) = &ep.key {
        req = req.bearer_auth(key);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "{url} returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ));
    }
    let body: Value = resp.json().await?;
    let data = body["data"].as_array().cloned().unwrap_or_default();
    let mut out: Vec<(usize, Vec<f32>)> = Vec::new();
    for (i, d) in data.iter().enumerate() {
        let at = d["index"].as_u64().map(|x| x as usize).unwrap_or(i);
        out.push((at, floats(&d["embedding"])));
    }
    out.sort_by_key(|(i, _)| *i);
    Ok(out.into_iter().map(|(_, v)| v).collect())
}

fn floats(v: &Value) -> Vec<f32> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect()
        })
        .unwrap_or_default()
}

/// Load the index for `root`, re-embedding only what changed. Returns the
/// index and whether anything was embedded.
async fn sync(root: &Path, ep: &Endpoint, rebuild: bool) -> Result<Index> {
    let path = index_path(root);
    let old: Index = if rebuild {
        Index::default()
    } else {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|i: &Index| i.model == ep.model)
            .unwrap_or_default()
    };
    // Reusable vectors: same path, same span, same file version.
    let mut reusable: std::collections::HashMap<(String, usize, usize), (u64, u64, String)> = old
        .entries
        .into_iter()
        .map(|e| ((e.path, e.start, e.end), (e.mtime, e.size, e.vec)))
        .collect();

    let files = tokio::task::spawn_blocking({
        let root = root.to_path_buf();
        move || collect(&root)
    })
    .await?;

    let mut entries: Vec<Entry> = Vec::new();
    let mut pending: Vec<(usize, String)> = Vec::new();
    for (_path, fc) in &files {
        for c in &fc.chunks {
            let key = (c.path.clone(), c.start, c.end);
            match reusable.remove(&key) {
                Some((mtime, size, vec)) if mtime == fc.mtime && size == fc.size => {
                    entries.push(Entry {
                        path: c.path.clone(),
                        start: c.start,
                        end: c.end,
                        mtime: fc.mtime,
                        size: fc.size,
                        vec,
                    });
                }
                _ => {
                    if entries.len() < MAX_CHUNKS {
                        // The slot this chunk occupies; pending holds the
                        // chunks already pushed, so their count must not be
                        // added to the length a second time.
                        pending.push((entries.len(), c.text.clone()));
                        entries.push(Entry {
                            path: c.path.clone(),
                            start: c.start,
                            end: c.end,
                            mtime: fc.mtime,
                            size: fc.size,
                            vec: String::new(),
                        });
                    }
                }
            }
        }
    }

    let mut embedded = 0usize;
    if !pending.is_empty() {
        let texts: Vec<String> = pending.iter().map(|(_, t)| t.clone()).collect();
        let vectors = embed(ep, &texts)
            .await
            .with_context(|| format!("embedding {} chunks with {}", texts.len(), ep.model))?;
        for ((i, _), v) in pending.iter().zip(vectors) {
            entries[*i].vec = enc(&normalize(v));
            embedded += 1;
        }
    }
    entries.retain(|e| !e.vec.is_empty());
    let index = Index {
        model: ep.model.clone(),
        entries,
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if embedded > 0 || !path.exists() {
        let _ = std::fs::write(&path, serde_json::to_string(&index)?);
    }
    Ok(index)
}

/// The `k` best entries for a unit query vector.
fn rank<'a>(query: &[f32], entries: &'a [Entry], k: usize) -> Vec<(f32, &'a Entry)> {
    let mut scored: Vec<(f32, &Entry)> = entries
        .iter()
        .filter(|e| !e.vec.is_empty())
        .map(|e| (cosine(query, &dec(&e.vec)), e))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

fn render(
    query: &str,
    ep: &Endpoint,
    index: &Index,
    hits: &[(f32, &Entry)],
    root: &Path,
) -> String {
    let mut out = format!(
        "Semantic search for {:?} — {} chunks indexed with {}, best {}:\n",
        query,
        index.entries.len(),
        ep.model,
        hits.len()
    );
    for (score, e) in hits {
        let snippet = std::fs::read_to_string(root.join(&e.path))
            .ok()
            .and_then(|t| {
                t.lines()
                    .skip(e.start.saturating_sub(1))
                    .take(e.end + 1 - e.start)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into()
            })
            .map(|s: String| crate::util::truncate(&s, 400))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n{score:.3}  {}:{}-{}\n{}\n",
            e.path,
            e.start,
            e.end,
            snippet
                .lines()
                .map(|l| format!("│{l}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    out
}

pub struct SemanticSearch;

#[async_trait]
impl Tool for SemanticSearch {
    fn name(&self) -> String {
        "SemanticSearch".into()
    }
    fn description(&self) -> String {
        "Search the project by meaning rather than by name: embed the query and \
         rank the code against an on-disk vector index, so it finds the code that \
         does what you mean even when it uses none of your words. The index is \
         built once and updated incrementally; embedding runs against the \
         `[embeddings]` endpoint (a local Ollama `nomic-embed-text` by default). \
         Use `path` to search a subtree, `k` for more or fewer hits, and \
         `rebuild` after changing the embedding model."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What you are looking for, in plain language or as a line of code"},
                "k": {"type": "integer", "description": "How many results (default 8, max 50)"},
                "path": {"type": "string", "description": "Subdirectory to search (default: the project root)"},
                "rebuild": {"type": "boolean", "description": "Re-embed everything, ignoring the cache"}
            },
            "required": ["query"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn rule_subjects(&self, args: &Value) -> Vec<String> {
        vec![
            args["path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .unwrap_or(".")
                .to_string(),
        ]
    }
    fn label(&self, args: &Value) -> String {
        format!(
            "SemanticSearch({})",
            crate::util::one_line(arg_str(args, "query").unwrap_or(""), 60)
        )
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let query = arg_str(&args, "query")?.trim().to_string();
        if query.is_empty() {
            anyhow::bail!("`query` is required");
        }
        let cwd = ctx.cwd();
        let root = args["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(|p| crate::util::resolve(&cwd, p))
            .unwrap_or(cwd.clone());
        let k = arg_u64(&args, "k").unwrap_or(DEFAULT_K as u64).clamp(1, 50) as usize;
        let ep = endpoint(&ctx.env.config.embeddings)?;
        let index = sync(&root, &ep, arg_bool(&args, "rebuild")).await?;
        if index.entries.is_empty() {
            return Ok(ToolOutput::err(format!(
                "Nothing indexed under {} — the tree has no readable text files.",
                root.display()
            )));
        }
        let mut qv = embed(&ep, std::slice::from_ref(&query)).await?;
        let Some(q) = qv.pop() else {
            anyhow::bail!("the embeddings endpoint returned nothing for the query");
        };
        let hits = rank(&normalize(q), &index.entries, k);
        Ok(
            ToolOutput::ok(render(&query, &ep, &index, &hits, &root)).display(format!(
                "SemanticSearch: {} hits in {} chunks",
                hits.len(),
                index.entries.len()
            )),
        )
    }
}

#[cfg(test)]
mod tests {
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
        let dir =
            std::env::temp_dir().join(format!("chanto-sem-{}", uuid::Uuid::new_v4().simple()));
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
        let dir =
            std::env::temp_dir().join(format!("chanto-sem-{}", uuid::Uuid::new_v4().simple()));
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
        let dir =
            std::env::temp_dir().join(format!("chanto-sem-{}", uuid::Uuid::new_v4().simple()));
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
}
