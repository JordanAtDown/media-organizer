//! Name allocation is deterministic: the same inputs yield the same outputs
//! whatever the worker count, and no `exists()`-then-`rename()` race can occur.

mod common;

use common::*;

/// 48 distinct files fighting for two destination names: 24 source folders each
/// hold the same two file names, every file with a different size.
fn crowd(env: &Env) {
    for folder in 0..24 {
        for name in 0..2 {
            let body = vec![b'x'; 10 + folder * 3 + name];
            env.put(&format!("d{folder:02}/IMG-20250626-WA000{name}.jpg"), &body);
        }
    }
}

fn run(workers: usize) -> Vec<(String, Vec<u8>)> {
    let env = Env::new();
    crowd(&env);
    let mut scanner = env.scanner(&format!("workers = {workers}"), "");
    env.scan(&mut scanner);
    files_under(env.output.path())
        .into_iter()
        .map(|rel| {
            let bytes = std::fs::read(env.out(&rel)).unwrap();
            (rel, bytes)
        })
        .collect()
}

#[test]
fn one_worker_and_eight_workers_agree_on_every_path_and_content() {
    let sequential = run(1);
    let parallel = run(8);
    assert!(
        sequential.len() > 6,
        "the crowd must produce suffixed names"
    );
    assert_eq!(sequential, parallel);
}

#[test]
fn repeated_runs_are_identical() {
    assert_eq!(run(8), run(8));
}

#[test]
fn every_file_survives_with_a_unique_name() {
    let out = run(8);
    assert_eq!(out.len(), 48, "no file lost, none overwritten");
    let mut names: Vec<_> = out.iter().map(|(n, _)| n.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 48);
}

#[test]
fn suffixes_start_after_what_already_exists_on_disk() {
    let env = Env::new();
    std::fs::create_dir_all(env.out("2025/06")).unwrap();
    std::fs::write(env.out("2025/06/IMG-20250626-WA0001.jpg"), b"first").unwrap();
    std::fs::write(env.out("2025/06/IMG-20250626-WA0001_1.jpg"), b"second").unwrap();
    env.put("IMG-20250626-WA0001.jpg", b"a third, different one");
    let mut scanner = env.scanner("", "");

    env.scan(&mut scanner);

    assert!(env.out("2025/06/IMG-20250626-WA0001_2.jpg").exists());
    assert_eq!(
        std::fs::read(env.out("2025/06/IMG-20250626-WA0001.jpg")).unwrap(),
        b"first"
    );
}

#[test]
fn skip_leaves_the_newcomer_and_never_touches_the_existing_file() {
    let env = Env::new();
    std::fs::create_dir_all(env.out("2025/06")).unwrap();
    std::fs::write(env.out("2025/06/IMG-20250626-WA0001.jpg"), b"keep me").unwrap();
    let newcomer = env.put("IMG-20250626-WA0001.jpg", b"different bytes");
    let mut scanner = env.scanner("", "on_conflict = \"skip\"");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.conflicts_skipped, 1);
    assert!(newcomer.exists());
    assert_eq!(
        std::fs::read(env.out("2025/06/IMG-20250626-WA0001.jpg")).unwrap(),
        b"keep me"
    );
}

#[test]
fn overwrite_replaces_the_existing_file() {
    let env = Env::new();
    std::fs::create_dir_all(env.out("2025/06")).unwrap();
    std::fs::write(env.out("2025/06/IMG-20250626-WA0001.jpg"), b"old").unwrap();
    env.put("IMG-20250626-WA0001.jpg", b"new content!");
    let mut scanner = env.scanner("", "on_conflict = \"overwrite\"");

    env.scan(&mut scanner);

    assert_eq!(
        std::fs::read(env.out("2025/06/IMG-20250626-WA0001.jpg")).unwrap(),
        b"new content!"
    );
    assert_eq!(files_under(env.output.path()).len(), 1);
}
