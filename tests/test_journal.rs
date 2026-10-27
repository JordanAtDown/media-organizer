//! A crash between preparation and commit is recoverable from disk alone, and
//! one failing file never blocks the rest of a batch.

mod common;

use common::*;
use media_organizer::config::Config;
use media_organizer::dedup::Digester;
use media_organizer::plan::{plan_file, Candidate, PlanOutcome};
use media_organizer::resolve::resolve;
use media_organizer::staging::{Staging, JOURNAL_FILE};
use media_organizer::state::HashIndex;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn staging_dir(env: &Env) -> PathBuf {
    env.work_dir()
}

/// Run phases 2-4 by hand and write the journal — then "crash": return before
/// phase 5. Leaves the batch fully prepared on disk.
fn crash_before_commit(env: &Env, cfg: &Config) -> usize {
    let folder = &cfg.folders[0];
    let staging = Staging::new(
        folder.work_subdir(&cfg.work_root(env.state.path())),
        folder.library.clone(),
        folder.file_mode(&cfg.permissions),
        folder.dir_mode(&cfg.permissions),
    )
    .unwrap();

    let plans = files_under(env.input.path())
        .into_iter()
        .map(|rel| {
            let path = env.input.path().join(rel);
            let meta = std::fs::metadata(&path).unwrap();
            let c = Candidate {
                path,
                size: meta.len(),
                mtime: 1,
            };
            match plan_file(&c, folder) {
                PlanOutcome::Planned(p) => p,
                other => panic!("{other:?}"),
            }
        })
        .collect();
    let mut digester = Digester::new(HashIndex::load(env.state.path()));
    let resolved = resolve(
        plans,
        &folder.library,
        folder.on_conflict,
        &cfg.dedup,
        &mut digester,
    )
    .resolved;

    let entries = staging.prepare(resolved).prepared;
    staging.write_journal(&entries).unwrap();
    entries.len()
}

fn mixed_batch(env: &Env) {
    env.put("IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    env.put("IMG-20240101-WA0002.jpg", &jpeg_without_exif());
    env.put("VID-20230305-WA0003.mp4", b"video bytes");
}

#[test]
fn replay_after_a_crash_commits_everything_and_clears_the_journal() {
    let env = Env::new();
    mixed_batch(&env);
    let cfg = env.config("", "");
    assert_eq!(crash_before_commit(&env, &cfg), 3);
    assert!(
        files_under(env.output.path()).is_empty(),
        "crashed before phase 5"
    );
    assert!(staging_dir(&env).join(JOURNAL_FILE).exists());

    // A fresh process starts and scans: the journal is replayed first.
    let mut scanner = env.scanner_with(cfg, false);
    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 3);
    assert_eq!(
        files_under(env.output.path()),
        vec![
            "2023/03/VID-20230305-WA0003.mp4",
            "2024/01/IMG-20240101-WA0002.jpg",
            "2025/06/IMG-20250626-WA0001.jpg",
        ]
    );
    assert!(
        files_under(env.input.path()).is_empty(),
        "sources removed after publish"
    );
    assert!(
        !staging_dir(&env).join(JOURNAL_FILE).exists(),
        "journal cleared"
    );
    assert_eq!(std::fs::read_dir(staging_dir(&env)).unwrap().count(), 0);
    assert_eq!(sum.pending_in_staging, 0);
}

#[test]
fn replay_does_not_duplicate_work_when_nothing_is_pending() {
    let env = Env::new();
    mixed_batch(&env);
    let cfg = env.config("", "");
    crash_before_commit(&env, &cfg);
    let mut scanner = env.scanner_with(cfg, false);
    env.scan(&mut scanner);
    let after_first = files_under(env.output.path());

    let again = env.scan(&mut scanner);

    assert_eq!(again.organized, 0);
    assert_eq!(files_under(env.output.path()), after_first);
}

#[test]
fn a_journal_entry_whose_staged_file_vanished_is_dropped_and_its_source_reorganized() {
    let env = Env::new();
    mixed_batch(&env);
    let cfg = env.config("", "");
    crash_before_commit(&env, &cfg);
    // One staged file is gone (already published, then the run died before
    // rewriting the journal).
    let victim = std::fs::read_dir(staging_dir(&env))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "mp4"))
        .unwrap();
    std::fs::remove_file(victim).unwrap();

    let mut scanner = env.scanner_with(cfg, false);
    let sum = env.scan(&mut scanner);

    // Two entries replay from the journal; the dropped one still has its source,
    // so the same scan simply organizes it again. Nothing is lost or doubled.
    assert_eq!(sum.organized, 3);
    assert_eq!(files_under(env.output.path()).len(), 3);
    assert!(files_under(env.input.path()).is_empty());
    assert_eq!(sum.pending_in_staging, 0);
}

#[test]
fn staged_files_without_a_journal_are_purged_and_sources_stay_intact() {
    // A crash *during* preparation: files staged, journal never written.
    let env = Env::new();
    mixed_batch(&env);
    let cfg = env.config("", "");
    crash_before_commit(&env, &cfg);
    std::fs::remove_file(staging_dir(&env).join(JOURNAL_FILE)).unwrap();

    let mut scanner = env.scanner_with(cfg, true); // dry-run: replay is skipped
    env.scan(&mut scanner);
    let mut scanner = env.scanner("", "");
    let sum = env.scan(&mut scanner);

    // The orphans were purged and the batch was redone from the intact sources.
    assert_eq!(sum.organized, 3);
    assert_eq!(files_under(env.output.path()).len(), 3);
    assert_eq!(std::fs::read_dir(staging_dir(&env)).unwrap().count(), 0);
}

#[test]
fn one_failing_file_does_not_block_the_others_and_is_retried_next_scan() {
    let env = Env::new();
    env.put("IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    env.put("IMG-20240101-WA0002.jpg", &jpeg_without_exif());

    // 2024/01 exists but cannot be written into.
    let blocked = env.out("2024/01");
    std::fs::create_dir_all(&blocked).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::File::create(blocked.join(".probe")).is_ok() {
        // Running as root: permissions are not enforced, the scenario cannot occur.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let mut scanner = env.scanner("", "");
    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 1, "the writable one still lands");
    assert!(env.out("2025/06/IMG-20250626-WA0001.jpg").exists());
    assert!(
        env.input.path().join("IMG-20240101-WA0002.jpg").exists(),
        "failed source untouched"
    );
    assert_eq!(sum.pending_in_staging, 1, "kept in staging and journaled");
    assert!(sum.last_error.is_some());

    // The obstacle goes away; the next scan retries it from the journal.
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let retry = env.scan(&mut scanner);

    assert_eq!(retry.pending_in_staging, 0);
    assert!(env.out("2024/01/IMG-20240101-WA0002.jpg").exists());
    assert!(!env.input.path().join("IMG-20240101-WA0002.jpg").exists());
}

#[test]
fn the_recover_subcommand_finishes_an_interrupted_batch() {
    let env = Env::new();
    mixed_batch(&env);
    let cfg = env.config("", "");
    crash_before_commit(&env, &cfg);

    let cfg_file = env.state.path().join("config.toml");
    std::fs::write(
        &cfg_file,
        format!(
            "[[folders]]\nname = \"t\"\ninput = \"{}\"\nlibrary = \"{}\"\npattern = \"{{year}}/{{month}}/{{stem}}{{ext}}\"\n",
            env.input.path().display(),
            env.output.path().display()
        ),
    )
    .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_media-organizer"))
        .args([
            "--config",
            cfg_file.to_str().unwrap(),
            "--state-dir",
            env.state.path().to_str().unwrap(),
            "--health-bind",
            "",
            "recover",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("3 committed, 0 still pending"));
    assert_eq!(files_under(env.output.path()).len(), 3);
}

// -- announcing what a replay publishes ---------------------------------------

fn notify_into(env: &Env) -> String {
    format!(
        "[notify]\ndir = \"{}\"\nquiet_secs = 0\nmax_wait_secs = 0\n",
        env.state.path().join("events").display()
    )
}

fn event(env: &Env) -> media_organizer::notify::BatchEvent {
    let raw = std::fs::read(env.state.path().join("events/t.json")).expect("no event file");
    serde_json::from_slice(&raw).unwrap()
}

#[test]
fn a_replayed_batch_is_announced() {
    let env = Env::new();
    mixed_batch(&env);
    let cfg = env.config(&notify_into(&env), "");
    crash_before_commit(&env, &cfg);

    let mut scanner = env.scanner_with(cfg, false);
    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 3);
    let e = event(&env);
    assert_eq!((e.seq, e.committed), (1, 3));
    assert_eq!(e.library_dirs, ["2023/03", "2024/01", "2025/06"]);
    assert_eq!(e.input_dirs, ["."]);
}

#[test]
fn a_file_published_just_before_a_crash_is_still_announced() {
    let env = Env::new();
    mixed_batch(&env);
    let cfg = env.config(&notify_into(&env), "");
    crash_before_commit(&env, &cfg);

    // What phase 5 did for the video before the run died, ahead of rewriting the
    // journal: published under its name, source removed, journal line still there.
    let staging = Staging::new(
        cfg.folders[0].work_subdir(&cfg.work_root(env.state.path())),
        cfg.folders[0].library.clone(),
        None,
        None,
    )
    .unwrap();
    let published = staging
        .read_journal()
        .into_iter()
        .find(|e| e.source.extension().is_some_and(|x| x == "mp4"))
        .unwrap();
    std::fs::create_dir_all(published.final_dest.parent().unwrap()).unwrap();
    std::fs::rename(&published.staged, &published.final_dest).unwrap();
    std::fs::remove_file(&published.source).unwrap();

    let mut scanner = env.scanner_with(cfg, false);
    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 2, "the video was already done");
    let e = event(&env);
    assert_eq!(e.committed, 3, "the interrupted batch is announced whole");
    assert_eq!(e.library_dirs, ["2023/03", "2024/01", "2025/06"]);
}

#[test]
fn a_vanished_staged_file_whose_source_remains_is_counted_once() {
    let env = Env::new();
    mixed_batch(&env);
    let cfg = env.config(&notify_into(&env), "");
    crash_before_commit(&env, &cfg);
    // Staged file lost, source untouched: it was never published and is organized
    // afresh by this scan, so it must not also count as an earlier publication.
    let victim = std::fs::read_dir(staging_dir(&env))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "mp4"))
        .unwrap();
    std::fs::remove_file(victim).unwrap();

    let mut scanner = env.scanner_with(cfg, false);
    env.scan(&mut scanner);

    assert_eq!(event(&env).committed, 3);
}

#[test]
fn the_recover_subcommand_announces_what_it_publishes() {
    let env = Env::new();
    mixed_batch(&env);
    let cfg = env.config("", "");
    crash_before_commit(&env, &cfg);

    let cfg_file = env.state.path().join("config.toml");
    std::fs::write(
        &cfg_file,
        format!(
            "{}[[folders]]\nname = \"t\"\ninput = \"{}\"\nlibrary = \"{}\"\npattern = \"{{year}}/{{month}}/{{stem}}{{ext}}\"\n",
            // `[notify]` first: a table header takes every key after it.
            notify_into(&env),
            env.input.path().display(),
            env.output.path().display()
        ),
    )
    .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_media-organizer"))
        .args([
            "--config",
            cfg_file.to_str().unwrap(),
            "--state-dir",
            env.state.path().to_str().unwrap(),
            "--health-bind",
            "",
            "recover",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let e = event(&env);
    assert_eq!((e.seq, e.committed), (1, 3));
    assert_eq!(e.library_dirs, ["2023/03", "2024/01", "2025/06"]);
}
