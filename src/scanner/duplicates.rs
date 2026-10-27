//! What becomes of the duplicates phase 3 set aside.

use super::{FolderSummary, Scanner};
use crate::fsops;
use crate::notify::Changes;
use crate::resolve::Duplicate;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

impl Scanner {
    /// Sort the duplicates of phase 3. Those to remove are returned for phase 5
    /// ([`Scanner::dispose_duplicates`]); the others are remembered, or left for
    /// the next scan.
    pub(super) fn triage_duplicates(
        &mut self,
        duplicates: Vec<Duplicate>,
        sum: &mut FolderSummary,
    ) -> Vec<Duplicate> {
        let delete = self.config.dedup.delete_source_duplicates;
        let mut removable = Vec::new();
        for dup in duplicates {
            match (delete, dup.of_on_disk) {
                (true, true) if self.dry_run => {
                    info!(
                        path = %dup.source.display(), existing = %dup.of.display(),
                        "[dry-run] duplicate: would remove the source if still byte-identical"
                    );
                    sum.duplicates += 1;
                }
                (true, true) => removable.push(dup),
                (true, false) => {
                    // `of` is only a name this batch has not published yet. Not
                    // remembered: next scan it is a file on disk and can be verified.
                    info!(
                        path = %dup.source.display(), existing = %dup.of.display(),
                        "duplicate within the batch; source kept until the original is published"
                    );
                    sum.duplicates += 1;
                }
                (false, _) => self.remember_duplicate(dup, sum),
            }
        }
        removable
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
    ///
    /// Each removal is recorded in `changes`: the input directory it left is stale
    /// for whoever indexes it.
    pub(super) fn dispose_duplicates(
        &mut self,
        removable: Vec<Duplicate>,
        shutdown: &AtomicBool,
        sum: &mut FolderSummary,
        changes: &mut Changes,
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
                    changes.deleted_source(&dup.source);
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
}
