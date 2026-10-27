use super::*;
use crate::config;
use std::sync::atomic::AtomicBool;

struct Env {
    input: tempfile::TempDir,
    output: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            input: tempfile::tempdir().unwrap(),
            output: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
        }
    }

    fn scanner(&self, extra: &str, dry_run: bool) -> Scanner {
        let cfg = config::parse(&format!(
            "stability_secs = 0\nworkers = 2\n{extra}\n[[folders]]\ninput = \"{}\"\nlibrary = \"{}\"\npattern = \"{{year}}/{{month}}/{{stem}}{{ext}}\"\n",
            self.input.path().display(),
            self.output.path().display(),
        ))
        .unwrap();
        Scanner::new(cfg, self.state.path().to_path_buf(), dry_run).unwrap()
    }

    fn put(&self, rel: &str, content: &[u8]) -> PathBuf {
        let p = self.input.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }

    fn run(&self, s: &mut Scanner) -> FolderSummary {
        s.scan_once(&NoObserver, &AtomicBool::new(false))
            .folders
            .remove(0)
    }
}

static LOGS: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Run `f` and return the info-level log lines it produced that mention
/// `marker` (a path unique to the calling test). The subscriber is global,
/// because per-thread capture races with tracing's callsite interest cache
/// when tests run in parallel.
fn captured_logs(marker: &Path, f: impl FnOnce()) -> String {
    struct Sink;
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            LOGS.lock().unwrap().push_str(&String::from_utf8_lossy(b));
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_writer(|| Sink)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .init();
    });

    let start = LOGS.lock().unwrap().len();
    f();
    let marker = marker.display().to_string();
    let logs = LOGS.lock().unwrap();
    logs[start..]
        .lines()
        .filter(|l| l.contains(&marker))
        .collect::<Vec<_>>()
        .join("\n")
}

fn would_move(logs: &str) -> usize {
    logs.matches("[dry-run] would move file").count()
}

#[test]
fn dry_run_reports_a_backlog_once_across_scans() {
    let env = Env::new();
    env.put("IMG-20250626-WA0002.jpg", b"a");
    env.put("IMG-20250627-WA0003.jpg", b"b");
    let mut s = env.scanner("", true);

    let first = captured_logs(env.input.path(), || {
        env.run(&mut s);
    });
    let second = captured_logs(env.input.path(), || {
        env.run(&mut s);
    });

    assert_eq!(would_move(&first), 2);
    assert_eq!(would_move(&second), 0, "{second}");
    // Nothing was moved: the summary still counts the whole plan.
    assert_eq!(env.run(&mut s).organized, 2);
}

#[test]
fn dry_run_reports_new_and_changed_files_only() {
    let env = Env::new();
    env.put("IMG-20250626-WA0002.jpg", b"a");
    let mut s = env.scanner("", true);
    env.run(&mut s);

    env.put("IMG-20250627-WA0003.jpg", b"b");
    let logs = captured_logs(env.input.path(), || {
        env.run(&mut s);
    });
    assert_eq!(would_move(&logs), 1);
    assert!(logs.contains("IMG-20250627-WA0003"), "{logs}");

    // A new mtime is a changed plan: reported again.
    let changed = env.put("IMG-20250626-WA0002.jpg", b"a");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&changed)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let logs = captured_logs(env.input.path(), || {
        env.run(&mut s);
    });
    assert_eq!(would_move(&logs), 1, "{logs}");
    assert!(logs.contains("IMG-20250626-WA0002"), "{logs}");
}

#[test]
fn dry_run_forgets_files_that_left_the_input() {
    let env = Env::new();
    let f = env.put("IMG-20250626-WA0002.jpg", b"a");
    let mut s = env.scanner("", true);
    env.run(&mut s);
    std::fs::remove_file(&f).unwrap();
    env.run(&mut s);
    assert!(s.reported.is_empty());

    env.put("IMG-20250626-WA0002.jpg", b"a");
    let logs = captured_logs(env.input.path(), || {
        env.run(&mut s);
    });
    assert_eq!(would_move(&logs), 1);
}

#[test]
fn a_fresh_process_reports_everything_again() {
    let env = Env::new();
    env.put("IMG-20250626-WA0002.jpg", b"a");
    env.run(&mut env.scanner("", true));
    let logs = captured_logs(env.input.path(), || {
        env.run(&mut env.scanner("", true));
    });
    assert_eq!(would_move(&logs), 1);
}

#[test]
fn organizes_a_file_end_to_end() {
    let env = Env::new();
    let src = env.put("IMG-20250626-WA0002.jpg", b"not a jpeg");
    let mut s = env.scanner("", false);

    let sum = env.run(&mut s);

    assert_eq!(sum.organized, 1);
    assert!(!src.exists());
    let dest = env.output.path().join("2025/06/IMG-20250626-WA0002.jpg");
    assert_eq!(std::fs::read(dest).unwrap(), b"not a jpeg");
    assert_eq!(sum.by_date_source["filename"], 1);
    assert_eq!(sum.pending_in_staging, 0);
}

#[test]
fn dry_run_writes_nothing_anywhere() {
    let env = Env::new();
    let src = env.put("IMG-20250626-WA0002.jpg", b"x");
    let mut s = env.scanner("", true);

    let sum = env.run(&mut s);

    assert_eq!(sum.organized, 1, "reports what it would do");
    assert!(src.exists());
    assert_eq!(std::fs::read_dir(env.output.path()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(env.state.path()).unwrap().count(), 0);
}

#[test]
fn excluded_directories_are_pruned() {
    let env = Env::new();
    env.put(
        "@eaDir/IMG-20250626-WA0002.jpg/SYNOPHOTO_FILM_M.mp4",
        b"thumb",
    );
    env.put("@eaDir/IMG-20250626-WA0009.jpg", b"thumb");
    env.put("IMG-20250626-WA0003.jpg", b"real");
    let mut s = env.scanner("", false);

    let sum = env.run(&mut s);

    assert_eq!(sum.found, 1);
    assert!(env.input.path().join("@eaDir").exists(), "left alone");
}

#[test]
fn duplicate_is_left_in_place_and_remembered() {
    let env = Env::new();
    env.put("IMG-20250626-WA0002.jpg", b"same");
    let mut s = env.scanner("", false);
    env.run(&mut s);

    let twin = env.put("sub/IMG-20250626-WA0002.jpg", b"same");
    let sum = env.run(&mut s);

    assert_eq!(sum.duplicates, 1);
    assert!(twin.exists(), "never deleted");
    assert!(!env
        .output
        .path()
        .join("2025/06/IMG-20250626-WA0002_1.jpg")
        .exists());

    // Remembered: the next scan does not even inventory it.
    let again = env.run(&mut s);
    assert_eq!(again.found, 0);
}

#[test]
fn undatable_file_goes_to_the_no_date_cache_without_mtime_fallback() {
    let env = Env::new();
    let src = env.put("sans_date.jpg", b"x");
    let mut s = env.scanner("", false);
    // Rebuild with the fallback off for this folder.
    let mut cfg = s.config.clone();
    cfg.folders[0].use_mtime_fallback = false;
    s = Scanner::new(cfg, env.state.path().to_path_buf(), false).unwrap();

    let sum = env.run(&mut s);
    assert_eq!(sum.no_date, 1);
    assert!(src.exists());
    assert_eq!(env.run(&mut s).found, 0, "cached, not retried");
}

#[test]
fn mtime_is_the_last_resort_and_counted_apart() {
    let env = Env::new();
    env.put("sans_date.jpg", b"x");
    let mut s = env.scanner("", false);
    let sum = env.run(&mut s);
    assert_eq!(sum.organized, 1);
    assert_eq!(sum.by_date_source["mtime"], 1);
}

#[test]
fn recently_modified_files_are_left_for_the_next_scan() {
    let env = Env::new();
    let src = env.put("IMG-20250626-WA0002.jpg", b"still uploading");
    let mut s = env.scanner("", false);
    s.config.stability_secs = 3600;

    let sum = env.run(&mut s);
    assert_eq!(sum.found, 0);
    assert!(src.exists());
}

#[test]
fn missing_input_is_reported_not_fatal() {
    let env = Env::new();
    let mut s = env.scanner("", false);
    s.config.folders[0].input = PathBuf::from("/nonexistent/input");
    let sum = env.run(&mut s);
    assert!(!sum.input_exists);
}

#[test]
fn shutdown_before_commit_leaves_output_untouched() {
    let env = Env::new();
    let src = env.put("IMG-20250626-WA0002.jpg", b"x");
    let mut s = env.scanner("", false);
    let stop = AtomicBool::new(true);

    let summary = s.scan_once(&NoObserver, &stop);
    assert!(summary.folders.is_empty());
    assert!(src.exists());
    assert!(!env.output.path().join("2025").exists());
}

#[test]
fn leftover_journal_is_replayed_at_the_start_of_a_scan() {
    let env = Env::new();
    env.put("IMG-20250626-WA0002.jpg", b"x");
    let mut s = env.scanner("", false);
    let folder = s.config.folders[0].clone();

    // Simulate a crash after phase 4: prepared + journaled, never committed.
    let staging = Staging::new(
        folder.work_subdir(&s.work_root),
        folder.library.clone(),
        None,
        None,
    )
    .unwrap();
    let cand = s.inventory(&folder, &NoObserver);
    let mut sum = FolderSummary::default();
    let plans = s.plan(cand, &folder, &NoObserver, &mut sum);
    let r = resolve(
        plans,
        &folder.library,
        folder.on_conflict,
        &s.config.dedup,
        &mut s.digester,
    );
    let entries = staging.prepare(r.resolved).prepared;
    staging.write_journal(&entries).unwrap();
    assert!(!env.output.path().join("2025").exists());

    let sum = env.run(&mut s);

    assert_eq!(sum.organized, 1);
    assert!(env
        .output
        .path()
        .join("2025/06/IMG-20250626-WA0002.jpg")
        .exists());
}
