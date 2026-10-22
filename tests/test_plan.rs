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
