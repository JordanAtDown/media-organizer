//! Phase 2: planning reads files and decides; it never writes.

mod common;

use common::*;
use media_organizer::date_reader::DateSource;
use media_organizer::plan::{plan_file, Candidate, PlanOutcome};
use std::path::PathBuf;

fn candidate(env: &Env, name: &str, bytes: &[u8]) -> Candidate {
    let path = env.put(name, bytes);
    Candidate {
        size: bytes.len() as u64,
        mtime: 0,
        path,
    }
}

fn folder(env: &Env, extra: &str) -> media_organizer::config::FolderConfig {
    env.config("", extra).folders.remove(0)
}

fn plan(outcome: PlanOutcome) -> media_organizer::plan::FilePlan {
    match outcome {
        PlanOutcome::Planned(p) => p,
        other => panic!("expected a plan, got {other:?}"),
    }
}

#[test]
fn exif_date_and_camera_feed_the_pattern() {
    let env = Env::new();
    let bytes = jpeg_with_exif(local(2024, 3, 15, 10, 30, 45), Some("Canon EOS R5"));
    let c = candidate(&env, "IMG_9999.jpg", &bytes);
    let f = folder(
        &env,
        "pattern = \"{camera}/{year}-{month}-{day}_{hour}{min}{sec}{ext}\"",
    );

    let p = plan(plan_file(&c, &f));

    assert_eq!(p.capture.source, DateSource::Exif);
    assert_eq!(
        p.rel_dest,
        PathBuf::from("Canon EOS R5/2024-03-15_103045.jpg")
    );
    assert!(!p.inject_exif, "a real EXIF date is never rewritten");
}

#[test]
fn unknown_camera_renders_as_unknown() {
    let env = Env::new();
    let bytes = jpeg_with_exif(local(2024, 3, 15, 10, 30, 45), None);
    let c = candidate(&env, "a.jpg", &bytes);
    let f = folder(&env, "pattern = \"{camera}/{stem}{ext}\"");
    assert_eq!(
        plan(plan_file(&c, &f)).rel_dest,
        PathBuf::from("unknown/a.jpg")
    );
}

#[test]
fn a_hostile_camera_model_cannot_escape_the_output() {
    let env = Env::new();
    let bytes = jpeg_with_exif(local(2024, 3, 15, 10, 30, 45), Some("../../etc/evil"));
    let c = candidate(&env, "a.jpg", &bytes);
    let f = folder(&env, "pattern = \"{camera}/{stem}{ext}\"");

    let rel = plan(plan_file(&c, &f)).rel_dest;
    assert!(!rel.components().any(|c| c.as_os_str() == ".."), "{rel:?}");
}

#[test]
fn photo_and_video_use_their_own_prefix() {
    let env = Env::new();
    let f = folder(
        &env,
        "pattern = \"{prefix}{stem}{ext}\"\nphoto_prefix = \"P_\"\nvideo_prefix = \"V_\"",
    );
    let photo = candidate(&env, "IMG-20250626-WA0002.jpg", b"x");
    let video = candidate(&env, "VID-20250626-WA0003.mp4", b"x");

    assert_eq!(
        plan(plan_file(&photo, &f)).rel_dest,
        PathBuf::from("P_IMG-20250626-WA0002.jpg")
    );
    assert_eq!(
        plan(plan_file(&video, &f)).rel_dest,
        PathBuf::from("V_VID-20250626-WA0003.mp4")
    );
}

#[test]
fn an_unreadable_file_is_reported_without_panicking() {
    let env = Env::new();
    let c = Candidate {
        path: env.input.path().join("ghost.jpg"),
        size: 0,
        mtime: 0,
    };
    assert!(matches!(
        plan_file(&c, &folder(&env, "")),
        PlanOutcome::Unreadable(_)
    ));
}

#[test]
fn planning_leaves_every_directory_untouched() {
    let env = Env::new();
    let c = candidate(&env, "IMG-20250626-WA0002.jpg", &jpeg_without_exif());
    let before = (
        files_under(env.input.path()),
        files_under(env.output.path()),
    );

    plan(plan_file(&c, &folder(&env, "")));

    assert_eq!(
        before,
        (
            files_under(env.input.path()),
            files_under(env.output.path())
        )
    );
    assert_eq!(std::fs::read_dir(env.state.path()).unwrap().count(), 0);
}

// ── the date fallback chain: EXIF → QuickTime → filename → mtime ─────────────

/// A JPEG carrying every source at once, each pointing at a different day.
/// `Env` only places the bytes; the caller decides which sources to strip.
fn with_all_sources(env: &Env, exif: bool, name: &str) -> Candidate {
    let bytes = if exif {
        jpeg_with_exif(local(2020, 1, 10, 9, 0, 0), None)
    } else {
        jpeg_without_exif()
    };
    let c = candidate(env, name, &bytes);
    set_mtime(&c.path, 1_700_000_000); // 2023-11-14 UTC
    c
}

#[test]
fn fallback_chain_order_exif_then_filename_then_mtime() {
    let env = Env::new();
    let f = folder(&env, "");

    // EXIF + a dated name + an mtime: the EXIF wins.
    let c = with_all_sources(&env, true, "IMG-20250626-WA0001.jpg");
    let p = plan(plan_file(&c, &f));
    assert_eq!(p.capture.source, DateSource::Exif);
    assert_eq!(p.capture.date, local(2020, 1, 10, 9, 0, 0));

    // Strip the EXIF: it falls to the *name*, not to the mtime.
    let c = with_all_sources(&env, false, "IMG-20250626-WA0002.jpg");
    let p = plan(plan_file(&c, &f));
    assert_eq!(p.capture.source, DateSource::Filename);
    assert_eq!(p.capture.date.format("%Y-%m-%d").to_string(), "2025-06-26");

    // Strip the name pattern too: only then the mtime is used.
    let c = with_all_sources(&env, false, "holiday.jpg");
    let p = plan(plan_file(&c, &f));
    assert_eq!(p.capture.source, DateSource::Mtime);
    assert_eq!(p.capture.date.timestamp(), 1_700_000_000);
}

#[test]
fn video_chain_is_quicktime_then_filename_then_mtime() {
    let env = Env::new();
    let f = folder(&env, "");

    let c = candidate(
        &env,
        "VID-20250408-WA0001.mp4",
        &mp4_with_creation_time(1_600_000_000),
    );
    assert_eq!(
        plan(plan_file(&c, &f)).capture.source,
        DateSource::QuickTime
    );

    let c = candidate(&env, "VID-20250408-WA0002.mp4", b"no moov box");
    assert_eq!(plan(plan_file(&c, &f)).capture.source, DateSource::Filename);

    let c = candidate(&env, "clip.mp4", b"no moov box");
    set_mtime(&c.path, 1_700_000_000);
    assert_eq!(plan(plan_file(&c, &f)).capture.source, DateSource::Mtime);
}

#[test]
fn without_the_mtime_fallback_an_undatable_file_is_not_planned() {
    let env = Env::new();
    let c = with_all_sources(&env, false, "holiday.jpg");
    let f = folder(&env, "use_mtime_fallback = false");
    assert!(matches!(plan_file(&c, &f), PlanOutcome::NoDate));
}

#[test]
fn no_exif_injection_from_the_mtime_unless_enabled() {
    let env = Env::new();
    let c = with_all_sources(&env, false, "holiday.jpg");

    let default = plan(plan_file(&c, &folder(&env, "")));
    assert_eq!(default.capture.source, DateSource::Mtime);
    assert!(!default.inject_exif);

    let enabled = plan(plan_file(&c, &folder(&env, "write_exif_from_mtime = true")));
    assert!(enabled.inject_exif);
}

#[test]
fn an_mtime_dated_file_reaches_the_output_without_a_forged_exif_tag() {
    let env = Env::new();
    let src = env.put("holiday.jpg", &jpeg_without_exif());
    set_mtime(&src, 1_700_000_000);
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 1);
    assert_eq!(sum.exif_injected, 0);
    let placed = files_under(env.output.path());
    let info = media_organizer::metadata::exif_read::read_exif(&env.out(&placed[0])).unwrap();
    assert!(
        info.date.is_none(),
        "an approximation must not be written as a fact"
    );
}

#[test]
fn an_undatable_file_stays_put_and_is_remembered_without_the_fallback() {
    let env = Env::new();
    let src = env.put("holiday.jpg", &jpeg_without_exif());
    let mut scanner = env.scanner("", "use_mtime_fallback = false");

    let first = env.scan(&mut scanner);
    assert_eq!(first.no_date, 1);
    assert!(src.exists());
    assert!(files_under(env.output.path()).is_empty());
    assert!(env.state.path().join("no_date_cache.json").exists());

    assert_eq!(env.scan(&mut scanner).found, 0, "remembered, not retried");
}
