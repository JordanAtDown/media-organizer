//! PHASE 1 — inventory.

use super::{unix_now, Observer, Scanner};
use crate::config::FolderConfig;
use crate::metadata::exif_write::TMP_PREFIX;
use crate::plan::{extension_lower, Candidate};
use std::time::UNIX_EPOCH;
use tracing::{debug, warn};
use walkdir::WalkDir;

/// Name of a directory never scanned, wherever it appears in an input.
pub const WORK_DIR: &str = ".media-organizer";

impl Scanner {
    /// PHASE 1. Sequential walk that **prunes** excluded subtrees instead of
    /// descending into them: on an NFS-mounted DSM volume `@eaDir` holds a
    /// thumbnail per photo, and walking it just to discard it is a round-trip each.
    pub(super) fn inventory(
        &self,
        folder: &FolderConfig,
        observer: &dyn Observer,
    ) -> Vec<Candidate> {
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
}
