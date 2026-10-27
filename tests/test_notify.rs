//! A finished batch is announced once per folder, after the last commit: the event
//! file is atomic and carries a `seq`, nothing is announced in dry-run or for a
//! batch that changed nothing, a failing sink never fails the batch, and uploads
//! that trickle in are held and announced as one event once the input is quiet.

mod common;

use common::*;
use media_organizer::daemon;
use media_organizer::health::HealthState;
use media_organizer::notify::{BatchEvent, NOTIFY_FILE};
use media_organizer::shutdown::Shutdown;
use std::path::PathBuf;
use std::time::Duration;

const DELETE: &str = "[dedup]\ndelete_source_duplicates = true\n";
const NAME: &str = "IMG-20250626-WA0001.jpg";

fn events_dir(env: &Env) -> PathBuf {
    env.state.path().join("events")
}

/// `[notify]` writing under the state dir, as the compose example does, with no
/// debounce so each batch is announced at once. Appended last: a table header
/// takes every key after it.
fn notify(env: &Env, top: &str) -> String {
    format!(
        "{top}\n[notify]\ndir = \"{}\"\nquiet_secs = 0\nmax_wait_secs = 0\n",
        events_dir(env).display()
    )
}

/// The default debounce: a minute of quiet, capped at ten.
fn notify_debounced(env: &Env) -> String {
    format!("[notify]\ndir = \"{}\"\n", events_dir(env).display())
}

/// Pretend every held announcement has been waiting for hours (the clock is not
/// something a test should sleep on).
fn age_held_announcements(env: &Env) {
    let path = env.state.path().join(NOTIFY_FILE);
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for folder in state.as_object_mut().unwrap().values_mut() {
        if let Some(held) = folder.get_mut("pending") {
            held["first_at"] = 1.into();
            held["last_at"] = 1.into();
        }
    }
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
}

fn event(env: &Env) -> BatchEvent {
    // The folder is named `t` by the test config.
    let raw = std::fs::read(events_dir(env).join("t.json")).expect("no event file");
    serde_json::from_slice(&raw).expect("the event file is not valid JSON")
}

fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_batch_of_several_files_makes_one_event_with_its_directories() {
    let env = Env::new();
    env.put("IMG-20250626-WA0001.jpg", b"one");
    env.put("IMG-20250627-WA0002.jpg", b"two");
    env.put("trip/IMG-20250715-WA0003.jpg", b"three");
    env.put("trip/IMG-20250716-WA0004.jpg", b"four");
    let mut scanner = env.scanner(&notify(&env, ""), "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 4);
    let e = event(&env);
    assert_eq!((e.folder.as_str(), e.seq), ("t", 1));
    assert_eq!((e.committed, e.deleted_sources), (4, 0));
    assert_eq!(e.library_dirs, ["2025/06", "2025/07"]);
    assert_eq!(e.input_dirs, [".", "trip"]);
    assert_eq!(sum.announced.unwrap().event, e);
    // One event for the folder, whatever the file count; no temporary left visible.
    assert_eq!(entries(&events_dir(&env)), [".tmp", "t.json"]);
    assert_eq!(
        entries(&events_dir(&env).join(".tmp")),
        Vec::<String>::new()
    );
}

#[test]
fn every_announced_file_is_already_published() {
    let env = Env::new();
    env.put(NAME, b"payload");
    let mut scanner = env.scanner(&notify(&env, ""), "");
    env.scan(&mut scanner);

    // The event exists, so the library already holds what it announces.
    assert!(event(&env).library_dirs.iter().all(|d| env.out(d).is_dir()));
    assert!(env.out("2025/06").join(NAME).is_file());
    assert!(!env.input.path().join(NAME).exists());
}

#[test]
fn seq_grows_with_each_batch_and_a_quiet_scan_announces_nothing() {
    let env = Env::new();
    env.put(NAME, b"one");
    let mut scanner = env.scanner(&notify(&env, ""), "");
    env.scan(&mut scanner);
    assert_eq!(event(&env).seq, 1);

    let quiet = env.scan(&mut scanner);
    assert!(quiet.announced.is_none());
    assert_eq!(event(&env).seq, 1, "nothing new, nothing announced");

    env.put("IMG-20250701-WA0002.jpg", b"two");
    env.scan(&mut scanner);
    let e = event(&env);
    assert_eq!(
        (e.seq, e.committed, e.library_dirs),
        (2, 1, vec!["2025/07".to_string()])
    );
}

#[test]
fn seq_continues_after_a_restart() {
    let env = Env::new();
    env.put(NAME, b"one");
    let cfg = env.config(&notify(&env, ""), "");
    env.scan(&mut env.scanner_with(cfg.clone(), false));

    env.put("IMG-20250701-WA0002.jpg", b"two");
    let mut restarted = env.scanner_with(cfg, false);
    assert_eq!(restarted.last_events()["t"].seq, 1);
    env.scan(&mut restarted);

    assert_eq!(event(&env).seq, 2);
    assert!(env.state.path().join(NOTIFY_FILE).is_file());
}

#[test]
fn dry_run_announces_and_writes_nothing() {
    let env = Env::new();
    env.put(NAME, b"payload");
    let cfg = env.config(&notify(&env, ""), "");
    let mut scanner = env.scanner_with(cfg, true);

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 1, "reports what it would do");
    assert!(sum.announced.is_none());
    assert!(!events_dir(&env).exists());
    assert_eq!(std::fs::read_dir(env.state.path()).unwrap().count(), 0);
}

#[test]
fn without_a_notify_section_nothing_is_written() {
    let env = Env::new();
    env.put(NAME, b"payload");
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 1);
    assert!(sum.announced.is_none());
    assert!(!events_dir(&env).exists());
    assert!(!env.state.path().join(NOTIFY_FILE).exists());
}

#[test]
fn a_duplicate_that_is_kept_changes_nothing_and_is_not_announced() {
    let env = Env::new();
    env.put(NAME, b"payload");
    let mut scanner = env.scanner(&notify(&env, ""), "");
    env.scan(&mut scanner);
    env.put(&format!("again/{NAME}"), b"payload");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.duplicates, 1);
    assert!(sum.announced.is_none());
    assert_eq!(event(&env).seq, 1);
}

#[test]
fn a_removed_duplicate_source_is_announced_with_its_input_directory() {
    let env = Env::new();
    env.put(NAME, b"payload");
    let mut scanner = env.scanner(&notify(&env, DELETE), "");
    env.scan(&mut scanner);
    env.put(&format!("again/{NAME}"), b"payload");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.duplicates_removed, 1);
    let e = event(&env);
    assert_eq!((e.seq, e.committed, e.deleted_sources), (2, 0, 1));
    assert!(e.library_dirs.is_empty(), "nothing was published");
    assert_eq!(e.input_dirs, ["again"]);
}

#[test]
fn a_missing_event_directory_is_created() {
    let env = Env::new();
    env.put(NAME, b"payload");
    let deep = env.state.path().join("a").join("events");
    let mut scanner = env.scanner(
        &format!(
            "[notify]\ndir = \"{}\"\nquiet_secs = 0\nmax_wait_secs = 0\n",
            deep.display()
        ),
        "",
    );

    env.scan(&mut scanner);

    assert!(deep.join("t.json").is_file());
}

#[test]
fn an_unwritable_event_directory_warns_but_the_batch_goes_through() {
    let env = Env::new();
    env.put(NAME, b"payload");
    // `dir` exists as a file: nothing can be written under it.
    let blocker = env.state.path().join("events");
    std::fs::write(&blocker, b"in the way").unwrap();
    let mut scanner = env.scanner(&notify(&env, ""), "");

    let sum = env.scan(&mut scanner);

    assert_eq!((sum.organized, sum.failed), (1, 0));
    assert!(env.out("2025/06").join(NAME).is_file());
    let announced = sum.announced.unwrap();
    assert_eq!(announced.errors.len(), 1, "{:?}", announced.errors);
    assert_eq!(announced.event.seq, 1);
}

#[test]
fn healthz_reports_the_last_event_per_folder() {
    let env = Env::new();
    env.put(NAME, b"payload");
    let mut scanner = env.scanner(&notify(&env, ""), "");
    let health = HealthState::new(Default::default(), 30);

    daemon::run(
        &mut scanner,
        &health,
        &Shutdown::new(),
        Duration::from_secs(3600),
        true,
    );

    let json = serde_json::to_value(health.snapshot()).unwrap();
    assert_eq!(json["notify"]["t"]["seq"], 1);
    assert_eq!(json["notify"]["t"]["committed"], 1);
    assert!(json["notify"]["t"]["last_error"].is_null());
    assert_eq!(json["status"], "ok");
}

#[test]
fn healthz_has_no_notify_block_when_the_feature_is_off() {
    let env = Env::new();
    env.put(NAME, b"payload");
    let mut scanner = env.scanner("", "");
    let health = HealthState::new(Default::default(), 30);

    daemon::run(
        &mut scanner,
        &health,
        &Shutdown::new(),
        Duration::from_secs(3600),
        true,
    );

    let json = serde_json::to_value(health.snapshot()).unwrap();
    assert!(json.get("notify").is_none());
}

// -- uploads that trickle in ------------------------------------------------------

#[test]
fn trickling_uploads_are_held_then_announced_as_one_event() {
    let env = Env::new();
    let cfg = env.config(&notify_debounced(&env), "");
    let mut scanner = env.scanner_with(cfg.clone(), false);

    // Two scans, a file each: both are organized at once, neither is announced.
    env.put("IMG-20250626-WA0001.jpg", b"one");
    let first = env.scan(&mut scanner);
    env.put("trip/IMG-20250715-WA0002.jpg", b"two");
    let second = env.scan(&mut scanner);

    assert_eq!((first.organized, second.organized), (1, 1));
    assert!(first.announced.is_none() && second.announced.is_none());
    assert!(
        env.out("2025/06/IMG-20250626-WA0001.jpg").is_file(),
        "organizing is not delayed"
    );
    assert!(!events_dir(&env).join("t.json").exists(), "no event yet");

    // The uploads stop and the quiet period passes. This run restarts, so the held
    // announcement must have been persisted.
    age_held_announcements(&env);
    let mut restarted = env.scanner_with(cfg, false);
    let quiet = env.scan(&mut restarted);

    assert_eq!(quiet.organized, 0);
    let e = event(&env);
    assert_eq!((e.seq, e.committed), (1, 2));
    assert_eq!(e.library_dirs, ["2025/06", "2025/07"]);
    assert_eq!(e.input_dirs, [".", "trip"]);
    assert_eq!(quiet.announced.unwrap().event, e);

    // Nothing new, nothing more.
    assert!(env.scan(&mut restarted).announced.is_none());
    assert_eq!(event(&env).seq, 1);
}

#[test]
fn a_held_announcement_is_not_a_failure_and_healthz_stays_ok() {
    let env = Env::new();
    env.put("IMG-20250626-WA0001.jpg", b"one");
    let mut scanner = env.scanner(&notify_debounced(&env), "");
    let health = HealthState::new(Default::default(), 30);

    daemon::run(
        &mut scanner,
        &health,
        &Shutdown::new(),
        Duration::from_secs(3600),
        true,
    );

    let json = serde_json::to_value(health.snapshot()).unwrap();
    assert_eq!(json["status"], "ok");
    assert!(
        json.get("notify").is_none(),
        "nothing announced yet, nothing to report"
    );
}
