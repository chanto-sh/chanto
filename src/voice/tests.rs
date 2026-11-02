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
    let Backend::Hosted(h) = select(&groq, &providers, &probe(&none, &key, &dir)).unwrap() else {
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
    let Backend::Hosted(h) = select(&forced_hosted, &providers, &probe(&cli, &key, &dir)).unwrap()
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
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
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
        body.contains("filename=\"audio.webm\"\r\nContent-Type: audio/webm\r\n\r\nRIFFdata\r\n")
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
