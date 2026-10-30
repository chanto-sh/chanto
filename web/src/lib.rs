//! chanto in the browser: a session holding the agent loop and an in-memory
//! project, exported to JavaScript. The page supplies the model as a
//! `transport` function; the files never leave the page.
//!
//! ```js
//! const s = new Session("http://local/v1", "qwen", null, systemPrompt, true, "/project");
//! s.write_file("/project/main.py", "print('helo')\n");
//! const answer = await s.run("fix the typo", transport, (json) => console.log(JSON.parse(json)));
//! ```
//!
//! `transport(url, headersJson, body)` gets the request (`body` a
//! `Uint8Array` of OpenAI chat-completions JSON) and returns a promise of a
//! reader: an object whose `read()` resolves to `{done, value}` with
//! `Uint8Array` chunks of the Server-Sent Events reply, as
//! `response.body.getReader()` does.

use chanto_core::agent::{Agent, ByteStream, Event, LocalFuture, ModelConfig, Transport};
use chanto_core::fs::{Fs, MemFs};
use chanto_core::search::walk;
use chanto_core::tools::Workspace;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// One conversation with the agent, over one project.
#[wasm_bindgen]
pub struct Session {
    /// None while a turn runs: the turn owns it until it ends.
    agent: Rc<RefCell<Option<Agent>>>,
    fs: Rc<MemFs>,
    cwd: PathBuf,
    /// When each file was last read, kept between turns.
    read_times: Rc<RefCell<HashMap<PathBuf, SystemTime>>>,
}

#[wasm_bindgen]
impl Session {
    #[wasm_bindgen(constructor)]
    pub fn new(
        base_url: String,
        model: String,
        api_key: Option<String>,
        system_prompt: String,
        text_tools: bool,
        cwd: String,
    ) -> Session {
        let mut agent = Agent::new(
            ModelConfig {
                base_url,
                model,
                api_key,
            },
            system_prompt,
        );
        agent.text_tools = text_tools;
        let fs = MemFs::new();
        fs.mkdir_p(&cwd);
        Session {
            agent: Rc::new(RefCell::new(Some(agent))),
            fs: Rc::new(fs),
            cwd: cwd.into(),
            read_times: Default::default(),
        }
    }

    /// Model requests one turn may make (default 50).
    pub fn set_max_steps(&self, n: usize) {
        if let Some(a) = self.agent.borrow_mut().as_mut() {
            a.max_steps = n;
        }
    }

    pub fn write_file(&self, path: &str, text: &str) {
        self.fs.insert(path, text);
    }

    pub fn read_file(&self, path: &str) -> Option<String> {
        self.fs
            .read(Path::new(path))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Every file in the project (`.gitignore` honoured), sorted.
    pub fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk(
            self.fs.as_ref(),
            &self.cwd,
            None,
            usize::MAX,
            &|_| false,
            &mut |v| {
                if !v.is_dir {
                    out.push(v.path.display().to_string());
                }
                true
            },
        );
        out
    }

    /// The conversation so far, as the model sees it (JSON); empty while a
    /// turn runs.
    pub fn messages_json(&self) -> String {
        self.agent
            .borrow()
            .as_ref()
            .and_then(|a| serde_json::to_string(&a.messages).ok())
            .unwrap_or_default()
    }

    /// Answer `input`. `on_event` gets each event as a JSON string (see
    /// `chanto_core::agent::Event`); resolves to the final reply.
    pub fn run(
        &self,
        input: String,
        transport: js_sys::Function,
        on_event: js_sys::Function,
    ) -> js_sys::Promise {
        let (agent, fs, cwd, read_times) = (
            self.agent.clone(),
            self.fs.clone(),
            self.cwd.clone(),
            self.read_times.clone(),
        );
        wasm_bindgen_futures::future_to_promise(async move {
            let transport = JsTransport(transport);
            let mut workspace = Workspace::new(fs.as_ref(), cwd);
            workspace.read_times = std::mem::take(&mut *read_times.borrow_mut());
            let mut emit = |e: Event| {
                if let Ok(json) = serde_json::to_string(&e) {
                    let _ = on_event.call1(&JsValue::NULL, &JsValue::from_str(&json));
                }
            };
            let mut owned = agent
                .borrow_mut()
                .take()
                .ok_or_else(|| JsValue::from_str("a turn is already running"))?;
            let result = owned
                .run_turn(&input, &transport, &mut workspace, &mut emit)
                .await;
            *agent.borrow_mut() = Some(owned);
            *read_times.borrow_mut() = workspace.read_times;
            result
                .map(|answer| JsValue::from_str(&answer))
                .map_err(|e| JsValue::from_str(&format!("{e:#}")))
        })
    }
}

/// The page's `transport` function as the agent's network.
struct JsTransport(js_sys::Function);

impl Transport for JsTransport {
    fn post<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: Vec<u8>,
    ) -> LocalFuture<'a, anyhow::Result<ByteStream<'a>>> {
        Box::pin(async move {
            let headers = serde_json::to_string(headers)?;
            let body = js_sys::Uint8Array::from(body.as_slice());
            let promise = self
                .0
                .call3(
                    &JsValue::NULL,
                    &JsValue::from_str(url),
                    &JsValue::from_str(&headers),
                    &body,
                )
                .map_err(js_error)?;
            let reader = JsFuture::from(js_sys::Promise::resolve(&promise))
                .await
                .map_err(js_error)?;
            let read: js_sys::Function = js_sys::Reflect::get(&reader, &"read".into())
                .map_err(js_error)?
                .dyn_into()
                .map_err(|_| anyhow::anyhow!("the transport's reader has no read()"))?;
            let stream = futures::stream::unfold((reader, read), |(reader, read)| async move {
                let next = match read.call0(&reader) {
                    Ok(p) => JsFuture::from(js_sys::Promise::resolve(&p)).await,
                    Err(e) => Err(e),
                };
                let chunk = match next {
                    Err(e) => Err(js_error(e)),
                    Ok(r) => {
                        let done = js_sys::Reflect::get(&r, &"done".into())
                            .map(|d| d.is_truthy())
                            .unwrap_or(true);
                        if done {
                            return None;
                        }
                        js_sys::Reflect::get(&r, &"value".into())
                            .map(|v| js_sys::Uint8Array::new(&v).to_vec())
                            .map_err(js_error)
                    }
                };
                Some((chunk, (reader, read)))
            });
            let stream: ByteStream<'a> = Box::pin(stream);
            Ok(stream)
        })
    }
}

fn js_error(e: JsValue) -> anyhow::Error {
    anyhow::anyhow!(
        "{}",
        e.as_string()
            .or_else(|| js_sys::Reflect::get(&e, &"message".into())
                .ok()
                .and_then(|m| m.as_string()))
            .unwrap_or_else(|| format!("{e:?}"))
    )
}
