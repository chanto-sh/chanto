//! Recording and local transcription with shims standing in for ffmpeg, sox
//! and whisper, so no microphone is opened and no model runs.

use super::*;
#[cfg(unix)]
use crate::agent::tests::tmpdir;

#[cfg(unix)]
fn cfg() -> VoiceConfig {
    VoiceConfig::default()
}

#[test]
fn backends_describe_themselves_without_their_secrets() {
    let local = Backend::Local {
        bin: PathBuf::from("/opt/bin/whisper-cli"),
        model: "m".into(),
        python: false,
        language: None,
    };
    assert_eq!(local.describe(), "local (whisper-cli)");
    let hosted = Backend::Hosted(HostedBackend {
        url: "https://api.example.test/v1/audio/transcriptions".into(),
        key: Key::Static("sk-not-shown".into()),
        model: "m".into(),
        language: None,
    });
    assert_eq!(hosted.describe(), "hosted (api.example.test)");
}

#[test]
fn silence_only_counts_after_speech_or_a_long_pause() {
    let mut w = SilenceWatch::default();
    assert!(!w.line("silence_start: 0.1"), "the pause before speaking");
    assert!(w.line("silence_start: 0.6"));
    let mut w = SilenceWatch::default();
    assert!(!w.line("silence_end: 2.0"));
    assert!(w.line("silence_start: 0.1"), "silence after speech");
    let mut w = SilenceWatch::default();
    assert!(!w.line("silence_start: junk"));
    assert!(!w.line("other output"));
}

#[cfg(unix)]
mod shims {
    use super::*;
    use crate::test_support::{isolated, shim};

    fn wav_with_data() -> TempAudio {
        let a = TempAudio::new("wav").unwrap();
        std::fs::write(a.path(), vec![0u8; 100]).unwrap();
        a
    }

    #[tokio::test]
    async fn ffmpeg_is_asked_to_quit_so_it_finishes_the_file() {
        let dir = tmpdir("voice-ffmpeg");
        let (bin, got) = (dir.join("ffmpeg"), dir.join("stdin"));
        shim(
            &bin,
            &format!(
                "[ $# -eq 0 ] && exit 0\nfor a; do last=$a; done\ndd bs=1 count=1 of='{}' 2>/dev/null\nhead -c 100 /dev/zero > \"$last\"",
                got.display()
            ),
        );
        let rec = Recording::start_with(Recorder::Ffmpeg(bin), Os::Linux { pulse: true }, &cfg())
            .await
            .unwrap();
        let audio = rec.stop().await.unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), b"q");
        assert_eq!(std::fs::metadata(audio.path()).unwrap().len(), 100);
    }

    #[tokio::test]
    async fn a_recorder_that_captures_nothing_explains_why() {
        let dir = tmpdir("voice-fail");
        let bin = dir.join("ffmpeg");
        shim(
            &bin,
            "[ $# -eq 0 ] && exit 0\necho 'first line' >&2\necho 'Device busy' >&2\necho >&2\nexit 3",
        );
        let mut rec =
            Recording::start_with(Recorder::Ffmpeg(bin), Os::Linux { pulse: false }, &cfg())
                .await
                .unwrap();
        for _ in 0..500 {
            if rec.should_stop() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(rec.should_stop(), "the recorder exited on its own");
        assert!(rec.elapsed() < Duration::from_secs(60));
        let e = rec.stop().await.err().unwrap().to_string();
        assert!(
            e.starts_with("recording failed (exit 3): Device busy"),
            "{e}"
        );

        let quiet = dir.join("quiet");
        shim(&quiet, "[ $# -eq 0 ] && exit 0\nexit 0");
        let rec =
            Recording::start_with(Recorder::Ffmpeg(quiet), Os::Linux { pulse: false }, &cfg())
                .await
                .unwrap();
        let e = rec.stop().await.err().unwrap().to_string();
        assert!(e.contains("no audio was captured"), "{e}");
    }

    /// The recorder exits before its last stderr line has been read (here a
    /// child it left behind writes it): `stop` must still give that reason.
    #[tokio::test]
    async fn a_reason_written_after_the_recorder_exits_is_still_reported() {
        let dir = tmpdir("voice-fail-late");
        let bin = dir.join("ffmpeg");
        shim(
            &bin,
            "[ $# -eq 0 ] && exit 0\n(sleep 0.3; echo 'Input/output error' >&2) &\nexit 5",
        );
        let rec = Recording::start_with(Recorder::Ffmpeg(bin), Os::Linux { pulse: false }, &cfg())
            .await
            .unwrap();
        let e = rec.stop().await.err().unwrap().to_string();
        assert!(
            e.starts_with("recording failed (exit 5): Input/output error"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn silence_reported_by_the_recorder_asks_to_stop() {
        let dir = tmpdir("voice-silence");
        let bin = dir.join("ffmpeg");
        shim(
            &bin,
            "[ $# -eq 0 ] && exit 0\necho 'silence_end: 1.0' >&2\necho 'silence_start: 3.0' >&2\nexec sleep 30",
        );
        let mut rec =
            Recording::start_with(Recorder::Ffmpeg(bin), Os::Linux { pulse: true }, &cfg())
                .await
                .unwrap();
        for _ in 0..500 {
            if rec.should_stop() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(rec.should_stop());
    }

    #[tokio::test]
    async fn sox_gets_an_interrupt_and_keeps_its_file() {
        let dir = tmpdir("voice-sox");
        let (bin, ready) = (dir.join("rec"), dir.join("ready"));
        // -q -c 1 -r 16000 -b 16 <out> ...: the output is the eighth argument.
        shim(
            &bin,
            &format!(
                "[ $# -eq 0 ] && exit 0\nout=$8\ntrap 'head -c 100 /dev/zero > \"$out\"; exit 0' INT\ntouch '{}'\nwhile :; do sleep 0.05; done",
                ready.display()
            ),
        );
        let rec = Recording::start_with(Recorder::Sox(bin), Os::Mac, &cfg())
            .await
            .unwrap();
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let audio = rec.stop().await.unwrap();
        assert_eq!(std::fs::metadata(audio.path()).unwrap().len(), 100);
    }

    #[tokio::test]
    async fn windows_asks_ffmpeg_for_a_microphone_unless_one_is_set() {
        let dir = tmpdir("voice-win");
        let (bin, args) = (dir.join("ffmpeg"), dir.join("args"));
        let listing = r#"[dshow @ 0] "HD Camera" (video)\n[dshow @ 0] "USB Mic" (audio)"#;
        shim(
            &bin,
            &format!(
                "[ $# -eq 0 ] && exit 0\necho \"$*\" >> '{}'\ncase \"$*\" in *-list_devices*) printf '{listing}\\n' >&2; exit 1;; esac\nexit 0",
                args.display()
            ),
        );
        let rec = Recording::start_with(Recorder::Ffmpeg(bin.clone()), Os::Windows, &cfg())
            .await
            .unwrap();
        let _ = rec.stop().await;
        let log = std::fs::read_to_string(&args).unwrap();
        assert!(log.contains("audio=USB Mic"), "{log}");

        std::fs::remove_file(&args).unwrap();
        let mut set = cfg();
        set.device = Some("audio=Chosen".into());
        let rec = Recording::start_with(Recorder::Ffmpeg(bin), Os::Windows, &set)
            .await
            .unwrap();
        let _ = rec.stop().await;
        let log = std::fs::read_to_string(&args).unwrap();
        assert!(!log.contains("-list_devices"), "{log}");

        let none = dir.join("none");
        shim(&none, "[ $# -eq 0 ] && exit 0\nexit 1");
        let e = Recording::start_with(Recorder::Ffmpeg(none), Os::Windows, &cfg())
            .await
            .err()
            .unwrap();
        assert!(e.to_string().contains("no microphone found"), "{e}");
    }

    #[tokio::test]
    async fn a_recorder_that_cannot_start_is_reported() {
        let dir = tmpdir("voice-absent");
        let e = Recording::start_with(Recorder::Sox(dir.join("absent")), Os::Mac, &cfg())
            .await
            .err()
            .unwrap();
        assert_eq!(e.to_string(), "starting the recorder");
    }

    #[tokio::test]
    async fn whisper_cpp_output_is_cleaned_and_failures_name_the_program() {
        let dir = tmpdir("voice-whisper");
        let bin = dir.join("whisper-cli");
        shim(&bin, "[ $# -eq 0 ] && exit 0\necho '  hello there  '");
        let backend = |bin: &Path, python| Backend::Local {
            bin: bin.to_path_buf(),
            model: "m.bin".into(),
            python,
            language: Some("en".into()),
        };
        let audio = wav_with_data();
        assert_eq!(
            transcribe(&backend(&bin, false), audio.path())
                .await
                .unwrap(),
            "hello there"
        );
        let bad = dir.join("broken");
        shim(
            &bad,
            "[ $# -eq 0 ] && exit 0\necho noise >&2\necho 'model missing' >&2\nexit 1",
        );
        let e = transcribe(&backend(&bad, false), audio.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.ends_with("failed: model missing"), "{e}");
        let e = transcribe(&backend(&dir.join("absent"), false), audio.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("running "), "{e}");
    }

    #[tokio::test]
    async fn python_whisper_reads_the_file_it_wrote() {
        let dir = tmpdir("voice-python");
        let (bin, args) = (dir.join("whisper"), dir.join("args"));
        // Writes <output_dir>/<stem>.txt, as the Python whisper CLI does.
        shim(
            &bin,
            &format!(
                "[ $# -eq 0 ] && exit 0\necho \"$*\" > '{}'\nwav=$1\nwhile [ $# -gt 0 ]; do [ \"$1\" = --output_dir ] && out=$2; shift; done\nstem=$(basename \"$wav\" .wav)\necho ' from python ' > \"$out/$stem.txt\"",
                args.display()
            ),
        );
        let audio = wav_with_data();
        let local = |bin: PathBuf, language: Option<String>| Backend::Local {
            bin,
            model: "tiny".into(),
            python: true,
            language,
        };
        assert_eq!(
            transcribe(&local(bin.clone(), Some("de".into())), audio.path())
                .await
                .unwrap(),
            "from python"
        );
        let got = std::fs::read_to_string(&args).unwrap();
        assert!(
            got.contains("--model tiny") && got.contains("--language de"),
            "{got}"
        );
        transcribe(&local(bin, None), audio.path()).await.unwrap();
        assert!(
            !std::fs::read_to_string(&args)
                .unwrap()
                .contains("--language")
        );

        // It exits happily but leaves no file, or fails outright.
        let mute = dir.join("mute");
        shim(&mute, "[ $# -eq 0 ] && exit 0\nexit 0");
        let e = transcribe(&local(mute, None), audio.path())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("failed"), "{e}");
        let bad = dir.join("bad");
        shim(&bad, "[ $# -eq 0 ] && exit 0\necho 'no torch' >&2\nexit 1");
        let e = transcribe(&local(bad, None), audio.path())
            .await
            .unwrap_err();
        assert!(e.to_string().ends_with("failed: no torch"), "{e}");
    }

    #[tokio::test]
    async fn other_formats_are_converted_with_ffmpeg_first() {
        if isolated("voice::recording_tests::shims::other_formats_are_converted_with_ffmpeg_first")
        {
            return;
        }
        let dir = tmpdir("voice-convert");
        let (ffmpeg, whisper, log) = (dir.join("ffmpeg"), dir.join("whisper-cli"), dir.join("log"));
        shim(
            &ffmpeg,
            &format!(
                "[ $# -eq 0 ] && exit 0\necho \"$*\" >> '{}'\nif [ -f '{}/undecodable' ]; then echo 'Invalid data' >&2; exit 1; fi\nfor a; do last=$a; done\nhead -c 100 /dev/zero > \"$last\"",
                log.display(),
                dir.display()
            ),
        );
        shim(&whisper, "[ $# -eq 0 ] && exit 0\necho converted");
        // SAFETY: the test runs alone in its own child process.
        unsafe { std::env::set_var("PATH", format!("{}:/usr/bin:/bin", dir.display())) };
        assert_eq!(find_tool("ffmpeg"), Some(ffmpeg.clone()));
        assert_eq!(find_recorder().unwrap(), Recorder::Ffmpeg(ffmpeg));
        assert_eq!(find_tool(whisper.to_str().unwrap()), Some(whisper.clone()));
        assert_eq!(find_tool("/definitely/absent"), None);

        let input = dir.join("clip.webm");
        std::fs::write(&input, "x").unwrap();
        let backend = Backend::Local {
            bin: whisper,
            model: "m".into(),
            python: false,
            language: None,
        };
        assert_eq!(transcribe(&backend, &input).await.unwrap(), "converted");
        let args = std::fs::read_to_string(&log).unwrap();
        assert!(
            args.contains("-ar 16000") && args.contains("clip.webm"),
            "{args}"
        );

        std::fs::write(dir.join("undecodable"), "").unwrap();
        let e = to_wav(&input).await.err().unwrap().to_string();
        assert_eq!(e, "could not decode the audio: Invalid data");
    }

    #[test]
    fn sox_needs_both_of_its_programs() {
        let find = |have: &'static [&'static str]| {
            move |tool: &str| have.contains(&tool).then(|| PathBuf::from(tool))
        };
        assert!(
            pick_recorder(find(&["rec"]))
                .unwrap_err()
                .to_string()
                .contains("needs ffmpeg")
        );
        assert_eq!(
            pick_recorder(find(&["rec", "sox"])).unwrap(),
            Recorder::Sox(PathBuf::from("rec"))
        );
        assert_eq!(
            pick_recorder(find(&["ffmpeg", "rec", "sox"])).unwrap(),
            Recorder::Ffmpeg(PathBuf::from("ffmpeg"))
        );
    }
}
