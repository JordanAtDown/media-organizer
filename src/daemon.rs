//! The scan loop: scan, record health, sleep interruptibly, repeat.

use crate::health::state::Caches;
use crate::health::HealthState;
use crate::scanner::{ScanSummary, Scanner};
use crate::shutdown::Shutdown;
use std::time::Duration;

/// Run scans until shutdown, or exactly once when `once` is set.
/// Returns the summary of the last scan.
pub fn run(
    scanner: &mut Scanner,
    health: &HealthState,
    shutdown: &Shutdown,
    interval: Duration,
    once: bool,
) -> ScanSummary {
    loop {
        health.begin_scan();
        let summary = scanner.scan_once(health, shutdown.flag());
        let (no_date, duplicates, hash_index) = scanner.cache_sizes();
        health.record_scan(
            &summary,
            Caches {
                no_date,
                duplicates,
                hash_index,
            },
        );

        // One pass for --once; otherwise sleep, waking at once on SIGTERM.
        if once || shutdown.wait(interval) {
            return summary;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn scanner(
        input: &std::path::Path,
        output: &std::path::Path,
        state: &std::path::Path,
    ) -> Scanner {
        let cfg = config::parse(&format!(
            "stability_secs = 0\nworkers = 1\n[[folders]]\ninput = \"{}\"\noutput = \"{}\"\npattern = \"{{year}}/{{stem}}{{ext}}\"\n",
            input.display(),
            output.display()
        ))
        .unwrap();
        Scanner::new(cfg, state.to_path_buf(), false).unwrap()
    }

    #[test]
    fn once_runs_exactly_one_scan_and_records_it() {
        let (i, o, st) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        std::fs::write(i.path().join("IMG-20250626-WA0002.jpg"), b"x").unwrap();
        let mut s = scanner(i.path(), o.path(), st.path());
        let health = HealthState::new(Default::default(), 30);

        let sum = run(
            &mut s,
            &health,
            &Shutdown::new(),
            Duration::from_secs(3600),
            true,
        );

        assert_eq!(sum.folders[0].organized, 1);
        assert_eq!(health.snapshot().scans_completed, 1);
    }

    #[test]
    fn shutdown_interrupts_the_sleep_between_scans() {
        let (i, o, st) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let mut s = scanner(i.path(), o.path(), st.path());
        let health = HealthState::new(Default::default(), 30);
        let shutdown = Shutdown::new();
        let stopper = shutdown.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stopper.request();
        });

        let start = std::time::Instant::now();
        run(&mut s, &health, &shutdown, Duration::from_secs(3600), false);
        t.join().unwrap();

        assert!(
            start.elapsed() < Duration::from_secs(10),
            "did not wait the full interval"
        );
        assert!(health.snapshot().scans_completed >= 1);
    }
}
