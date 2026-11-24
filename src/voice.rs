//! Voice input: dictate a prompt. Audio is recorded with `ffmpeg` (or sox's
//! `rec`) and transcribed by whisper.cpp on this machine, or by an
//! OpenAI-compatible `/audio/transcriptions` endpoint when `[voice]` names one.
//!
//! Audio never leaves the machine unless the config says where to send it:
//! the default backend is local, and the hosted one is used only when
//! `[voice] provider` or `base_url` is set (`voice` is a risky key, so an
//! untrusted project can't set it). Recordings are temp files deleted after use.

use crate::config::ProviderConfig;
use crate::providers::{Cred, Hosted as Catalog};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// `[voice]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct VoiceConfig {
    /// "auto" (default): whisper.cpp when installed, else the hosted endpoint
    /// when one is configured. "local", "hosted" or "off".
    pub backend: Option<String>,
    /// whisper.cpp binary (default: `whisper-cli` or `whisper-cpp` on PATH).
    pub whisper_bin: Option<String>,
    /// whisper.cpp model file (default: the first `~/.chanto/voice/ggml-*.bin`).
    pub whisper_model: Option<String>,
    /// Hosted: a built-in provider (`openai`, `groq`, ...) or a `[providers.<name>]` entry.
    pub provider: Option<String>,
    /// Hosted: an OpenAI-compatible base URL (`.../v1`); overrides the provider's.
    pub base_url: Option<String>,
    /// Hosted: API key (default: the provider's saved login or key variable).
    pub api_key: Option<String>,
    /// Hosted: transcription model (default: whisper-1 on OpenAI, whisper-large-v3-turbo on Groq).
    pub model: Option<String>,
    /// Spoken language hint, e.g. "en" (default: detected).
    pub language: Option<String>,
    /// Input device (ffmpeg: ":default" on macOS, "default" on Linux, the
    /// DirectShow device name on Windows).
    pub device: Option<String>,
    /// Stop after this many seconds of silence following speech (0 = only on the key).
    pub silence_secs: Option<f32>,
    /// Longest recording, in seconds (default 120).
    pub max_secs: Option<u64>,
}

impl VoiceConfig {
    pub fn merge(&mut self, o: VoiceConfig) {
        macro_rules! opt {
            ($($f:ident),*) => { $( if o.$f.is_some() { self.$f = o.$f; } )* };
        }
        opt!(
            backend,
            whisper_bin,
            whisper_model,
            provider,
            base_url,
            api_key,
            model,
            language,
            device,
            silence_secs,
            max_secs
        );
    }

    fn hosted_configured(&self) -> bool {
        self.provider.as_deref().is_some_and(|p| !p.is_empty())
            || self.base_url.as_deref().is_some_and(|u| !u.is_empty())
    }
}

pub const INSTALL_HELP: &str = "Install whisper.cpp for local transcription (`brew install whisper-cpp`, or build https://github.com/ggml-org/whisper.cpp) \
and download a model:\n  mkdir -p ~/.chanto/voice && curl -L -o ~/.chanto/voice/ggml-base.en.bin https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin\n\
Or send audio to a hosted endpoint you choose: `[voice] provider = \"openai\"` (or \"groq\", or base_url + api_key) in ~/.chanto/config.toml.";

/// Where speech becomes text.
#[derive(Debug, Clone, PartialEq)]
pub enum Backend {
    /// whisper.cpp (`model` is a ggml file) or OpenAI's Python `whisper` CLI
    /// (`python`; `model` is a model name).
    Local {
        bin: PathBuf,
        model: String,
        python: bool,
        language: Option<String>,
    },
    Hosted(HostedBackend),
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostedBackend {
    /// The full `/audio/transcriptions` URL.
    pub url: String,
    pub key: Key,
    pub model: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    None,
    Static(String),
    /// A browser login (`chanto login <id>`): fetched per request.
    Login(String),
}

impl Backend {
    pub fn describe(&self) -> String {
        match self {
            Backend::Local { bin, .. } => format!(
                "local ({})",
                bin.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("whisper")
            ),
            Backend::Hosted(h) => format!("hosted ({})", h.url.split('/').nth(2).unwrap_or(&h.url)),
        }
    }
}

/// What backend selection can see of the machine (injectable for tests).
pub struct Probe<'a> {
    pub which: &'a dyn Fn(&str) -> Option<PathBuf>,
    pub credential: &'a dyn Fn(&Catalog) -> Option<Cred>,
    /// Where downloaded whisper.cpp models are looked for.
    pub models_dir: PathBuf,
}

impl Probe<'static> {
    pub fn system() -> Probe<'static> {
        Probe {
            which: &find_tool,
            credential: &crate::providers::credential,
            models_dir: crate::config::home_dir().join("voice"),
        }
    }
}

/// A program on PATH, or in the usual install prefixes (apps started from
/// the Dock or Finder get a minimal PATH without Homebrew's).
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() || name.contains('/') {
        return p.is_file().then(|| p.to_path_buf());
    }
    crate::shell::find_on_path(name).or_else(|| {
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
            .iter()
            .map(|d| Path::new(d).join(name))
            .find(|c| c.is_file())
    })
}

fn local(cfg: &VoiceConfig, probe: &Probe) -> Result<Backend, String> {
    let bin = match &cfg.whisper_bin {
        Some(b) => {
            (probe.which)(b).ok_or_else(|| format!("[voice] whisper_bin `{b}` was not found"))?
        }
        None => ["whisper-cli", "whisper-cpp", "whisper"]
            .iter()
            .find_map(|n| (probe.which)(n))
            .ok_or("whisper.cpp (`whisper-cli`) is not installed")?,
    };
    let python = bin.file_stem().is_some_and(|s| s == "whisper");
    let model = if python {
        cfg.whisper_model.clone().unwrap_or_else(|| "base".into())
    } else {
        match &cfg.whisper_model {
            Some(m) => {
                let path = crate::util::resolve(&std::env::current_dir().unwrap_or_default(), m);
                if !path.is_file() {
                    return Err(format!("[voice] whisper_model `{m}` doesn't exist"));
                }
                path.display().to_string()
            }
            None => {
                let mut found: Vec<PathBuf> = std::fs::read_dir(&probe.models_dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("ggml-") && n.ends_with(".bin"))
                    })
                    .collect();
                found.sort();
                found
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        format!("no whisper.cpp model in {}", probe.models_dir.display())
                    })?
                    .display()
                    .to_string()
            }
        }
    };
    Ok(Backend::Local {
        bin,
        model,
        python,
        language: cfg.language.clone(),
    })
}

fn hosted(
    cfg: &VoiceConfig,
    providers: &BTreeMap<String, ProviderConfig>,
    probe: &Probe,
) -> Result<Backend, String> {
    let provider = cfg.provider.as_deref().filter(|p| !p.is_empty());
    let custom = provider.and_then(|p| providers.get(p));
    let catalog = provider.and_then(crate::providers::find);
    let base = match (&cfg.base_url, custom, catalog) {
        (Some(u), _, _) if !u.is_empty() => u.clone(),
        (_, Some(c), _) => c.base_url.clone(),
        (_, _, Some(h)) => h.base_url.into(),
        _ => {
            return Err(match provider {
                Some(p) => format!(
                    "[voice] provider `{p}` is neither a built-in provider nor a [providers.{p}] entry"
                ),
                None => "no hosted transcription endpoint is configured".into(),
            });
        }
    };
    let key = if let Some(k) = cfg.api_key.clone().filter(|k| !k.is_empty()) {
        Key::Static(k)
    } else if let Some(k) = custom.and_then(|c| c.api_key.clone()) {
        Key::Static(k)
    } else if let Some(h) = catalog {
        match (probe.credential)(h) {
            Some(Cred::Key(k)) => Key::Static(k),
            Some(Cred::Login) => Key::Login(h.id.into()),
            // Cloud platforms sign requests their own way and have no
            // OpenAI-style transcription endpoint here.
            Some(Cred::Cloud(_)) => {
                return Err(format!(
                    "{} has no OpenAI-compatible /audio/transcriptions endpoint; set [voice] base_url and api_key for one, or use local whisper",
                    h.name
                ));
            }
            None => {
                return Err(format!(
                    "no API key for {}: set {} or run `chanto login {}`",
                    h.name,
                    h.env.first().unwrap_or(&"[voice] api_key"),
                    h.id
                ));
            }
        }
    } else {
        Key::None
    };
    let model = match (&cfg.model, provider) {
        (Some(m), _) => m.clone(),
        (None, Some("openai" | "chatgpt")) => "whisper-1".into(),
        (None, Some("groq")) => "whisper-large-v3-turbo".into(),
        (None, _)
            if cfg
                .base_url
                .as_deref()
                .is_some_and(|u| u.contains("api.openai.com")) =>
        {
            "whisper-1".into()
        }
        _ => return Err("set [voice] model to the endpoint's transcription model".into()),
    };
    Ok(Backend::Hosted(HostedBackend {
        url: format!("{}/audio/transcriptions", base.trim_end_matches('/')),
        key,
        model,
        language: cfg.language.clone(),
    }))
}

/// Pick the backend: local first; hosted only when configured.
pub fn select(
    cfg: &VoiceConfig,
    providers: &BTreeMap<String, ProviderConfig>,
    probe: &Probe,
) -> Result<Backend, String> {
    match cfg.backend.as_deref().unwrap_or("auto") {
        "off" => Err("voice input is off ([voice] backend = \"off\")".into()),
        "local" => local(cfg, probe).map_err(|e| format!("{e}.\n{INSTALL_HELP}")),
        "hosted" => hosted(cfg, providers, probe),
        "auto" | "" => match local(cfg, probe) {
            Ok(b) => Ok(b),
            Err(_) if cfg.hosted_configured() => hosted(cfg, providers, probe),
            Err(e) => Err(format!("{e}.\n{INSTALL_HELP}")),
        },
        other => Err(format!(
            "unknown [voice] backend `{other}` (auto, local, hosted or off)"
        )),
    }
}

/// The backend for this machine and config.
pub fn backend(config: &crate::config::Config) -> Result<Backend, String> {
    select(&config.voice, &config.providers, &Probe::system())
}

// ------------------------------------------------------------- temp audio

/// A temp audio file, deleted when dropped.
pub struct TempAudio(PathBuf);

impl TempAudio {
    pub fn new(ext: &str) -> Result<TempAudio> {
        let path = std::env::temp_dir().join(format!(
            "chanto-voice-{}.{ext}",
            uuid::Uuid::new_v4().simple()
        ));
        // Owner-only from the start; the recorder overwrites it in place.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        opts.open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(TempAudio(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempAudio {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// --------------------------------------------------------------- recording

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Os {
    Mac,
    Linux { pulse: bool },
    Windows,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::Mac
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux {
                pulse: find_tool("pactl").is_some(),
            }
        }
    }
}

/// ffmpeg arguments that record 16 kHz mono WAV from the default input.
/// `silence`: log silence (for stopping after speech ends).
pub fn ffmpeg_args(
    os: Os,
    device: Option<&str>,
    out: &Path,
    max_secs: u64,
    silence: Option<f32>,
) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-nostats", "-loglevel", "info"]
        .map(String::from)
        .to_vec();
    let (format, dev) = match os {
        Os::Mac => (
            "avfoundation",
            device
                .map(|d| {
                    if d.contains(':') {
                        d.to_string()
                    } else {
                        format!(":{d}")
                    }
                })
                .unwrap_or_else(|| ":default".into()),
        ),
        Os::Linux { pulse } => (
            if pulse { "pulse" } else { "alsa" },
            device.unwrap_or("default").to_string(),
        ),
        Os::Windows => (
            "dshow",
            device
                .map(|d| {
                    if d.starts_with("audio=") {
                        d.to_string()
                    } else {
                        format!("audio={d}")
                    }
                })
                .unwrap_or_default(),
        ),
    };
    a.extend(["-f".into(), format.into(), "-i".into(), dev]);
    a.extend([
        "-t".into(),
        max_secs.to_string(),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        "16000".into(),
        "-c:a".into(),
        "pcm_s16le".into(),
    ]);
    if let Some(s) = silence.filter(|s| *s > 0.0) {
        a.extend(["-af".into(), format!("silencedetect=noise=-35dB:d={s}")]);
    }
    a.extend(["-y".into(), out.display().to_string()]);
    a
}

/// sox's `rec`: the same format, stopping on silence after speech with sox's own effect.
pub fn sox_args(out: &Path, max_secs: u64, silence: Option<f32>) -> Vec<String> {
    let mut a: Vec<String> = ["-q", "-c", "1", "-r", "16000", "-b", "16"]
        .map(String::from)
        .to_vec();
    a.push(out.display().to_string());
    a.extend(["trim".into(), "0".into(), max_secs.to_string()]);
    if let Some(s) = silence.filter(|s| *s > 0.0) {
        a.extend(["silence", "1", "0.1", "2%", "1"].map(String::from));
        a.extend([format!("{s}"), "2%".into()]);
    }
    a
}

/// The first audio device in `ffmpeg -list_devices true -f dshow -i dummy` output.
pub fn parse_dshow_audio(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .find(|l| l.contains("(audio)"))
        .and_then(|l| {
            let start = l.find('"')? + 1;
            let end = start + l[start..].find('"')?;
            Some(l[start..end].to_string())
        })
}

/// Watches ffmpeg's silencedetect log: true once silence follows speech.
#[derive(Default)]
pub struct SilenceWatch {
    heard: bool,
}

impl SilenceWatch {
    pub fn line(&mut self, line: &str) -> bool {
        if line.contains("silence_end:") {
            self.heard = true;
            return false;
        }
        if let Some(rest) = line.split("silence_start:").nth(1) {
            let at: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0.0);
            // Silence from the very start is the pause before speaking.
            return self.heard || at > 0.5;
        }
        false
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Recorder {
    Ffmpeg(PathBuf),
    Sox(PathBuf),
}

pub fn find_recorder() -> Result<Recorder> {
    pick_recorder(find_tool)
}

fn pick_recorder(find_tool: impl Fn(&str) -> Option<PathBuf>) -> Result<Recorder> {
    if let Some(p) = find_tool("ffmpeg") {
        return Ok(Recorder::Ffmpeg(p));
    }
    if let Some(p) = find_tool("rec").filter(|_| find_tool("sox").is_some()) {
        return Ok(Recorder::Sox(p));
    }
    bail!(
        "voice input needs ffmpeg (or sox) to record: `brew install ffmpeg`, `apt install ffmpeg`, or `winget install ffmpeg`"
    )
}

/// A recording in progress.
pub struct Recording {
    child: tokio::process::Child,
    audio: TempAudio,
    started: Instant,
    sox: bool,
    silence: Arc<AtomicBool>,
    log: Arc<std::sync::Mutex<String>>,
    /// Copies the recorder's stderr into `log`; it ends when the recorder does.
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl Recording {
    pub async fn start(cfg: &VoiceConfig) -> Result<Recording> {
        Self::start_with(find_recorder()?, Os::current(), cfg).await
    }

    pub async fn start_with(recorder: Recorder, os: Os, cfg: &VoiceConfig) -> Result<Recording> {
        let audio = TempAudio::new("wav")?;
        let max = cfg.max_secs.unwrap_or(120).max(1);
        let silence = Some(cfg.silence_secs.unwrap_or(2.0));
        let (mut cmd, sox) = match &recorder {
            Recorder::Ffmpeg(bin) => {
                let mut device = cfg.device.clone();
                if os == Os::Windows && device.is_none() {
                    let out = tokio::process::Command::new(bin)
                        .args([
                            "-hide_banner",
                            "-list_devices",
                            "true",
                            "-f",
                            "dshow",
                            "-i",
                            "dummy",
                        ])
                        .output()
                        .await?;
                    device = Some(
                        parse_dshow_audio(&String::from_utf8_lossy(&out.stderr)).ok_or_else(
                            || anyhow::anyhow!("no microphone found; set [voice] device"),
                        )?,
                    );
                }
                let mut c = tokio::process::Command::new(bin);
                c.args(ffmpeg_args(
                    os,
                    device.as_deref(),
                    audio.path(),
                    max,
                    silence,
                ));
                (c, false)
            }
            Recorder::Sox(bin) => {
                let mut c = tokio::process::Command::new(bin);
                c.args(sox_args(audio.path(), max, silence));
                (c, true)
            }
        };
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting the recorder")?;
        let flag = Arc::new(AtomicBool::new(false));
        let log = Arc::new(std::sync::Mutex::new(String::new()));
        let reader = child.stderr.take().map(|err| {
            let (flag, log) = (flag.clone(), log.clone());
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(err).lines();
                let mut watch = SilenceWatch::default();
                while let Ok(Some(l)) = lines.next_line().await {
                    if watch.line(&l) {
                        flag.store(true, Ordering::Relaxed);
                    }
                    let mut log = log.lock().unwrap();
                    if log.len() < 16_000 {
                        log.push_str(&l);
                        log.push('\n');
                    }
                }
            })
        });
        Ok(Recording {
            child,
            audio,
            started: Instant::now(),
            sox,
            silence: flag,
            log,
            reader,
        })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Speech was followed by silence, or the recorder stopped on its own
    /// (time limit, sox's silence effect, a device error).
    pub fn should_stop(&mut self) -> bool {
        self.silence.load(Ordering::Relaxed) || matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Stop and return the audio. ffmpeg is asked to quit (`q`) so it writes a
    /// complete file; sox gets an interrupt.
    pub async fn stop(mut self) -> Result<TempAudio> {
        if !self.sox {
            if let Some(mut stdin) = self.child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(b"q").await;
                let _ = stdin.flush().await;
            }
        } else {
            #[cfg(unix)]
            if let Some(pid) = self.child.id() {
                unsafe { libc::kill(pid as i32, libc::SIGINT) };
            }
        }
        let status = match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(s) => s.ok(),
            Err(_) => {
                let _ = self.child.kill().await;
                None
            }
        };
        // The recorder's last words (often the reason it failed) may still be
        // on their way into the log. Something it started can hold the pipe
        // open, so don't wait long.
        if let Some(reader) = self.reader.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), reader).await;
        }
        let size = std::fs::metadata(self.audio.path())
            .map(|m| m.len())
            .unwrap_or(0);
        // A WAV header alone is 44 bytes.
        if size <= 44 {
            let log = self.log.lock().unwrap().clone();
            let reason = log
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no audio was captured")
                .to_string();
            let hint = if cfg!(target_os = "macos") {
                " (check that your terminal has microphone access in System Settings → Privacy & Security → Microphone)"
            } else {
                ""
            };
            bail!(
                "recording failed{}: {reason}{hint}",
                status
                    .and_then(|s| s.code())
                    .map(|c| format!(" (exit {c})"))
                    .unwrap_or_default()
            );
        }
        Ok(self.audio)
    }
}

/// Record for `secs` seconds.
#[cfg(test)]
pub async fn record_for(cfg: &VoiceConfig, secs: f32) -> Result<TempAudio> {
    let cfg = VoiceConfig {
        silence_secs: Some(0.0),
        ..cfg.clone()
    };
    let rec = Recording::start(&cfg).await?;
    tokio::time::sleep(Duration::from_secs_f32(secs)).await;
    rec.stop().await
}

// ------------------------------------------------------------ transcribing

/// Convert any audio ffmpeg reads (webm/opus, mp4/aac from browsers) to
/// 16 kHz mono WAV, which whisper.cpp needs.
pub async fn to_wav(input: &Path) -> Result<TempAudio> {
    let Some(ffmpeg) = find_tool("ffmpeg") else {
        bail!("converting audio for whisper.cpp needs ffmpeg")
    };
    let out = TempAudio::new("wav")?;
    let o = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(input)
        .args(["-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", "-y"])
        .arg(out.path())
        .output()
        .await?;
    if !o.status.success() {
        bail!(
            "could not decode the audio: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(out)
}

/// Transcribe a recording. Non-WAV input is converted first for local backends.
pub async fn transcribe(backend: &Backend, audio: &Path) -> Result<String> {
    match backend {
        Backend::Local {
            bin,
            model,
            python,
            language,
        } => {
            let converted = if audio.extension().is_some_and(|e| e == "wav") {
                None
            } else {
                Some(to_wav(audio).await?)
            };
            let wav = converted.as_ref().map(|t| t.path()).unwrap_or(audio);
            let text = if *python {
                python_whisper(bin, model, language.as_deref(), wav).await?
            } else {
                whisper_cpp(bin, model, language.as_deref(), wav).await?
            };
            Ok(text)
        }
        Backend::Hosted(h) => {
            let bytes = std::fs::read(audio)?;
            let ext = audio.extension().and_then(|e| e.to_str()).unwrap_or("wav");
            hosted_transcribe(h, bytes, ext).await
        }
    }
}

pub fn whisper_cpp_args(model: &str, language: Option<&str>, wav: &Path) -> Vec<String> {
    vec![
        "-m".into(),
        model.into(),
        "-f".into(),
        wav.display().to_string(),
        "-nt".into(),
        "-np".into(),
        "-l".into(),
        language.unwrap_or("auto").into(),
    ]
}

/// whisper.cpp prints the text (no timestamps with -nt) on stdout.
pub fn clean_transcript(stdout: &str) -> String {
    let text = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    // Markers for silence/noise, e.g. "[BLANK_AUDIO]" or "(wind blowing)".
    let marker = {
        static RE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"\[[A-Z_ ]+\]|\([a-z ]+\)").unwrap());
        &*RE
    };
    marker
        .replace_all(&text, "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

async fn whisper_cpp(
    bin: &Path,
    model: &str,
    language: Option<&str>,
    wav: &Path,
) -> Result<String> {
    let o = tokio::process::Command::new(bin)
        .args(whisper_cpp_args(model, language, wav))
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {}", bin.display()))?;
    if !o.status.success() {
        bail!(
            "{} failed: {}",
            bin.display(),
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .last()
                .unwrap_or("")
                .trim()
        );
    }
    Ok(clean_transcript(&String::from_utf8_lossy(&o.stdout)))
}

async fn python_whisper(
    bin: &Path,
    model: &str,
    language: Option<&str>,
    wav: &Path,
) -> Result<String> {
    let dir = std::env::temp_dir().join(format!("chanto-voice-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir)?;
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(wav)
        .args([
            "--model",
            model,
            "--output_format",
            "txt",
            "--fp16",
            "False",
            "--output_dir",
        ])
        .arg(&dir);
    if let Some(l) = language {
        cmd.args(["--language", l]);
    }
    let o = cmd.stdin(std::process::Stdio::null()).output().await;
    let txt = dir
        .join(wav.file_stem().unwrap_or_default())
        .with_extension("txt");
    let text = std::fs::read_to_string(&txt);
    let _ = std::fs::remove_dir_all(&dir);
    let o = o.with_context(|| format!("running {}", bin.display()))?;
    match text {
        Ok(t) if o.status.success() => Ok(clean_transcript(&t)),
        _ => bail!(
            "{} failed: {}",
            bin.display(),
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .last()
                .unwrap_or("")
                .trim()
        ),
    }
}

fn mime_for(ext: &str) -> &'static str {
    match ext {
        "webm" => "audio/webm",
        "ogg" | "oga" => "audio/ogg",
        "mp4" | "m4a" => "audio/mp4",
        "mp3" => "audio/mpeg",
        _ => "audio/wav",
    }
}

/// A `multipart/form-data` body (reqwest's multipart feature isn't built in).
pub fn multipart(
    boundary: &str,
    fields: &[(&str, &str)],
    file_name: &str,
    mime: &str,
    bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 512);
    for (name, value) in fields {
        body.extend(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: {mime}\r\n\r\n").as_bytes());
    body.extend(bytes);
    body.extend(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// The `/audio/transcriptions` request for `bytes` (audio of type `ext`).
pub fn hosted_request(
    client: &reqwest::Client,
    h: &HostedBackend,
    key: Option<&str>,
    bytes: Vec<u8>,
    ext: &str,
) -> reqwest::RequestBuilder {
    let boundary = format!("chanto-{}", uuid::Uuid::new_v4().simple());
    let mut fields = vec![("model", h.model.as_str()), ("response_format", "json")];
    if let Some(l) = &h.language {
        fields.push(("language", l));
    }
    let body = multipart(
        &boundary,
        &fields,
        &format!("audio.{ext}"),
        mime_for(ext),
        &bytes,
    );
    let mut req = client
        .post(&h.url)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body);
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    req
}

pub async fn hosted_transcribe(h: &HostedBackend, bytes: Vec<u8>, ext: &str) -> Result<String> {
    let key = match &h.key {
        Key::None => None,
        Key::Static(k) => Some(k.clone()),
        Key::Login(id) => Some(crate::auth::token(id).await?),
    };
    let client = crate::http::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let r = hosted_request(&client, h, key.as_deref(), bytes, ext)
        .send()
        .await
        .with_context(|| format!("contacting {}", h.url))?;
    let status = r.status();
    let v: serde_json::Value = r.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "transcription failed ({status}): {}",
            v["error"]["message"]
                .as_str()
                .or(v["error"].as_str())
                .unwrap_or("no details")
        );
    }
    Ok(clean_transcript(v["text"].as_str().unwrap_or_default()))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod recording_tests;
