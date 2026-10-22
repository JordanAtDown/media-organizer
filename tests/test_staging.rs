//! The central property of the design: nothing reaches `output` before the
//! commit, and a prepared batch hard-links where it can and copies where it must.

mod common;

use common::*;
use media_organizer::scanner::{NoObserver, Observer, Phase};
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};

/// Requests shutdown the moment phase 4 starts, so the scan stops right after
/// the batch is prepared and right before it would be committed.
struct StopAfterPreparation<'a>(&'a AtomicBool);

impl Observer for StopAfterPreparation<'_> {
    fn phase(&self, _folder: &str, phase: Phase) {
        if phase == Phase::Preparation {
            self.0.store(true, Ordering::SeqCst);
        }
    }
}

fn populate(env: &Env) -> Vec<std::path::PathBuf> {
    (1..=5)
        .map(|i| env.put(&format!("IMG-20250626-WA{i:04}.jpg"), &jpeg_without_exif()))
        .collect()
}

#[test]
fn nothing_is_written_to_output_before_the_commit() {
    let env = Env::new();
    let sources = populate(&env);
    let mut scanner = env.scanner("", "");
    let stop = AtomicBool::new(false);

    // Prepared in staging, then stopped before phase 5.
    scanner.scan_once(&StopAfterPreparation(&stop), &stop);

    assert!(
        files_under(env.output.path()).is_empty(),
        "output must be untouched"
    );
    assert!(!env.out("2025").exists(), "not even a directory");
    assert!(sources.iter().all(|s| s.exists()), "every source intact");
    assert!(
        std::fs::read_dir(env.output.path().join(".media-organizer/staging"))
            .unwrap()
            .count()
            > 0,
        "the batch was genuinely prepared in staging"
    );
}

#[test]
fn the_batch_is_committed_by_the_next_scan() {
    let env = Env::new();
    let sources = populate(&env);
    let mut scanner = env.scanner("", "");
    let stop = AtomicBool::new(false);
    scanner.scan_once(&StopAfterPreparation(&stop), &stop);

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 5);
    assert_eq!(files_under(env.output.path()).len(), 5);
    assert!(sources.iter().all(|s| !s.exists()));
    assert_eq!(sum.pending_in_staging, 0);
}

#[test]
fn staging_is_empty_after_a_successful_scan() {
    let env = Env::new();
    populate(&env);
    let mut scanner = env.scanner("", "");
    env.scan(&mut scanner);

    let left: Vec<_> = std::fs::read_dir(env.output.path().join(".media-organizer/staging"))
        .unwrap()
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_staging_dir_inside_input_is_never_inventoried() {
    let env = Env::new();
    populate(&env);
    // Leftovers of an interrupted run sit in staging, *inside* the scanned tree.
    let staging = env.input.path().join("_staging");
    let mut scanner = env.scanner(&format!("[staging]\ndir = \"{}\"", staging.display()), "");
    let stop = AtomicBool::new(false);
    scanner.scan_once(&StopAfterPreparation(&stop), &stop);
    assert!(std::fs::read_dir(&staging).unwrap().count() > 0);

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.found, 5, "the staged hard links are not candidates");
    assert_eq!(sum.organized, 5);
    assert_eq!(files_under(env.output.path()).len(), 5);
}

#[test]
fn a_dry_run_never_creates_the_staging_area() {
    let env = Env::new();
    populate(&env);
    let mut scanner = env.scanner_with(env.config("", ""), true);

    let sum = scanner
        .scan_once(&NoObserver, &AtomicBool::new(false))
        .folders
        .remove(0);

    assert_eq!(sum.organized, 5, "reports the plan");
    assert_eq!(std::fs::read_dir(env.output.path()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(env.state.path()).unwrap().count(), 0);
}

#[test]
fn a_plain_file_is_hard_linked_to_staging_not_copied() {
    // Observed through the committed result: link then rename leaves one inode,
    // and the source is the *same* inode until it is unlinked.
    let env = Env::new();
    let src = env.put("IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    let ino = std::fs::metadata(&src).unwrap().ino();
    let mut scanner = env.scanner("", "write_exif_from_filename = false");

    env.scan(&mut scanner);

    let dest = env.out("2025/06/IMG-20250626-WA0001.jpg");
    assert_eq!(
        std::fs::metadata(dest).unwrap().ino(),
        ino,
        "moved, never copied"
    );
}

#[test]
fn exif_injection_works_on_a_copy_and_the_source_is_never_rewritten() {
    let env = Env::new();
    let original = jpeg_without_exif();
    let src = env.put("IMG-20250626-WA0001.jpg", &original);
    let src_ino = std::fs::metadata(&src).unwrap().ino();
    let mut scanner = env.scanner("", "");
    let stop = AtomicBool::new(false);

    // Stop after phase 4: the source and the staged copy coexist.
    scanner.scan_once(&StopAfterPreparation(&stop), &stop);

    assert_eq!(
        std::fs::read(&src).unwrap(),
        original,
        "source byte-for-byte unchanged"
    );
    assert_eq!(
        std::fs::metadata(&src).unwrap().nlink(),
        1,
        "no shared inode"
    );
    let staged = std::fs::read_dir(env.output.path().join(".media-organizer/staging"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "jpg"))
        .expect("a staged copy");
    assert_ne!(std::fs::metadata(&staged).unwrap().ino(), src_ino);
    let info = media_organizer::metadata::exif_read::read_exif(&staged).unwrap();
    assert_eq!(info.date, Some(local(2025, 6, 26, 12, 0, 0)));
}
