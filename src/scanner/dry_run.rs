//! Dry-run memory, and what a dry-run reports in place of phases 4-5.

use crate::resolve::Resolved;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// Dry-run memory: a file is reported again only when its plan changed.
///
/// Nothing leaves the input in dry-run, so a daemon would otherwise log the
/// same backlog every poll. The key is the source path; the value is what the
/// plan depends on (mtime and where the file would land). A fresh process
/// reports everything once, which keeps `--dry-run --once` unchanged.
#[derive(Default)]
pub(super) struct ReportedPlans(std::collections::HashMap<PathBuf, (u64, PathBuf)>);

impl ReportedPlans {
    /// The entries of `resolved` not reported yet (or changed since), recorded as
    /// reported. Entries under `input` that left the plan are forgotten, so the
    /// map stays bounded and a file that comes back is reported again.
    pub(super) fn update<'a>(
        &mut self,
        input: &Path,
        resolved: &'a [Resolved],
    ) -> Vec<&'a Resolved> {
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

    /// Log what a dry-run would move: only what is new or changed since the last
    /// report, so a daemon does not repeat the same backlog every poll.
    pub(super) fn report(&mut self, input: &Path, resolved: &[Resolved]) {
        let fresh = self.update(input, resolved);
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
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
