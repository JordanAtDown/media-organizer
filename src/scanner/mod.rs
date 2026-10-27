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
//!
//! Once a folder's pipeline is done, what it changed is announced (see
//! [`crate::notify`]). That is the one write outside the phases, and it never
//! goes inside a library.
//!
//! This file only knows the order. Each phase's work lives in its own module:
//! [`inventory`], [`planning`], `duplicates` (phase 3's leftovers, removed in
//! phase 5); the resolution and commit logic are in `resolve` and `staging`.

mod dry_run;
mod duplicates;
mod inventory;
mod planning;
mod report;
#[cfg(test)]
mod tests;

pub use inventory::WORK_DIR;
pub use report::{FolderSummary, NoObserver, Observer, Phase, ScanSummary};

use crate::config::{Config, FolderConfig};
use crate::dedup::Digester;
use crate::error::StagingError;
use crate::notify::{Changes, LastEvent, Notifier};
use crate::resolve::{resolve, Resolution};
use crate::staging::{Entry, Staging};
use crate::state::{DuplicateCache, HashIndex, NoDateCache};
use dry_run::ReportedPlans;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};

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
    /// `None` unless `[notify]` configures a sink, and always `None` in dry-run.
    notifier: Option<Notifier>,
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
        // Dry-run writes nothing anywhere: no event, not even the counter.
        let notifier = if dry_run {
            None
        } else {
            Notifier::new(&config.notify, &state_dir)
        };
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
            notifier,
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

    /// The last batch announced per folder, as recorded by earlier runs: what
    /// `/healthz` reports until this process announces one of its own.
    pub fn last_events(&self) -> BTreeMap<String, LastEvent> {
        self.notifier
            .as_ref()
            .map(Notifier::last_events)
            .unwrap_or_default()
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

    /// One folder, start to finish, then its announcement. What the pipeline got
    /// through is handed to the notifier even if it stopped early: files already
    /// published are a fact for the consumer whatever happened next. The notifier is
    /// consulted after *every* scan, changes or not: a quiet scan is what lets a
    /// held announcement's quiet period elapse.
    fn scan_folder(
        &mut self,
        folder: &FolderConfig,
        observer: &dyn Observer,
        shutdown: &AtomicBool,
    ) -> FolderSummary {
        let mut changes = Changes::new(&folder.input, &folder.library);
        let mut sum = self.run_pipeline(folder, observer, shutdown, &mut changes);
        if let Some(notifier) = &mut self.notifier {
            sum.announced = notifier.tick(folder, changes, unix_now());
        }
        sum
    }

    /// The five phases for one folder. Everything that reaches a library or leaves
    /// an input is recorded in `changes`.
    fn run_pipeline(
        &mut self,
        folder: &FolderConfig,
        observer: &dyn Observer,
        shutdown: &AtomicBool,
        changes: &mut Changes,
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

        let staging = match self.open_staging(folder) {
            Ok(staging) => staging,
            Err(e) => {
                error!(folder = %label, error = %e, "cannot set up staging");
                sum.fail(e.to_string());
                return sum;
            }
        };

        // Finish whatever an interrupted run left behind before starting a new batch.
        if let Some(staging) = &staging {
            Self::replay_leftovers(staging, &label, &mut sum, changes);
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
        let removable = self.triage_duplicates(duplicates, &mut sum);
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
            self.reported.report(&folder.input, &resolved);
            sum.organized = resolved.len();
            observer.phase(&label, Phase::Idle);
            return sum;
        }
        let Some(staging) = staging else {
            return sum;
        };

        self.dispose_duplicates(removable, shutdown, &mut sum, changes);

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
        self.commit_batch(
            &staging,
            report.prepared,
            &label,
            observer,
            &mut sum,
            changes,
        );
        self.finish_folder(&mut sum, Some(&staging), observer, &label);
        sum
    }

    /// The folder's staging area; `None` in dry-run, which writes nothing.
    fn open_staging(&self, folder: &FolderConfig) -> Result<Option<Staging>, StagingError> {
        if self.dry_run {
            return Ok(None);
        }
        Staging::new(
            folder.work_subdir(&self.work_root),
            folder.library.clone(),
            folder.file_mode(&self.config.permissions),
            folder.dir_mode(&self.config.permissions),
        )
        .map(Some)
    }

    /// Commit what a previous, interrupted run journaled but never finished.
    fn replay_leftovers(
        staging: &Staging,
        label: &str,
        sum: &mut FolderSummary,
        changes: &mut Changes,
    ) {
        match staging.replay() {
            Ok(report) => {
                sum.organized += report.committed.len();
                for (source, dest) in report.published() {
                    changes.committed(source, dest);
                }
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

    /// PHASE 5. The journal is written first, so from there the batch can always
    /// be finished; each published file is recorded in `changes`.
    fn commit_batch(
        &mut self,
        staging: &Staging,
        prepared: Vec<Entry>,
        label: &str,
        observer: &dyn Observer,
        sum: &mut FolderSummary,
        changes: &mut Changes,
    ) {
        observer.phase(label, Phase::Commit);
        if let Err(e) = staging.write_journal(&prepared) {
            error!(folder = %label, error = %e, "cannot write journal; batch not committed");
            sum.fail(e.to_string());
            return;
        }
        match staging.commit(prepared) {
            Ok(commit) => {
                for done in &commit.committed {
                    info!(
                        from = %done.source.display(), to = %done.final_dest.display(),
                        date_source = ?done.date_source, "organized"
                    );
                    self.no_date.remove(&done.source);
                    changes.committed(&done.source, &done.final_dest);
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
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
