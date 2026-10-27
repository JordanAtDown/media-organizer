//! PHASE 2 — planning (parallel, read-only).

use super::{unix_now, FolderSummary, Observer, Scanner};
use crate::config::FolderConfig;
use crate::date_reader::DateSource;
use crate::plan::{plan_file, Candidate, FilePlan, PlanOutcome};
use rayon::prelude::*;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use tracing::{error, warn};

impl Scanner {
    /// PHASE 2. Parallel and strictly read-only; a panic in one file is contained.
    pub(super) fn plan(
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
