//! Orchestrates one scan of every configured folder through the five phases.
//!
//! ```text
//! 1 INVENTORY    sequential   walk, prune, filter          → candidates
//! 2 PLANNING     parallel     date + destination (reads)   → plans
//! 3 RESOLUTION   sequential   duplicates, final names      → resolved   (memory)
//! 4 PREPARATION  parallel     materialize in staging       → entries    (staging only)
//! 5 COMMIT       sequential   rename into `output`         → published  (fast burst)
//! ```
//!
//! **Nothing is written to `output` before phase 5.** Only phases 2 and 4 are
//! parallel and neither mutates shared state, which is why the caches carry no
//! lock.

use crate::config::{Config, FolderConfig};
use crate::date_reader::DateSource;
use crate::dedup::Digester;
use crate::fsops;
use crate::metadata::exif_write::TMP_PREFIX;
use crate::plan::{extension_lower, plan_file, Candidate, FilePlan, PlanOutcome};
use crate::resolve::{resolve, Duplicate, Resolution, Resolved};
use crate::staging::Staging;
use crate::state::{DuplicateCache, HashIndex, NoDateCache};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;

/// Name of a directory never scanned, wherever it appears in an input.
pub const WORK_DIR: &str = ".media-organizer";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Inventory,
    Planning,
    Resolution,
    Preparation,
    Commit,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Phase::Idle => "idle",
            Phase::Inventory => "inventory",
            Phase::Planning => "planning",
            Phase::Resolution => "resolution",
            Phase::Preparation => "preparation",
            Phase::Commit => "commit",
        };
        f.write_str(s)
    }
}

/// Receives progress as a scan runs. `progress` is the heartbeat that lets a
/// supervisor tell a long scan from one wedged on a dead NFS mount.
pub trait Observer: Sync {
    fn phase(&self, _folder: &str, _phase: Phase) {}
    fn progress(&self) {}
}

pub struct NoObserver;
impl Observer for NoObserver {}

/// What happened to one folder during a scan.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FolderSummary {
    pub name: String,
    pub input_exists: bool,
    /// Candidates inventoried this scan.
    pub found: usize,
    pub organized: usize,
    pub duplicates: usize,
    /// Duplicate sources removed (`dedup.delete_source_duplicates`); a subset of `duplicates`.
    pub duplicates_removed: usize,
    pub conflicts_skipped: usize,
    pub no_date: usize,
    pub failed: usize,
    pub panics: usize,
    pub exif_injected: usize,
    pub by_date_source: BTreeMap<String, usize>,
    /// Entries left staged because a commit failed.
    pub pending_in_staging: usize,
    pub last_error: Option<String>,
}

impl FolderSummary {
    fn count_source(&mut self, source: DateSource) {
        let key = match source {
            DateSource::Exif => "exif",
            DateSource::QuickTime => "quicktime",
            DateSource::Filename => "filename",
            DateSource::Mtime => "mtime",
        };
        *self.by_date_source.entry(key.to_string()).or_default() += 1;
    }

    fn fail(&mut self, message: String) {
        self.failed += 1;
        self.last_error = Some(message);
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    pub folders: Vec<FolderSummary>,
}

pub struct Scanner {
    config: Config,
    work_root: PathBuf,
    dry_run: bool,
    state_dir: PathBuf,
    pool: rayon::ThreadPool,
    no_date: NoDateCache,
    duplicates: DuplicateCache,
    digester: Digester,
    /// Dry-run only: what was already reported, so the daemon does not repeat it
    /// every poll. Process memory, never persisted.
    reported: ReportedPlans,
}

impl Scanner {
    pub fn new(config: Config, state_dir: PathBuf, dry_run: bool) -> Result<Self, std::io::Error> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.workers)
            .thread_name(|i| format!("mo-worker-{i}"))
            .build()
            .map_err(std::io::Error::other)?;
        let no_date = NoDateCache::load(
            &state_dir,
            config.no_date_cache.enabled,
            config.no_date_cache.ttl_days,
        );
        let duplicates = DuplicateCache::load(&state_dir);
        let digester = Digester::new(HashIndex::load(&state_dir));
        let work_root = config.work_root(&state_dir);
        Ok(Self {
            config,
            work_root,
            dry_run,
            state_dir,
            pool,
            no_date,
            duplicates,
            digester,
            reported: ReportedPlans::default(),
        })
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Files read and hashed so far (index hits excluded). Stays at zero while
    /// no two files collide on size — the property that keeps hashing cheap.
    pub fn hashes_computed(&self) -> usize {
        self.digester.hashes_computed()
    }

    /// Sizes of the three persistent caches, for `/healthz`.
    pub fn cache_sizes(&self) -> (usize, usize, usize) {
        (
            self.no_date.len(),
            self.duplicates.len(),
            self.digester.index_len(),
        )
    }

    /// Scan every folder once. A folder's failure is recorded in its summary and
    /// never stops the others.
    pub fn scan_once(&mut self, observer: &dyn Observer, shutdown: &AtomicBool) -> ScanSummary {
        let mut summary = ScanSummary::default();
        let folders = self.config.folders.clone();
        for folder in &folders {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            summary
                .folders
                .push(self.scan_folder(folder, observer, shutdown));
        }
        if !self.dry_run {
            self.save_state();
        }
        summary
    }

    fn save_state(&mut self) {
        let results = [
            self.no_date.save_if_dirty(),
            self.duplicates.save_if_dirty(),
            self.digester.save(),
        ];
        for e in results.into_iter().filter_map(Result::err) {
            warn!(error = %e, dir = %self.state_dir.display(), "cannot save state");
        }
    }

    fn scan_folder(
        &mut self,
        folder: &FolderConfig,
        observer: &dyn Observer,
        shutdown: &AtomicBool,
    ) -> FolderSummary {
        let label = folder.label();
        let mut sum = FolderSummary {
            name: label.clone(),
            input_exists: folder.input.is_dir(),
            ..Default::default()
        };
        if !sum.input_exists {
            warn!(folder = %label, path = %folder.input.display(), "input folder missing (mount not ready?)");
            return sum;
        }

        // The staging area is only created for a real run: --dry-run writes nothing.
        let staging = if self.dry_run {
            None
        } else {
            match Staging::new(
                folder.work_subdir(&self.work_root),
                folder.library.clone(),
                folder.file_mode(&self.config.permissions),
                folder.dir_mode(&self.config.permissions),
            ) {
                Ok(s) => Some(s),
                Err(e) => {
                    error!(folder = %label, error = %e, "cannot set up staging");
                    sum.fail(e.to_string());
                    return sum;
                }
            }
        };

        // Finish whatever an interrupted run left behind before starting a new batch.
        if let Some(staging) = &staging {
            match staging.replay() {
                Ok(report) => {
                    sum.organized += report.committed.len();
                    for (entry, message) in &report.failed {
                        warn!(path = %entry.source.display(), error = %message, "replay failed; will retry");
                    }
                }
                Err(e) => {
                    error!(folder = %label, error = %e, "journal replay failed");
                    sum.fail(e.to_string());
                }
            }
        }

        // PHASE 1 — inventory
        observer.phase(&label, Phase::Inventory);
        let candidates = self.inventory(folder, observer);
        sum.found = candidates.len();
        if candidates.is_empty() {
            self.reported.update(&folder.input, &[]);
            self.finish_folder(&mut sum, staging.as_ref(), observer, &label);
            return sum;
        }

        // PHASE 2 — planning (parallel, read-only)
        observer.phase(&label, Phase::Planning);
        let plans = self.plan(candidates, folder, observer, &mut sum);

        // PHASE 3 — resolution (sequential, in memory)
        observer.phase(&label, Phase::Resolution);
        let Resolution {
            mut resolved,
            duplicates,
            skipped,
            failed,
        } = resolve(
            plans,
            &folder.library,
            folder.on_conflict,
            &self.config.dedup,
            &mut self.digester,
        );
        // PHASE 5 (source cleanup) — see `dispose_duplicates`; here they are only
        // sorted into "to remove" and "to remember".
        let mut removable = Vec::new();
        for dup in duplicates {
            if self.config.dedup.delete_source_duplicates && dup.of_on_disk && self.dry_run {
                info!(
                    path = %dup.source.display(), existing = %dup.of.display(),
                    "[dry-run] duplicate: would remove the source if still byte-identical"
                );
                sum.duplicates += 1;
            } else if self.config.dedup.delete_source_duplicates && dup.of_on_disk {
                removable.push(dup);
            } else if self.config.dedup.delete_source_duplicates && !dup.of_on_disk {
                // `of` is only a name this batch has not published yet. Not
                // remembered: next scan it is a file on disk and can be verified.
                info!(
                    path = %dup.source.display(), existing = %dup.of.display(),
                    "duplicate within the batch; source kept until the original is published"
                );
                sum.duplicates += 1;
            } else {
                self.remember_duplicate(dup, &mut sum);
            }
        }
        for (source, existing) in skipped {
            info!(path = %source.display(), existing = %existing.display(), "skipping: destination exists");
            sum.conflicts_skipped += 1;
        }
        for (source, message) in failed {
            warn!(path = %source.display(), error = %message, "cannot resolve; will retry");
            sum.fail(message);
        }

        // A safety valve for a huge first run: the rest waits for the next scan.
        let max = self.config.staging.max_batch;
        if max > 0 && resolved.len() > max {
            info!(
                total = resolved.len(),
                batch = max,
                "batch capped; remainder next scan"
            );
            resolved.truncate(max);
        }

        if self.dry_run {
            let fresh = self.reported.update(&folder.input, &resolved);
            for r in &fresh {
                info!(
                    from = %r.plan.source.display(), to = %r.final_dest.display(),
                    date_source = ?r.plan.capture.source, exif = r.plan.inject_exif,
                    "[dry-run] would move file"
                );
            }
            debug!(
                reported = fresh.len(),
                unchanged = resolved.len() - fresh.len(),
                "[dry-run] plans"
            );
            sum.organized = resolved.len();
            observer.phase(&label, Phase::Idle);
            return sum;
        }
        let Some(staging) = staging else {
            return sum;
        };

        self.dispose_duplicates(removable, shutdown, &mut sum);

        if resolved.is_empty() || shutdown.load(Ordering::SeqCst) {
            self.finish_folder(&mut sum, Some(&staging), observer, &label);
            return sum;
        }

        // PHASE 4 — preparation (parallel, staging only)
        observer.phase(&label, Phase::Preparation);
        let report = self.pool.install(|| staging.prepare(resolved));
        for (source, message) in &report.failed {
            warn!(path = %source.display(), error = %message, "cannot prepare; will retry");
            sum.fail(message.clone());
        }
        observer.progress();

        // Up to here nothing outside staging changed, so stopping is free.
        // Once the journal is written the commit always runs to completion.
        if shutdown.load(Ordering::SeqCst) {
            info!(folder = %label, "shutdown requested before commit; batch discarded");
            self.finish_folder(&mut sum, Some(&staging), observer, &label);
            return sum;
        }

        // PHASE 5 — commit (sequential, fast)
        observer.phase(&label, Phase::Commit);
        if let Err(e) = staging.write_journal(&report.prepared) {
            error!(folder = %label, error = %e, "cannot write journal; batch not committed");
            sum.fail(e.to_string());
            self.finish_folder(&mut sum, Some(&staging), observer, &label);
            return sum;
        }
        match staging.commit(report.prepared) {
            Ok(commit) => {
                for done in &commit.committed {
                    info!(
                        from = %done.source.display(), to = %done.final_dest.display(),
                        date_source = ?done.date_source, "organized"
                    );
                    self.no_date.remove(&done.source);
                    sum.organized += 1;
                    if done.injected_exif {
                        sum.exif_injected += 1;
                    }
                    observer.progress();
                }
                for (entry, message) in &commit.failed {
                    error!(path = %entry.source.display(), error = %message, "commit failed; will retry");
                    sum.fail(message.clone());
                }
            }
            Err(e) => {
                error!(folder = %label, error = %e, "commit bookkeeping failed");
                sum.fail(e.to_string());
            }
        }

        self.finish_folder(&mut sum, Some(&staging), observer, &label);
        sum
    }

    fn remember_duplicate(&mut self, dup: Duplicate, sum: &mut FolderSummary) {
        info!(
            path = %dup.source.display(), existing = %dup.of.display(),
            "duplicate: identical file already organized; leaving the source in place"
        );
        self.duplicates.insert(dup.source, dup.mtime, dup.of);
        sum.duplicates += 1;
    }

    /// PHASE 5, source cleanup (`dedup.delete_source_duplicates`). The only place
    /// a duplicate source is ever deleted, and only after
    /// [`fsops::remove_if_identical`] re-read both files byte for byte, so the
    /// (path, size, mtime) hash index is never what the deletion rests on. The
    /// library is not written. Anything short of a proof keeps the source: a
    /// mismatch is remembered like any duplicate, a read error is retried next scan.
    fn dispose_duplicates(
        &mut self,
        removable: Vec<Duplicate>,
        shutdown: &AtomicBool,
        sum: &mut FolderSummary,
    ) {
        for dup in removable {
            if shutdown.load(Ordering::SeqCst) {
                // Not remembered: the next scan finds it again and decides then.
                return;
            }
            match fsops::remove_if_identical(&dup.source, &dup.of, dup.mtime) {
                Ok(Ok(())) => {
                    info!(
                        path = %dup.source.display(), existing = %dup.of.display(),
                        "duplicate: verified byte-identical to the organized file; source removed"
                    );
                    sum.duplicates += 1;
                    sum.duplicates_removed += 1;
                }
                Ok(Err(kept)) => {
                    warn!(
                        path = %dup.source.display(), existing = %dup.of.display(), reason = ?kept,
                        "duplicate not removed: byte-for-byte check did not confirm it"
                    );
                    self.remember_duplicate(dup, sum);
                }
                Err(e) => {
                    warn!(
                        path = %dup.source.display(), existing = %dup.of.display(), error = %e,
                        "cannot verify duplicate; source kept, will retry"
                    );
                    sum.duplicates += 1;
                }
            }
        }
    }

    fn finish_folder(
        &self,
        sum: &mut FolderSummary,
        staging: Option<&Staging>,
        observer: &dyn Observer,
        label: &str,
    ) {
        sum.pending_in_staging = staging.map_or(0, Staging::pending);
        observer.phase(label, Phase::Idle);
        if sum.found > 0 {
            info!(
                folder = %label, found = sum.found, organized = sum.organized,
                duplicates = sum.duplicates, no_date = sum.no_date, failed = sum.failed,
                "scan complete"
            );
        }
    }

    /// PHASE 1. Sequential walk that **prunes** excluded subtrees instead of
    /// descending into them: on an NFS-mounted DSM volume `@eaDir` holds a
    /// thumbnail per photo, and walking it just to discard it is a round-trip each.
    fn inventory(&self, folder: &FolderConfig, observer: &dyn Observer) -> Vec<Candidate> {
        let work_dir = folder.work_subdir(&self.work_root);
        let work_root = &self.work_root;
        let max_depth = if folder.recursive { usize::MAX } else { 1 };
        let now = unix_now();
        let stability = self.config.stability_secs;
        let mut out = Vec::new();

        let walker = WalkDir::new(&folder.input)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 || !e.file_type().is_dir() {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                name != WORK_DIR
                    && e.path() != work_dir
                    && e.path() != work_root
                    && !folder.excluded_dirs.iter().any(|d| d.as_str() == name)
            });

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "cannot read directory entry");
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(TMP_PREFIX))
            {
                continue;
            }
            let ext = extension_lower(path);
            if !folder.extensions.contains(&ext) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                warn!(path = %path.display(), "cannot stat file");
                continue;
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());

            // Still being written (an upload in progress): try again next scan.
            if mtime <= now && now - mtime < stability {
                debug!(path = %path.display(), "skipping: modified too recently");
                continue;
            }
            if self.no_date.should_skip(path, mtime, now) {
                debug!(path = %path.display(), "skipping: no capture date (cached)");
                continue;
            }
            if self.duplicates.lookup(path, mtime).is_some() {
                debug!(path = %path.display(), "skipping: known duplicate");
                continue;
            }

            out.push(Candidate {
                path: path.to_path_buf(),
                size: meta.len(),
                mtime,
            });
            observer.progress();
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// PHASE 2. Parallel and strictly read-only; a panic in one file is contained.
    fn plan(
        &mut self,
        candidates: Vec<Candidate>,
        folder: &FolderConfig,
        observer: &dyn Observer,
        sum: &mut FolderSummary,
    ) -> Vec<FilePlan> {
        enum Out {
            Plan(Box<FilePlan>),
            NoDate(Candidate),
            Failed(PathBuf, String),
            Panicked(PathBuf),
        }

        let outcomes: Vec<Out> = self.pool.install(|| {
            candidates
                .par_iter()
                .map(|c| {
                    let out = match catch_unwind(AssertUnwindSafe(|| plan_file(c, folder))) {
                        Ok(PlanOutcome::Planned(p)) => Out::Plan(Box::new(p)),
                        Ok(PlanOutcome::NoDate) => Out::NoDate(c.clone()),
                        Ok(PlanOutcome::Unreadable(m)) => Out::Failed(c.path.clone(), m),
                        Err(_) => Out::Panicked(c.path.clone()),
                    };
                    observer.progress();
                    out
                })
                .collect()
        });

        let now = unix_now();
        let mut plans = Vec::new();
        for out in outcomes {
            match out {
                Out::Plan(plan) => {
                    sum.count_source(plan.capture.source);
                    if plan.capture.source == DateSource::Mtime {
                        // The one approximate source: make it visible.
                        warn!(
                            path = %plan.source.display(),
                            date = %plan.capture.date.format("%Y-%m-%d %H:%M:%S"),
                            date_source = "mtime",
                            "no capture date found; using file modification time"
                        );
                    }
                    plans.push(*plan);
                }
                Out::NoDate(c) => {
                    warn!(path = %c.path.display(), "skipping: capture date not found");
                    self.no_date.insert(c.path, c.mtime, now);
                    sum.no_date += 1;
                }
                Out::Failed(path, message) => {
                    warn!(path = %path.display(), error = %message, "cannot read file");
                    sum.fail(message);
                }
                Out::Panicked(path) => {
                    error!(path = %path.display(), "panic while planning; file skipped");
                    sum.panics += 1;
                    sum.fail(format!("panic while planning {}", path.display()));
                }
            }
        }
        plans
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Dry-run memory: a file is reported again only when its plan changed.
///
/// Nothing leaves the input in dry-run, so a daemon would otherwise log the
/// same backlog every poll. The key is the source path; the value is what the
/// plan depends on (mtime and where the file would land). A fresh process
/// reports everything once, which keeps `--dry-run --once` unchanged.
#[derive(Default)]
struct ReportedPlans(std::collections::HashMap<PathBuf, (u64, PathBuf)>);

impl ReportedPlans {
    /// The entries of `resolved` not reported yet (or changed since), recorded as
    /// reported. Entries under `input` that left the plan are forgotten, so the
    /// map stays bounded and a file that comes back is reported again.
    fn update<'a>(&mut self, input: &Path, resolved: &'a [Resolved]) -> Vec<&'a Resolved> {
        let current: std::collections::HashSet<&Path> =
            resolved.iter().map(|r| r.plan.source.as_path()).collect();
        self.0
            .retain(|path, _| !path.starts_with(input) || current.contains(path.as_path()));
        resolved
            .iter()
            .filter(|r| {
                let key = (r.plan.mtime, r.final_dest.clone());
                self.0.insert(r.plan.source.clone(), key.clone()) != Some(key)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
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
        assert!(s.reported.0.is_empty());

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
}
