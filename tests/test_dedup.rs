//! Duplicates are found by name *and* content, and cost no hashing when they
//! cannot exist. A duplicate is never deleted, unless `delete_source_duplicates`
//! is on and the source is re-verified byte for byte against the library.

mod common;

use common::*;

fn names(env: &Env) -> Vec<String> {
    files_under(env.output.path())
}

#[test]
fn distinct_sizes_are_never_hashed() {
    let env = Env::new();
    // Same destination name, different sizes: provably different, no hash needed.
    env.put("a/IMG-20250626-WA0001.jpg", b"short");
    env.put("b/IMG-20250626-WA0001.jpg", b"a rather longer payload");
    env.put("IMG-20250626-WA0002.jpg", b"unrelated");
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 3);
    assert_eq!(sum.duplicates, 0);
    assert_eq!(scanner.hashes_computed(), 0);
}

#[test]
fn same_size_different_content_is_not_a_duplicate() {
    let env = Env::new();
    env.put("a/IMG-20250626-WA0001.jpg", b"aaaa");
    env.put("b/IMG-20250626-WA0001.jpg", b"aaab");
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 2);
    assert_eq!(sum.duplicates, 0);
    assert_eq!(
        names(&env),
        vec![
            "2025/06/IMG-20250626-WA0001.jpg",
            "2025/06/IMG-20250626-WA0001_1.jpg"
        ]
    );
    assert!(
        scanner.hashes_computed() > 0,
        "same size must be compared by content"
    );
}

#[test]
fn an_identical_pair_in_one_batch_is_organized_once_and_left_once() {
    let env = Env::new();
    env.put("a/IMG-20250626-WA0001.jpg", b"identical");
    let twin = env.put("b/IMG-20250626-WA0001.jpg", b"identical");
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 1);
    assert_eq!(sum.duplicates, 1);
    assert_eq!(
        names(&env),
        vec!["2025/06/IMG-20250626-WA0001.jpg"],
        "no _1 variant"
    );
    assert!(twin.exists(), "a duplicate is never deleted");
}

#[test]
fn a_copy_of_an_already_organized_file_is_recognized_on_a_later_scan() {
    let env = Env::new();
    env.put("IMG-20250626-WA0001.jpg", b"original");
    let mut scanner = env.scanner("", "");
    env.scan(&mut scanner);

    let again = env.put("resync/IMG-20250626-WA0001.jpg", b"original");
    let sum = env.scan(&mut scanner);

    assert_eq!(sum.duplicates, 1);
    assert_eq!(names(&env), vec!["2025/06/IMG-20250626-WA0001.jpg"]);
    assert!(again.exists());
}

#[test]
fn a_remembered_duplicate_is_not_rehashed_on_the_next_scan() {
    let env = Env::new();
    env.put("IMG-20250626-WA0001.jpg", b"original");
    let mut scanner = env.scanner("", "");
    env.scan(&mut scanner);
    env.put("resync/IMG-20250626-WA0001.jpg", b"original");
    env.scan(&mut scanner);
    let hashed = scanner.hashes_computed();

    let third = env.scan(&mut scanner);

    assert_eq!(third.found, 0, "known duplicate skipped at inventory");
    assert_eq!(scanner.hashes_computed(), hashed);
    assert!(env.state.path().join("duplicates.json").exists());
}

#[test]
fn a_changed_duplicate_is_looked_at_again() {
    let env = Env::new();
    env.put("IMG-20250626-WA0001.jpg", b"original");
    let mut scanner = env.scanner("", "");
    env.scan(&mut scanner);
    let twin = env.put("resync/IMG-20250626-WA0001.jpg", b"original");
    env.scan(&mut scanner);

    // The user edits the file: new content, new mtime. It is no longer a duplicate.
    std::fs::write(&twin, b"edited!!").unwrap();
    set_mtime(&twin, 1_800_000_000);
    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 1);
    assert_eq!(sum.duplicates, 0);
    assert!(names(&env).contains(&"2025/06/IMG-20250626-WA0001_1.jpg".to_string()));
}

#[test]
fn dedup_can_be_turned_off() {
    let env = Env::new();
    env.put("a/IMG-20250626-WA0001.jpg", b"identical");
    env.put("b/IMG-20250626-WA0001.jpg", b"identical");
    let mut scanner = env.scanner("[dedup]\nenabled = false", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.duplicates, 0);
    assert_eq!(sum.organized, 2);
    assert_eq!(scanner.hashes_computed(), 0);
}

#[test]
fn the_hash_index_survives_a_restart() {
    let env = Env::new();
    env.put("IMG-20250626-WA0001.jpg", b"original");
    let mut first = env.scanner("", "");
    env.scan(&mut first);
    env.put("a/IMG-20250626-WA0001.jpg", b"original");
    env.scan(&mut first);
    assert!(env.state.path().join("hash_index.json").exists());

    // A new process: same state dir, fresh scanner.
    let mut second = env.scanner("", "");
    env.put("b/IMG-20250626-WA0001.jpg", b"original");
    env.scan(&mut second);
    assert_eq!(
        second.hashes_computed(),
        1,
        "only the new file is read; the library copy is indexed"
    );
}

// -- dedup.delete_source_duplicates ---------------------------------------------

const DELETE: &str = "[dedup]\ndelete_source_duplicates = true\n";
const NAME: &str = "IMG-20250626-WA0001.jpg";

/// Organize one file, then drop an identical one under another directory.
fn organized_with_twin(env: &Env) -> std::path::PathBuf {
    env.put(NAME, b"payload");
    let mut first = env.scanner("", "");
    assert_eq!(env.scan(&mut first).organized, 1);
    env.put(&format!("again/{NAME}"), b"payload")
}

#[test]
fn a_verified_duplicate_source_is_removed_when_asked() {
    let env = Env::new();
    let twin = organized_with_twin(&env);
    let mut scanner = env.scanner(DELETE, "");

    let sum = env.scan(&mut scanner);

    assert_eq!((sum.duplicates, sum.duplicates_removed), (1, 1));
    assert!(!twin.exists());
    assert_eq!(
        std::fs::read(env.out("2025/06").join(NAME)).unwrap(),
        b"payload"
    );
    assert_eq!(names(&env), vec![format!("2025/06/{NAME}")]);
}

#[test]
fn a_duplicate_source_is_kept_by_default() {
    let env = Env::new();
    let twin = organized_with_twin(&env);
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!((sum.duplicates, sum.duplicates_removed), (1, 0));
    assert!(twin.exists());
}

#[test]
fn dry_run_never_removes_a_duplicate() {
    let env = Env::new();
    let twin = organized_with_twin(&env);
    let mut scanner = env.scanner_with(env.config(DELETE, ""), true);

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.duplicates_removed, 0);
    assert!(twin.exists());
}

#[test]
fn an_intra_batch_duplicate_is_kept_until_the_original_is_published() {
    let env = Env::new();
    let a = env.put(&format!("a/{NAME}"), b"payload");
    let b = env.put(&format!("b/{NAME}"), b"payload");
    let mut scanner = env.scanner(DELETE, "");

    // The target name does not exist yet: nothing may be removed.
    let first = env.scan(&mut scanner);
    assert_eq!(
        (first.organized, first.duplicates, first.duplicates_removed),
        (1, 1, 0)
    );
    assert!(!a.exists() && b.exists(), "a is organized, b is kept");

    // Now the original is a real file in the library: the twin is verified and removed.
    let second = env.scan(&mut scanner);
    assert_eq!(second.duplicates_removed, 1);
    assert!(!b.exists());
    assert_eq!(names(&env), vec![format!("2025/06/{NAME}")]);
}

#[test]
fn a_stale_hash_index_cannot_get_a_different_file_deleted() {
    let env = Env::new();
    // The twin is hashed (and remembered) with the option off...
    let twin = organized_with_twin(&env);
    let mtime = std::fs::metadata(&twin).unwrap().modified().unwrap();
    let mut off = env.scanner("", "");
    assert_eq!(env.scan(&mut off).duplicates, 1);
    drop(off);

    // ...then its content changes to something of the same size, mtime restored:
    // the persistent index (path, size, mtime) still says "identical".
    std::fs::write(&twin, b"payloaX").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&twin)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    std::fs::remove_file(env.state.path().join("duplicates.json")).unwrap();
    let mut on = env.scanner(DELETE, "");

    let sum = env.scan(&mut on);

    assert_eq!(sum.duplicates_removed, 0);
    assert_eq!(std::fs::read(&twin).unwrap(), b"payloaX");
    assert_eq!(
        std::fs::read(env.out("2025/06").join(NAME)).unwrap(),
        b"payload"
    );
}

#[test]
fn a_missing_original_means_the_source_is_organized_not_deleted() {
    let env = Env::new();
    let twin = organized_with_twin(&env);
    // No file at the target: nothing to prove identity against.
    std::fs::remove_file(env.out("2025/06").join(NAME)).unwrap();
    let mut scanner = env.scanner(DELETE, "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.duplicates_removed, 0);
    assert!(!twin.exists(), "it was organized, not deleted");
    assert_eq!(
        std::fs::read(env.out("2025/06").join(NAME)).unwrap(),
        b"payload"
    );
}
