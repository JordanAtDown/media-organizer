use crate::config::HealthConfig;
use crate::scanner::{FolderSummary, Observer, Phase, ScanSummary};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Never call a scan stale for being quiet less than this long.
const STALE_FLOOR_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Nothing completed yet, but the daemon is still inside its grace period
    /// (or is making progress through a long first scan).
    Starting,
    Ok,
    /// Working, but something needs a look: a missing input, entries stuck in staging.
    Degraded,
    /// No progress for too long — typically a scan wedged on a hung NFS mount.
    Stale,
}

impl Status {
    /// HTTP status code for `/healthz`. Only `stale` is a failure.
    pub fn http_code(self) -> u16 {
        match self {
            Status::Stale => 503,
            _ => 200,
        }
    }
}

/// Everything the status decision depends on, so it is a pure function.
#[derive(Debug, Clone, Copy)]
pub struct Inputs {
    pub now: u64,
    pub uptime_secs: u64,
    pub scans_completed: u64,
    pub last_scan_end: u64,
    /// Heartbeat of the most recent unit of work (0 = none yet).
    pub last_progress: u64,
    pub poll_interval_secs: u64,
    pub startup_grace_secs: u64,
    pub stale_intervals: u64,
    pub degraded: bool,
}

/// The staleness rule.
///
/// `stale_after = max(60, poll_interval * stale_intervals)` and activity is the
/// later of "a scan finished" and "a file made progress". Using the heartbeat
/// rather than only the last *finished* scan is what keeps a legitimately long
/// scan green while a scan stuck on a dead `stat()` goes red.
pub fn evaluate(i: Inputs) -> Status {
    let stale_after = STALE_FLOOR_SECS.max(i.poll_interval_secs.saturating_mul(i.stale_intervals));
    let quiet_for = |t: u64| i.now.saturating_sub(t);

    if i.scans_completed == 0 {
        let in_grace = i.uptime_secs < i.startup_grace_secs;
        let progressing = i.last_progress > 0 && quiet_for(i.last_progress) <= stale_after;
        return if in_grace || progressing {
            Status::Starting
        } else {
            Status::Stale
        };
    }

    let last_activity = i.last_scan_end.max(i.last_progress);
    if quiet_for(last_activity) > stale_after {
        Status::Stale
    } else if i.degraded {
        Status::Degraded
    } else {
        Status::Ok
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Totals {
    pub organized: u64,
    pub duplicates: u64,
    pub conflicts_skipped: u64,
    pub no_date: u64,
    pub failed: u64,
    pub panics: u64,
    pub exif_injected: u64,
    pub by_date_source: BTreeMap<String, u64>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Caches {
    pub no_date: usize,
    pub duplicates: usize,
    pub hash_index: usize,
}

#[derive(Default)]
struct Inner {
    phase: Option<Phase>,
    current_folder: Option<String>,
    scan_started: u64,
    scan_finished: u64,
    scan_duration_secs: u64,
    scans_completed: u64,
    totals: Totals,
    caches: Caches,
    folders: Vec<FolderSummary>,
}

/// Shared between the scan loop (writer) and the HTTP thread (reader).
///
/// The per-file hot path touches only one relaxed atomic; the mutex is taken
/// once per phase change and once per finished scan.
pub struct HealthState {
    started: SystemTime,
    poll_interval_secs: u64,
    cfg: HealthConfig,
    last_progress: AtomicU64,
    inner: Mutex<Inner>,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub status: Status,
    pub version: &'static str,
    pub uptime_secs: u64,
    pub phase: String,
    pub current_folder: Option<String>,
    pub scans_completed: u64,
    pub last_scan: LastScan,
    pub totals: Totals,
    pub pending_in_staging: usize,
    pub caches: Caches,
    pub folders: Vec<FolderSummary>,
}

#[derive(Debug, Serialize)]
pub struct LastScan {
    pub started_at: u64,
    pub finished_at: u64,
    pub duration_secs: u64,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl HealthState {
    pub fn new(cfg: HealthConfig, poll_interval_secs: u64) -> Self {
        Self {
            started: SystemTime::now(),
            poll_interval_secs,
            cfg,
            last_progress: AtomicU64::new(0),
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Call when a scan begins.
    pub fn begin_scan(&self) {
        self.lock().scan_started = unix_now();
        self.last_progress.store(unix_now(), Ordering::Relaxed);
    }

    /// Fold a finished scan into the running totals.
    pub fn record_scan(&self, summary: &ScanSummary, caches: Caches) {
        let now = unix_now();
        let mut g = self.lock();
        for f in &summary.folders {
            let t = &mut g.totals;
            t.organized += f.organized as u64;
            t.duplicates += f.duplicates as u64;
            t.conflicts_skipped += f.conflicts_skipped as u64;
            t.no_date += f.no_date as u64;
            t.failed += f.failed as u64;
            t.panics += f.panics as u64;
            t.exif_injected += f.exif_injected as u64;
            for (source, n) in &f.by_date_source {
                *t.by_date_source.entry(source.clone()).or_default() += *n as u64;
            }
        }
        g.scans_completed += 1;
        g.scan_finished = now;
        g.scan_duration_secs = now.saturating_sub(g.scan_started);
        g.caches = caches;
        g.folders = summary.folders.clone();
        g.phase = Some(Phase::Idle);
        g.current_folder = None;
        self.last_progress.store(now, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_at(unix_now())
    }

    pub fn snapshot_at(&self, now: u64) -> Snapshot {
        let g = self.lock();
        let pending: usize = g.folders.iter().map(|f| f.pending_in_staging).sum();
        let degraded = pending > 0 || g.folders.iter().any(|f| !f.input_exists);
        let uptime = self.started.elapsed().map_or(0, |d| d.as_secs());

        let status = evaluate(Inputs {
            now,
            uptime_secs: uptime,
            scans_completed: g.scans_completed,
            last_scan_end: g.scan_finished,
            last_progress: self.last_progress.load(Ordering::Relaxed),
            poll_interval_secs: self.poll_interval_secs,
            startup_grace_secs: self.cfg.startup_grace_secs,
            stale_intervals: self.cfg.stale_intervals,
            degraded,
        });

        Snapshot {
            status,
            version: env!("CARGO_PKG_VERSION"),
            uptime_secs: uptime,
            phase: g.phase.unwrap_or(Phase::Idle).to_string(),
            current_folder: g.current_folder.clone(),
            scans_completed: g.scans_completed,
            last_scan: LastScan {
                started_at: g.scan_started,
                finished_at: g.scan_finished,
                duration_secs: g.scan_duration_secs,
            },
            totals: g.totals.clone(),
            pending_in_staging: pending,
            caches: g.caches.clone(),
            folders: g.folders.clone(),
        }
    }
}

impl Observer for HealthState {
    fn phase(&self, folder: &str, phase: Phase) {
        let mut g = self.lock();
        g.phase = Some(phase);
        g.current_folder = (phase != Phase::Idle).then(|| folder.to_string());
        drop(g);
        self.last_progress.store(unix_now(), Ordering::Relaxed);
    }

    fn progress(&self) {
        self.last_progress.store(unix_now(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn inputs() -> Inputs {
        Inputs {
            now: 10_000,
            uptime_secs: 5_000,
            scans_completed: 5,
            last_scan_end: 9_990,
            last_progress: 9_990,
            poll_interval_secs: 30,
            startup_grace_secs: 120,
            stale_intervals: 3,
            degraded: false,
        }
    }

    #[rstest]
    // ── before the first scan finishes ──
    #[case("in grace", |i: &mut Inputs| { i.scans_completed = 0; i.uptime_secs = 10; i.last_progress = 0; }, Status::Starting)]
    #[case("past grace, silent", |i: &mut Inputs| { i.scans_completed = 0; i.last_progress = 0; }, Status::Stale)]
    #[case("long first scan still beating", |i: &mut Inputs| { i.scans_completed = 0; i.last_progress = 9_980; }, Status::Starting)]
    #[case("first scan beat stopped", |i: &mut Inputs| { i.scans_completed = 0; i.last_progress = 1_000; }, Status::Stale)]
    // ── steady state ──
    #[case("healthy", |_: &mut Inputs| {}, Status::Ok)]
    #[case("idle between scans, within window", |i: &mut Inputs| { i.last_scan_end = 9_920; i.last_progress = 9_920; }, Status::Ok)]
    #[case("idle too long", |i: &mut Inputs| { i.last_scan_end = 9_000; i.last_progress = 9_000; }, Status::Stale)]
    // The decisive case: a scan is running (last_scan_end is old) but its
    // heartbeat froze — a stat() blocked on a dead NFS mount.
    #[case("scan running, heartbeat frozen", |i: &mut Inputs| { i.last_scan_end = 9_000; i.last_progress = 9_500; }, Status::Stale)]
    // ...while a long scan that keeps beating stays green.
    #[case("scan running, heartbeat alive", |i: &mut Inputs| { i.last_scan_end = 9_000; i.last_progress = 9_995; }, Status::Ok)]
    #[case("degraded but alive", |i: &mut Inputs| { i.degraded = true; }, Status::Degraded)]
    #[case("stale outranks degraded", |i: &mut Inputs| { i.degraded = true; i.last_scan_end = 1; i.last_progress = 1; }, Status::Stale)]
    // ── the 60 s floor ──
    #[case("short poll still gets the floor", |i: &mut Inputs| { i.poll_interval_secs = 1; i.stale_intervals = 3; i.last_scan_end = 9_950; i.last_progress = 9_950; }, Status::Ok)]
    fn status_rules(#[case] _name: &str, #[case] tweak: fn(&mut Inputs), #[case] expected: Status) {
        let mut i = inputs();
        tweak(&mut i);
        assert_eq!(evaluate(i), expected, "{_name}");
    }

    #[test]
    fn only_stale_is_a_failure_code() {
        assert_eq!(Status::Stale.http_code(), 503);
        for s in [Status::Ok, Status::Starting, Status::Degraded] {
            assert_eq!(s.http_code(), 200);
        }
    }

    fn state() -> HealthState {
        HealthState::new(HealthConfig::default(), 30)
    }

    fn folder(name: &str, organized: usize, exists: bool, pending: usize) -> FolderSummary {
        FolderSummary {
            name: name.into(),
            input_exists: exists,
            organized,
            pending_in_staging: pending,
            ..Default::default()
        }
    }

    #[test]
    fn fresh_state_is_starting() {
        let s = state().snapshot();
        assert_eq!(s.status, Status::Starting);
        assert_eq!(s.scans_completed, 0);
    }

    #[test]
    fn recorded_scan_turns_ok_and_accumulates_totals() {
        let h = state();
        h.begin_scan();
        let mut f = folder("phone", 3, true, 0);
        f.by_date_source.insert("filename".into(), 3);
        f.no_date = 1;
        let scan = ScanSummary { folders: vec![f] };
        h.record_scan(&scan, Caches::default());
        h.record_scan(&scan, Caches::default());

        let s = h.snapshot();
        assert_eq!(s.status, Status::Ok);
        assert_eq!(s.scans_completed, 2);
        assert_eq!(s.totals.organized, 6);
        assert_eq!(s.totals.no_date, 2);
        assert_eq!(s.totals.by_date_source["filename"], 6);
    }

    #[test]
    fn stuck_staging_entries_make_it_degraded() {
        let h = state();
        h.record_scan(
            &ScanSummary {
                folders: vec![folder("p", 0, true, 4)],
            },
            Caches::default(),
        );
        let s = h.snapshot();
        assert_eq!(s.status, Status::Degraded);
        assert_eq!(s.pending_in_staging, 4);
    }

    #[test]
    fn a_missing_input_makes_it_degraded() {
        let h = state();
        h.record_scan(
            &ScanSummary {
                folders: vec![folder("p", 0, false, 0)],
            },
            Caches::default(),
        );
        assert_eq!(h.snapshot().status, Status::Degraded);
    }

    #[test]
    fn a_frozen_heartbeat_during_a_scan_is_stale_through_the_real_state() {
        let h = state();
        h.record_scan(
            &ScanSummary {
                folders: vec![folder("p", 0, true, 0)],
            },
            Caches::default(),
        );
        h.phase("p", Phase::Inventory);
        // Jump the clock past the window with no further progress.
        let far = unix_now() + 3_600;
        assert_eq!(h.snapshot_at(far).status, Status::Stale);
    }

    #[test]
    fn observer_reports_the_current_phase_and_folder() {
        let h = state();
        h.phase("phone", Phase::Planning);
        let s = h.snapshot();
        assert_eq!(s.phase, "planning");
        assert_eq!(s.current_folder.as_deref(), Some("phone"));
        h.phase("phone", Phase::Idle);
        assert_eq!(h.snapshot().current_folder, None);
    }

    #[test]
    fn snapshot_serializes_to_json() {
        let json = serde_json::to_value(state().snapshot()).unwrap();
        assert_eq!(json["status"], "starting");
        assert!(json["version"].is_string());
        assert!(json["totals"]["by_date_source"].is_object());
    }
}
