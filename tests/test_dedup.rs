//! Duplicates are found by name *and* content, and cost no hashing when they
//! cannot exist. A duplicate is never deleted.

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
