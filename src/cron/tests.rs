use super::*;

fn dir() -> std::path::PathBuf {
    std::env::temp_dir()
}

#[test]
fn fixed_loop_fires_on_its_interval_and_stops() {
    let s = Scheduler::default();
    s.start_loop(Some(Duration::from_secs(300)), Some("check CI".into()));
    assert!(s.loop_active());
    assert!(
        s.awake.lock().unwrap().is_some(),
        "a loop keeps the machine awake"
    );
    let now = Local::now();
    // The first run is due at once.
    assert_eq!(s.take_due(now, &dir()).unwrap().prompt, "check CI");
    assert!(s.take_due(now, &dir()).is_none(), "not due yet");
    let fire = s
        .take_due(now + ChronoDuration::seconds(301), &dir())
        .unwrap();
    assert_eq!(
        (fire.source, fire.prompt.as_str()),
        (Source::Loop, "check CI")
    );
    // Advanced past now: no catch-up for missed runs.
    assert!(
        s.take_due(now + ChronoDuration::seconds(302), &dir())
            .is_none()
    );
    assert!(s.loop_status().unwrap().contains("every 5m"));
    assert!(s.stop_loop());
    assert!(!s.loop_active() && s.next_due().is_none());
    assert!(s.awake.lock().unwrap().is_none());
}

#[test]
fn self_paced_loop_reschedules_falls_back_then_ends() {
    let s = Scheduler::default();
    s.start_loop(None, Some("watch the deploy".into()));
    let first = s.take_due(Local::now(), &dir()).unwrap();
    assert!(
        first.prompt.starts_with("watch the deploy") && first.prompt.contains("ScheduleWakeup")
    );
    // Clamped to the 60s minimum.
    let out = s
        .schedule_wakeup(Some(5), Some("build finishing".into()), false)
        .unwrap();
    assert!(out.contains("in 1m"), "{out}");
    assert!(s.iteration_done().unwrap().contains("build finishing"));
    let fire = s
        .take_due(Local::now() + ChronoDuration::seconds(61), &dir())
        .unwrap();
    assert_eq!(fire.source, Source::Wakeup);
    // This iteration doesn't reschedule: one fallback wake-up, ~20 minutes out.
    assert!(s.iteration_done().unwrap().contains("didn't schedule"));
    let next = s.next_due().unwrap();
    assert!((next - Local::now()).num_seconds() > 19 * 60);
    s.take_due(next, &dir()).unwrap();
    // Still no reschedule: the loop ends.
    assert!(s.iteration_done().unwrap().contains("Loop ended"));
    assert!(!s.loop_active());
    assert!(
        s.schedule_wakeup(Some(60), None, false).is_err(),
        "no loop to schedule"
    );
}

#[test]
fn stop_true_ends_a_self_paced_loop() {
    let s = Scheduler::default();
    s.start_loop(None, None);
    assert!(
        s.take_due(Local::now(), &dir())
            .unwrap()
            .prompt
            .contains("ScheduleWakeup")
    );
    s.schedule_wakeup(Some(600), None, false).unwrap();
    assert!(
        s.schedule_wakeup(None, None, true)
            .unwrap()
            .contains("stopped")
    );
    assert!(!s.loop_active() && s.next_due().is_none());
}

#[test]
fn cron_tasks_recur_or_fire_once() {
    let s = Scheduler::default();
    assert!(s.create("61 * * * *", "x", true).is_err());
    assert!(s.create("* * * * *", " ", true).is_err());
    let every = s.create("*/5 * * * *", "poll the PR", true).unwrap();
    let once = s.create("* * * * *", "remind me", false).unwrap();
    assert_eq!(every.id.len(), 8);
    let later = Local::now() + ChronoDuration::minutes(6);
    let a = s.take_due(later, &dir()).unwrap();
    let b = s.take_due(later, &dir()).unwrap();
    assert!(s.take_due(later, &dir()).is_none());
    let fired: Vec<&str> = vec![a.id.as_str(), b.id.as_str()];
    assert!(fired.contains(&every.id.as_str()) && fired.contains(&once.id.as_str()));
    assert!(a.prompt.contains("CronCreate"));
    // The one-shot deleted itself; the recurring one remains.
    assert_eq!(
        s.tasks().iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
        vec![every.id.clone()]
    );
    assert!(s.delete(&every.id).is_some() && s.tasks().is_empty());
    // Loops are separate from cron tasks.
    s.create("0 9 * * 1-5", "standup notes", true).unwrap();
    s.start_loop(Some(Duration::from_secs(60)), Some("x".into()));
    s.stop_loop();
    assert_eq!(s.tasks().len(), 1);
}

#[test]
fn model_scheduled_prompts_never_read_as_typed_input() {
    let s = Scheduler::default();
    for p in ["!touch /tmp/x", " /review", "#remember this", "\n!id"] {
        assert!(s.create("* * * * *", p, true).is_err(), "{p:?}");
    }
    s.create("* * * * *", "run the tests", false).unwrap();
    let later = Local::now() + ChronoDuration::minutes(2);
    assert!(s.take_due(later, &dir()).unwrap().literal);
    // The user's own `/loop 5m /cmd` may run a command; the default
    // prompt (which `loop.md` in the project can supply) may not.
    s.start_loop(Some(Duration::from_secs(300)), Some("/babysit".into()));
    assert!(!s.take_due(Local::now(), &dir()).unwrap().literal);
    s.start_loop(Some(Duration::from_secs(300)), None);
    assert!(s.take_due(Local::now(), &dir()).unwrap().literal);
    s.start_loop(None, None);
    assert!(s.take_due(Local::now(), &dir()).unwrap().literal);
    s.stop_loop();
}

#[test]
fn recurring_tasks_fire_a_final_time_after_a_week() {
    let s = Scheduler::default();
    let t = s.create("* * * * *", "x", true).unwrap();
    let late = t.created + ChronoDuration::days(8);
    let f = s.take_due(late, &dir()).unwrap();
    assert!(f.note.contains("final run"));
    assert!(s.tasks().is_empty());
}

#[test]
fn default_prompt_prefers_loop_md() {
    let d = std::env::temp_dir().join(format!("chanto-loopmd-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::write(d.join(".claude/loop.md"), "Keep the release PR green.\n").unwrap();
    assert_eq!(default_prompt(&d), "Keep the release PR green.");
    let _ = std::fs::remove_dir_all(&d);
}
