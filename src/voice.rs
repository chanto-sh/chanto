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
        if let Some(err) = child.stderr.take() {
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
            });
        }
        Ok(Recording {
            child,
            audio,
            started: Instant::now(),
            sox,
            silence: flag,
            log,
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
mod tests {
    use super::*;

    fn probe<'a>(
        which: &'a dyn Fn(&str) -> Option<PathBuf>,
        cred: &'a dyn Fn(&Catalog) -> Option<Cred>,
        dir: &Path,
    ) -> Probe<'a> {
        Probe {
            which,
            credential: cred,
            models_dir: dir.to_path_buf(),
        }
    }

    fn models_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "chanto-voice-models-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn backend_selection() {
        let dir = models_dir();
        let none = |_: &str| None;
        let cli = |n: &str| (n == "whisper-cli").then(|| PathBuf::from("/opt/bin/whisper-cli"));
        let no_cred = |_: &Catalog| None;
        let key = |_: &Catalog| Some(Cred::Key("sk-test".into()));
        let providers = BTreeMap::new();
        let auto = VoiceConfig::default();

        // Nothing installed and nothing configured: no audio goes anywhere.
        let e = select(&auto, &providers, &probe(&none, &key, &dir)).unwrap_err();
        assert!(e.contains("brew install whisper-cpp"), "{e}");
        // A binary without a model is not enough.
        assert!(
            select(&auto, &providers, &probe(&cli, &key, &dir))
                .unwrap_err()
                .contains("no whisper.cpp model")
        );
        std::fs::write(dir.join("ggml-base.en.bin"), b"x").unwrap();
        let local = select(&auto, &providers, &probe(&cli, &no_cred, &dir)).unwrap();
        assert!(
            matches!(&local, Backend::Local { model, python: false, .. } if model.ends_with("ggml-base.en.bin"))
        );
        // Local wins over a configured hosted endpoint in auto mode...
        let groq = VoiceConfig {
            provider: Some("groq".into()),
            ..Default::default()
        };
        assert!(matches!(
            select(&groq, &providers, &probe(&cli, &key, &dir)).unwrap(),
            Backend::Local { .. }
        ));
        // ...which is the fallback without whisper.cpp.
        let Backend::Hosted(h) = select(&groq, &providers, &probe(&none, &key, &dir)).unwrap()
        else {
            panic!()
        };
        assert_eq!(h.url, "https://api.groq.com/openai/v1/audio/transcriptions");
        assert_eq!(h.model, "whisper-large-v3-turbo");
        assert_eq!(h.key, Key::Static("sk-test".into()));
        // A hosted provider without a key says how to add one.
        assert!(
            select(&groq, &providers, &probe(&none, &no_cred, &dir))
                .unwrap_err()
                .contains("GROQ_API_KEY")
        );
        // Forced backends.
        let forced_hosted = VoiceConfig {
            backend: Some("hosted".into()),
            provider: Some("openai".into()),
            ..Default::default()
        };
        let Backend::Hosted(h) =
            select(&forced_hosted, &providers, &probe(&cli, &key, &dir)).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (h.url.as_str(), h.model.as_str()),
            (
                "https://api.openai.com/v1/audio/transcriptions",
                "whisper-1"
            )
        );
        assert!(
            select(
                &VoiceConfig {
                    backend: Some("hosted".into()),
                    ..Default::default()
                },
                &providers,
                &probe(&cli, &key, &dir)
            )
            .is_err()
        );
        assert!(
            select(
                &VoiceConfig {
                    backend: Some("local".into()),
                    ..groq.clone()
                },
                &providers,
                &probe(&none, &key, &dir)
            )
            .unwrap_err()
            .contains("whisper")
        );
        assert!(
            select(
                &VoiceConfig {
                    backend: Some("off".into()),
                    ..Default::default()
                },
                &providers,
                &probe(&cli, &key, &dir)
            )
            .is_err()
        );
        assert!(
            select(
                &VoiceConfig {
                    backend: Some("cloud".into()),
                    ..Default::default()
                },
                &providers,
                &probe(&cli, &key, &dir)
            )
            .is_err()
        );
        // A [providers.<name>] entry and an explicit base URL.
        let providers = BTreeMap::from([(
            "local-ai".to_string(),
            ProviderConfig {
                base_url: "http://127.0.0.1:8080/v1/".into(),
                api_key: Some("k".into()),
                ..Default::default()
            },
        )]);
        let named = VoiceConfig {
            provider: Some("local-ai".into()),
            model: Some("whisper".into()),
            ..Default::default()
        };
        let Backend::Hosted(h) = select(&named, &providers, &probe(&none, &no_cred, &dir)).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (h.url.as_str(), &h.key),
            (
                "http://127.0.0.1:8080/v1/audio/transcriptions",
                &Key::Static("k".into())
            )
        );
        assert!(
            select(
                &VoiceConfig {
                    model: None,
                    ..named
                },
                &providers,
                &probe(&none, &no_cred, &dir)
            )
            .unwrap_err()
            .contains("model")
        );
        let url = VoiceConfig {
            base_url: Some("http://h/v1".into()),
            model: Some("m".into()),
            ..Default::default()
        };
        let Backend::Hosted(h) = select(&url, &providers, &probe(&none, &no_cred, &dir)).unwrap()
        else {
            panic!()
        };
        assert_eq!(h.key, Key::None);
        // A configured binary and model path; Python's `whisper` takes a model name.
        let py = |n: &str| (n == "whisper").then(|| PathBuf::from("/usr/bin/whisper"));
        let b = select(
            &VoiceConfig {
                whisper_bin: Some("whisper".into()),
                ..Default::default()
            },
            &BTreeMap::new(),
            &probe(&py, &no_cred, &dir),
        )
        .unwrap();
        assert!(matches!(b, Backend::Local { python: true, ref model, .. } if model == "base"));
        assert!(
            select(
                &VoiceConfig {
                    whisper_model: Some("/nope.bin".into()),
                    ..Default::default()
                },
                &BTreeMap::new(),
                &probe(&cli, &no_cred, &dir)
            )
            .is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ffmpeg_commands_per_os() {
        let out = Path::new("/tmp/a.wav");
        let mac = ffmpeg_args(Os::Mac, None, out, 60, Some(2.0));
        let s = mac.join(" ");
        assert!(s.contains("-f avfoundation -i :default"), "{s}");
        assert!(s.contains("-t 60 -ac 1 -ar 16000 -c:a pcm_s16le"));
        assert!(s.contains("-af silencedetect=noise=-35dB:d=2"));
        assert!(s.ends_with("-y /tmp/a.wav"));
        assert!(
            ffmpeg_args(Os::Mac, Some("1"), out, 60, None)
                .join(" ")
                .contains("-i :1")
        );
        let linux = ffmpeg_args(Os::Linux { pulse: true }, None, out, 60, Some(0.0)).join(" ");
        assert!(linux.contains("-f pulse -i default") && !linux.contains("silencedetect"));
        assert!(
            ffmpeg_args(Os::Linux { pulse: false }, Some("hw:1"), out, 60, None)
                .join(" ")
                .contains("-f alsa -i hw:1")
        );
        let win = ffmpeg_args(Os::Windows, Some("Microphone (USB)"), out, 60, None);
        assert!(
            win.windows(2)
                .any(|w| w[0] == "-i" && w[1] == "audio=Microphone (USB)")
        );
        assert_eq!(
            parse_dshow_audio(
                "[dshow @ 0x1] \"Webcam\" (video)\n[dshow @ 0x1] \"Microphone (Realtek)\" (audio)\n"
            ),
            Some("Microphone (Realtek)".into())
        );
        assert_eq!(parse_dshow_audio("nothing"), None);
        let sox = sox_args(out, 30, Some(1.5)).join(" ");
        assert!(
            sox.starts_with("-q -c 1 -r 16000 -b 16 /tmp/a.wav trim 0 30 silence"),
            "{sox}"
        );
    }

    #[test]
    fn silence_after_speech_stops() {
        let mut w = SilenceWatch::default();
        // The pause before speaking doesn't stop the recording.
        assert!(!w.line("[silencedetect @ 0x1] silence_start: 0"));
        assert!(!w.line("[silencedetect @ 0x1] silence_end: 2.4 | silence_duration: 2.4"));
        assert!(w.line("[silencedetect @ 0x1] silence_start: 5.1"));
        assert!(SilenceWatch::default().line("silence_start: 3.2"));
        assert!(!SilenceWatch::default().line("size=  12kB time=00:00:01"));
    }

    #[test]
    fn temp_audio_is_removed_on_drop() {
        let t = TempAudio::new("wav").unwrap();
        let p = t.path().to_path_buf();
        assert!(p.exists());
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&p).unwrap().permissions())
                & 0o777,
            0o600
        );
        drop(t);
        assert!(!p.exists());
    }

    #[test]
    fn transcripts_are_cleaned() {
        assert_eq!(
            clean_transcript("\n  Hello there.\n  How are you? \n"),
            "Hello there. How are you?"
        );
        assert_eq!(clean_transcript("[BLANK_AUDIO]\n"), "");
        assert_eq!(
            clean_transcript(" (wind blowing) fix the tests"),
            "fix the tests"
        );
        assert_eq!(
            whisper_cpp_args("m.bin", None, Path::new("a.wav")).join(" "),
            "-m m.bin -f a.wav -nt -np -l auto"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_backend_runs_whisper_cpp() {
        // A stand-in whisper-cli that prints what it was given.
        let dir = models_dir();
        let bin = dir.join("whisper-cli");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho \"\"\necho \" heard $2 $4 $8 [BLANK_AUDIO]\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let audio = TempAudio::new("wav").unwrap();
        let b = Backend::Local {
            bin,
            model: "model.bin".into(),
            python: false,
            language: Some("en".into()),
        };
        let text = transcribe(&b, audio.path()).await.unwrap();
        assert_eq!(
            text,
            format!("heard model.bin {} en", audio.path().display())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn hosted_request_reaches_endpoint() {
        use axum::extract::Request;
        let seen = Arc::new(std::sync::Mutex::new(None::<(String, String, Vec<u8>)>));
        let s2 = seen.clone();
        let app = axum::Router::new()
            .route(
                "/v1/audio/transcriptions",
                axum::routing::post(move |req: Request| {
                    let seen = s2.clone();
                    async move {
                        let auth = req
                            .headers()
                            .get("authorization")
                            .and_then(|h| h.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let ct = req
                            .headers()
                            .get("content-type")
                            .and_then(|h| h.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                            .await
                            .unwrap()
                            .to_vec();
                        *seen.lock().unwrap() = Some((auth, ct, body));
                        axum::Json(serde_json::json!({"text": " Fix the flaky test. "}))
                    }
                }),
            )
            .route(
                "/bad/audio/transcriptions",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::UNAUTHORIZED,
                        axum::Json(serde_json::json!({"error": {"message": "bad key"}})),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let h = HostedBackend {
            url: format!("{base}/v1/audio/transcriptions"),
            key: Key::Static("sk-1".into()),
            model: "whisper-1".into(),
            language: Some("en".into()),
        };
        let text = hosted_transcribe(&h, b"RIFFdata".to_vec(), "webm")
            .await
            .unwrap();
        assert_eq!(text, "Fix the flaky test.");
        let (auth, ct, body) = seen.lock().unwrap().take().unwrap();
        assert_eq!(auth, "Bearer sk-1");
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n"
        )));
        assert!(body.contains("name=\"language\"\r\n\r\nen\r\n"));
        assert!(
            body.contains(
                "filename=\"audio.webm\"\r\nContent-Type: audio/webm\r\n\r\nRIFFdata\r\n"
            )
        );
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
        let bad = HostedBackend {
            url: format!("{base}/bad/audio/transcriptions"),
            ..h
        };
        assert!(
            hosted_transcribe(&bad, vec![1], "wav")
                .await
                .unwrap_err()
                .to_string()
                .contains("bad key")
        );
        task.abort();
    }

    /// Records a second from the default microphone (`CHANTO_VOICE_TEST=1`).
    #[tokio::test]
    async fn records_a_second_with_ffmpeg() {
        if std::env::var("CHANTO_VOICE_TEST").as_deref() != Ok("1") {
            return;
        }
        let audio = record_for(&VoiceConfig::default(), 1.0).await.unwrap();
        let path = audio.path().to_path_buf();
        assert!(std::fs::metadata(&path).unwrap().len() > 44);
        drop(audio);
        assert!(!path.exists());
    }
}
