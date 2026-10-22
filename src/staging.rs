//! PHASE 4 — preparation, then PHASE 5 — commit.
//!
//! Phase 4 materializes every resolved file in the staging area: a hard link
//! when possible, a copy when the EXIF must be rewritten. **Nothing outside the
//! staging area is touched.**
//!
//! Phase 5 publishes in two steps so a library scanner never sees anything
//! half-done:
//!
//! * **5a — place**: each file is moved (free on one filesystem) or copied (across
//!   mounts) next to its destination under a hidden temporary name,
//!   `.<name>.mo-part`. Its extension is not a media one, so a library scanner
//!   such as Immich does not index it. This is the slow part.
//! * **5b — publish**: once every file is in place, each one is linked to its final
//!   name — atomically, without ever clobbering an existing file — in one quick
//!   burst of metadata operations. A file therefore appears complete or not at all.

use crate::config::Mode;
use crate::date_reader::DateSource;
use crate::error::StagingError;
use crate::fsops::{self, Materialized};
use crate::metadata::exif_write;
use crate::naming::with_suffix;
use crate::resolve::Resolved;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Name of the replay journal inside the staging directory.
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// Upper bound on `_N` suffixes tried when an outside writer took the name.
const MAX_SUFFIX: u32 = 9999;

/// A file fully prepared in staging and ready to be committed.
///
/// This is also the journal line: everything phase 5 needs, so a crash between
/// the phases can be replayed from disk alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub staged: PathBuf,
    pub final_dest: PathBuf,
    pub source: PathBuf,
    pub overwrite: bool,
    pub file_mode: Option<u32>,
    pub dir_mode: Option<u32>,
    pub date_source: DateSource,
    pub injected_exif: bool,
}

#[derive(Debug, Default)]
pub struct PrepareReport {
    pub prepared: Vec<Entry>,
    /// `(source, message)` — the source is untouched and retried next scan.
    pub failed: Vec<(PathBuf, String)>,
}

/// A file published to its final destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub source: PathBuf,
    /// Where it really landed (differs from the planned path only if an outside
    /// writer took the name in between).
    pub final_dest: PathBuf,
    pub date_source: DateSource,
    pub injected_exif: bool,
}

#[derive(Debug, Default)]
pub struct CommitReport {
    pub committed: Vec<Committed>,
    /// Entries that could not be published. They stay staged and journaled and
    /// are retried on the next scan; the rest of the batch is unaffected.
    pub failed: Vec<(Entry, String)>,
}

/// Hidden temporary name next to the destination: `dir/.<name>.mo-part`.
///
/// Its extension is not a media one, so a library scanner ignores it.
pub fn temp_path(final_dest: &Path) -> PathBuf {
    let name = final_dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    final_dest.with_file_name(format!(".{name}.mo-part"))
}

pub struct Staging {
    dir: PathBuf,
    output: PathBuf,
    file_mode: Option<Mode>,
    dir_mode: Option<Mode>,
}

impl Staging {
    /// Create the staging directory. It may live on any filesystem: publication
    /// moves a file next to its destination first, copying when mounts differ.
    pub fn new(
        dir: PathBuf,
        output: PathBuf,
        file_mode: Option<Mode>,
        dir_mode: Option<Mode>,
    ) -> Result<Self, StagingError> {
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |source| StagingError::Io { path, source }
        };
        std::fs::create_dir_all(&dir).map_err(io(&dir))?;
        std::fs::create_dir_all(&output).map_err(io(&output))?;

        Ok(Self {
            dir,
            output,
            file_mode,
            dir_mode,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn output(&self) -> &Path {
        &self.output
    }

    /// Staged file name: derived from the destination so it is unique within
    /// the batch and keeps the extension (`little_exif` needs it).
    fn staged_path(&self, final_dest: &Path) -> PathBuf {
        let digest = blake3::hash(final_dest.as_os_str().as_encoded_bytes());
        let name = &digest.to_hex()[..24];
        match final_dest.extension().and_then(|e| e.to_str()) {
            Some(ext) => self.dir.join(format!("{name}.{ext}")),
            None => self.dir.join(name),
        }
    }

    /// PHASE 4. Runs on the current rayon pool; one file's failure — or panic —
    /// never affects the others.
    pub fn prepare(&self, resolved: Vec<Resolved>) -> PrepareReport {
        let results: Vec<_> = resolved
            .into_par_iter()
            .map(|item| {
                let source = item.plan.source.clone();
                let outcome = catch_unwind(AssertUnwindSafe(|| self.prepare_one(&item)))
                    .unwrap_or_else(|_| Err("panicked while preparing".to_string()));
                (source, outcome)
            })
            .collect();

        let mut report = PrepareReport::default();
        for (source, outcome) in results {
            match outcome {
                Ok(entry) => report.prepared.push(entry),
                Err(message) => report.failed.push((source, message)),
            }
        }
        // `par_iter().collect()` preserves order, but sort anyway: callers and
        // the journal rely on a stable order.
        report.prepared.sort_by(|a, b| a.source.cmp(&b.source));
        report
    }

    fn prepare_one(&self, item: &Resolved) -> Result<Entry, String> {
        let staged = self.staged_path(&item.final_dest);
        // A leftover from an interrupted run would make the link fail.
        let _ = std::fs::remove_file(&staged);

        let source = &item.plan.source;
        let materialized = if item.plan.inject_exif {
            // The staged copy gets rewritten: it must not share the source's inode.
            fsops::copy_file(source, &staged)
        } else {
            fsops::link_or_copy(source, &staged)
        }
        .map_err(|e| format!("cannot stage {}: {e}", source.display()))?;

        let mut injected = false;
        if item.plan.inject_exif {
            let result = exif_write::write_exif_date(
                &staged,
                &item.plan.capture.date,
                self.file_mode.map(|m| m.0),
            );
            match result {
                Ok(()) => injected = true,
                // Non-fatal, as before: the file is still organized, just without the tag.
                Err(e) => tracing::warn!(
                    path = %source.display(), error = %e,
                    "failed to write EXIF date; organizing without it"
                ),
            }
        }
        debug_assert!(matches!(
            materialized,
            Materialized::Linked | Materialized::Copied
        ));

        Ok(Entry {
            staged,
            final_dest: item.final_dest.clone(),
            source: source.clone(),
            overwrite: item.overwrite,
            file_mode: self.file_mode.map(|m| m.0),
            dir_mode: self.dir_mode.map(|m| m.0),
            date_source: item.plan.capture.source,
            injected_exif: injected,
        })
    }

    // -----------------------------------------------------------------------
    // Journal
    // -----------------------------------------------------------------------

    fn journal_path(&self) -> PathBuf {
        self.dir.join(JOURNAL_FILE)
    }

    /// Persist the prepared entries, atomically, **before** the commit starts.
    /// An empty list removes the journal.
    pub fn write_journal(&self, entries: &[Entry]) -> Result<(), StagingError> {
        let path = self.journal_path();
        let io = |source| StagingError::Io {
            path: path.clone(),
            source,
        };
        if entries.is_empty() {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io(e)),
            };
        }
        let tmp = path.with_extension("jsonl.tmp");
        let mut buf = Vec::new();
        for entry in entries {
            serde_json::to_writer(&mut buf, entry)
                .map_err(|e| StagingError::Journal(e.to_string()))?;
            buf.push(b'\n');
        }
        let mut file = std::fs::File::create(&tmp).map_err(io)?;
        file.write_all(&buf).map_err(io)?;
        file.sync_all().map_err(io)?;
        std::fs::rename(&tmp, &path).map_err(io)
    }

    /// Entries still waiting in the journal. A malformed line is skipped with a
    /// warning rather than blocking every other entry.
    pub fn read_journal(&self) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(self.journal_path()) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| match serde_json::from_str(line) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    warn!(error = %e, "skipping malformed journal line");
                    None
                }
            })
            .collect()
    }

    /// Entries staged but not yet published: non-zero for long means a commit
    /// keeps failing.
    pub fn pending(&self) -> usize {
        self.read_journal().len()
    }

    // -----------------------------------------------------------------------
    // PHASE 5 — commit
    // -----------------------------------------------------------------------

    /// Publish the entries: place everything (5a), then publish in a burst (5b).
    ///
    /// A failure is reported and the batch **continues** — there is no global
    /// rollback, so one bad file can never stall the others. Failed entries stay
    /// journaled and are retried on the next scan.
    pub fn commit(&self, entries: Vec<Entry>) -> Result<CommitReport, StagingError> {
        let mut report = CommitReport::default();

        // 5a — place: the slow, invisible part.
        let mut ready = Vec::with_capacity(entries.len());
        for entry in entries {
            match self.place_temp(&entry) {
                Ok(()) => ready.push(entry),
                Err(message) => {
                    // A half-copied temp file is worthless: drop it.
                    if entry.staged.exists() {
                        let _ = std::fs::remove_file(temp_path(&entry.final_dest));
                    }
                    report.failed.push((entry, message));
                }
            }
        }

        // 5b — publish: one quick burst.
        for entry in ready {
            match self.publish(&entry) {
                Ok(done) => report.committed.push(done),
                Err(message) => report.failed.push((entry, message)),
            }
        }

        let remaining: Vec<Entry> = report.failed.iter().map(|(e, _)| e.clone()).collect();
        self.write_journal(&remaining)?;
        Ok(report)
    }

    /// Step 5a: put the file next to its destination under its temporary name,
    /// with the final mode already applied.
    fn place_temp(&self, entry: &Entry) -> Result<(), String> {
        let temp = temp_path(&entry.final_dest);
        let parent = entry.final_dest.parent().unwrap_or(&self.output);
        fsops::ensure_dir_chain(&self.output, parent, entry.dir_mode.map(Mode))
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;

        // Already placed by an earlier, interrupted run.
        if !entry.staged.exists() && temp.exists() {
            return Ok(());
        }
        // Same filesystem: a free rename. Across mounts it fails (EXDEV): copy.
        if std::fs::rename(&entry.staged, &temp).is_err() {
            std::fs::copy(&entry.staged, &temp)
                .map_err(|e| format!("cannot copy to {}: {e}", temp.display()))?;
        }
        fsops::apply_mode(&temp, entry.file_mode.map(Mode))
            .map_err(|e| format!("cannot set the mode of {}: {e}", temp.display()))
    }

    /// Step 5b: give the placed file its final name, then remove the source —
    /// only now, once the destination is complete.
    fn publish(&self, entry: &Entry) -> Result<Committed, String> {
        let temp = temp_path(&entry.final_dest);
        let target = if entry.overwrite {
            std::fs::rename(&temp, &entry.final_dest)
                .map_err(|e| format!("cannot publish {}: {e}", entry.final_dest.display()))?;
            entry.final_dest.clone()
        } else {
            self.link_into_place(&temp, &entry.final_dest)?
        };

        // After a copy the staged file is still here; after a rename it is gone.
        let _ = std::fs::remove_file(&entry.staged);
        if let Err(e) = std::fs::remove_file(&entry.source) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(path = %entry.source.display(), error = %e,
                      "published but could not remove the source");
            }
        }
        debug!(from = %entry.source.display(), to = %target.display(), "committed");

        Ok(Committed {
            source: entry.source.clone(),
            final_dest: target,
            date_source: entry.date_source,
            injected_exif: entry.injected_exif,
        })
    }

    /// Link `temp` to the first free name, then drop the temporary name.
    ///
    /// `link()` is atomic and fails if the name exists, so an *outside* writer
    /// (Nextcloud dropping a file mid-scan) is never overwritten, and the file
    /// is complete the instant it appears. Where hard links are unsupported it
    /// falls back to reserving the name with an empty file and renaming over it.
    fn link_into_place(&self, temp: &Path, dest: &Path) -> Result<PathBuf, String> {
        for attempt in 0..=MAX_SUFFIX {
            let candidate = if attempt == 0 {
                dest.to_path_buf()
            } else {
                with_suffix(dest, attempt)
            };
            match std::fs::hard_link(temp, &candidate) {
                Ok(()) => {
                    let _ = std::fs::remove_file(temp);
                    return Ok(candidate);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return self.rename_over_reservation(temp, dest),
            }
        }
        Err(format!("no free name for {}", dest.display()))
    }

    /// Fallback when hard links are unavailable: reserve a free name with
    /// `create_new`, then `rename` the file over that reservation.
    fn rename_over_reservation(&self, temp: &Path, dest: &Path) -> Result<PathBuf, String> {
        let target = self.claim_name(dest)?;
        if let Err(e) = std::fs::rename(temp, &target) {
            let _ = std::fs::remove_file(&target);
            return Err(format!("cannot publish {}: {e}", target.display()));
        }
        Ok(target)
    }

    /// Atomically create an empty file at the first free name (`create_new`).
    fn claim_name(&self, dest: &Path) -> Result<PathBuf, String> {
        for attempt in 0..=MAX_SUFFIX {
            let candidate = if attempt == 0 {
                dest.to_path_buf()
            } else {
                with_suffix(dest, attempt)
            };
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(_) => return Ok(candidate),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot reserve {}: {e}", candidate.display())),
            }
        }
        Err(format!("no free name for {}", dest.display()))
    }

    /// Finish whatever an interrupted run left behind.
    ///
    /// Each journaled entry whose staged file (or placed temporary file) still
    /// exists is committed; one with neither was already published and is dropped.
    /// Staged files that no journal line refers to (a crash during phase 4)
    /// are purged — their sources were never touched.
    pub fn replay(&self) -> Result<CommitReport, StagingError> {
        let (live, gone): (Vec<_>, Vec<_>) = self
            .read_journal()
            .into_iter()
            .partition(|e| e.staged.exists() || temp_path(&e.final_dest).exists());
        if !gone.is_empty() {
            debug!(
                entries = gone.len(),
                "dropping journal entries already published"
            );
        }
        let report = self.commit(live)?;
        self.purge_orphans();
        Ok(report)
    }

    /// Remove every file in the staging dir that the journal does not reference.
    fn purge_orphans(&self) {
        let keep: std::collections::HashSet<PathBuf> =
            self.read_journal().into_iter().map(|e| e.staged).collect();
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for item in dir.flatten() {
            let path = item.path();
            let is_journal = path.file_name().is_some_and(|n| n == JOURNAL_FILE);
            if !is_journal && !keep.contains(&path) && path.is_file() {
                debug!(path = %path.display(), "purging orphaned staged file");
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::date_reader::CaptureInfo;
    use crate::plan::FilePlan;
    use chrono::{Local, TimeZone};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    const MIN_JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xD9];

    struct Fixture {
        input: tempfile::TempDir,
        output: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                input: tempfile::tempdir().unwrap(),
                output: tempfile::tempdir().unwrap(),
            }
        }

        fn staging(&self, file: Option<u32>, dir: Option<u32>) -> Staging {
            Staging::new(
                self.output.path().join(".media-organizer/staging"),
                self.output.path().to_path_buf(),
                file.map(Mode),
                dir.map(Mode),
            )
            .unwrap()
        }

        fn resolved(&self, name: &str, content: &[u8], rel: &str, inject: bool) -> Resolved {
            let source = self.input.path().join(name);
            std::fs::write(&source, content).unwrap();
            Resolved {
                plan: FilePlan {
                    source,
                    size: content.len() as u64,
                    mtime: 1,
                    capture: CaptureInfo {
                        date: Local
                            .with_ymd_and_hms(2025, 6, 26, 12, 0, 0)
                            .single()
                            .unwrap(),
                        source: DateSource::Filename,
                        camera: None,
                    },
                    rel_dest: PathBuf::from(rel),
                    inject_exif: inject,
                },
                final_dest: self.output.path().join(rel),
                overwrite: false,
            }
        }
    }

    fn tree(p: &Path) -> Vec<PathBuf> {
        let mut v: Vec<_> = walkdir::WalkDir::new(p)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn new_creates_the_staging_dir() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        assert!(s.dir().is_dir());
    }

    #[test]
    fn prepare_links_without_touching_output_or_source() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let item = f.resolved("a.jpg", b"photo", "2025/06/a.jpg", false);
        let source = item.plan.source.clone();
        let before = tree(f.output.path());

        let report = s.prepare(vec![item]);

        assert_eq!(report.prepared.len(), 1);
        let entry = &report.prepared[0];
        assert!(entry.staged.starts_with(s.dir()));
        assert_eq!(std::fs::read(&entry.staged).unwrap(), b"photo");
        assert_eq!(
            std::fs::metadata(&source).unwrap().nlink(),
            2,
            "linked, not copied"
        );
        // Only new entries are inside the staging dir; nothing else changed.
        let after: Vec<_> = tree(f.output.path())
            .into_iter()
            .filter(|p| !p.starts_with(s.dir()))
            .collect();
        let before: Vec<_> = before
            .into_iter()
            .filter(|p| !p.starts_with(s.dir()))
            .collect();
        assert_eq!(after, before);
        assert!(!f.output.path().join("2025").exists());
    }

    #[test]
    fn prepare_copies_and_injects_exif_leaving_the_source_untouched() {
        let f = Fixture::new();
        let s = f.staging(Some(0o666), None);
        let item = f.resolved("a.jpg", MIN_JPEG, "2025/06/a.jpg", true);
        let source = item.plan.source.clone();

        let report = s.prepare(vec![item]);
        let entry = &report.prepared[0];

        assert!(entry.injected_exif);
        assert_eq!(
            std::fs::read(&source).unwrap(),
            MIN_JPEG,
            "source never rewritten"
        );
        assert_eq!(
            std::fs::metadata(&source).unwrap().nlink(),
            1,
            "copy, not link"
        );
        let info = crate::metadata::exif_read::read_exif(&entry.staged).unwrap();
        assert!(info.date.is_some());
        let mode = std::fs::metadata(&entry.staged)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o666);
    }

    #[test]
    fn exif_failure_is_not_fatal() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        // Not a JPEG: little_exif refuses, yet the file must still be prepared.
        let item = f.resolved("a.jpg", b"definitely not a jpeg", "a.jpg", true);
        let report = s.prepare(vec![item]);
        assert_eq!(report.prepared.len(), 1);
        assert!(!report.prepared[0].injected_exif);
    }

    #[test]
    fn a_missing_source_fails_alone() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let good = f.resolved("a.jpg", b"ok", "a.jpg", false);
        let bad = f.resolved("b.jpg", b"ok", "b.jpg", false);
        std::fs::remove_file(&bad.plan.source).unwrap();

        let report = s.prepare(vec![good, bad]);
        assert_eq!(report.prepared.len(), 1);
        assert_eq!(report.failed.len(), 1);
    }

    #[test]
    fn staged_names_are_unique_per_destination() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let a = s.staged_path(&f.output.path().join("x/a.jpg"));
        let b = s.staged_path(&f.output.path().join("y/a.jpg"));
        assert_ne!(a, b);
        assert_eq!(a.extension().unwrap(), "jpg");
    }

    #[test]
    fn leftover_staged_file_does_not_block_preparation() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let item = f.resolved("a.jpg", b"new", "a.jpg", false);
        std::fs::write(s.staged_path(&item.final_dest), b"stale").unwrap();

        let report = s.prepare(vec![item]);
        assert_eq!(std::fs::read(&report.prepared[0].staged).unwrap(), b"new");
    }

    // -- journal & commit ----------------------------------------------------

    fn prepared(f: &Fixture, s: &Staging, name: &str, content: &[u8], rel: &str) -> Entry {
        let item = f.resolved(name, content, rel, false);
        s.prepare(vec![item]).prepared.remove(0)
    }

    #[test]
    fn journal_roundtrips_and_clears() {
        let f = Fixture::new();
        let s = f.staging(Some(0o666), Some(0o777));
        let e = prepared(&f, &s, "a.jpg", b"x", "2025/a.jpg");

        s.write_journal(std::slice::from_ref(&e)).unwrap();
        assert_eq!(s.read_journal(), vec![e]);
        assert_eq!(s.pending(), 1);

        s.write_journal(&[]).unwrap();
        assert_eq!(s.pending(), 0);
        assert!(!s.dir().join(JOURNAL_FILE).exists());
    }

    #[test]
    fn malformed_journal_line_is_skipped_not_fatal() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let e = prepared(&f, &s, "a.jpg", b"x", "a.jpg");
        s.write_journal(std::slice::from_ref(&e)).unwrap();
        let mut text = std::fs::read_to_string(s.dir().join(JOURNAL_FILE)).unwrap();
        text.push_str("{broken\n");
        std::fs::write(s.dir().join(JOURNAL_FILE), text).unwrap();
        assert_eq!(s.read_journal(), vec![e]);
    }

    #[test]
    fn commit_publishes_applies_modes_and_removes_the_source() {
        let f = Fixture::new();
        let s = f.staging(Some(0o666), Some(0o777));
        let item = f.resolved("a.jpg", b"photo", "2025/06/a.jpg", false);
        let source = item.plan.source.clone();
        let entries = s.prepare(vec![item]).prepared;
        s.write_journal(&entries).unwrap();

        let report = s.commit(entries).unwrap();

        let dest = f.output.path().join("2025/06/a.jpg");
        assert_eq!(report.committed.len(), 1);
        assert_eq!(report.committed[0].final_dest, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), b"photo");
        assert!(!source.exists(), "source removed after publish");
        assert_eq!(std::fs::metadata(&dest).unwrap().nlink(), 1);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&dest), 0o666);
        assert_eq!(mode(&f.output.path().join("2025/06")), 0o777);
        assert_eq!(mode(&f.output.path().join("2025")), 0o777);
        assert_eq!(s.pending(), 0, "journal cleared after a clean commit");
        assert_eq!(
            std::fs::read_dir(s.dir()).unwrap().count(),
            0,
            "staging emptied"
        );
    }

    #[test]
    fn commit_continues_past_a_failure_and_keeps_it_journaled() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let good = prepared(&f, &s, "a.jpg", b"good", "a.jpg");
        let mut bad = prepared(&f, &s, "b.jpg", b"bad", "b.jpg");
        std::fs::remove_file(&bad.staged).unwrap(); // rename will fail
        bad.final_dest = f.output.path().join("sub/b.jpg");

        let report = s.commit(vec![bad.clone(), good]).unwrap();

        assert_eq!(
            report.committed.len(),
            1,
            "the good file was still published"
        );
        assert_eq!(report.failed.len(), 1);
        assert!(
            f.input.path().join("b.jpg").exists(),
            "failed source untouched"
        );
        assert!(
            !f.output.path().join("sub/b.jpg").exists(),
            "no placeholder left behind"
        );
        assert_eq!(
            s.read_journal(),
            vec![bad],
            "only the failure remains journaled"
        );
    }

    #[test]
    fn an_outside_writer_taking_the_name_gets_a_suffix() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let e = prepared(&f, &s, "a.jpg", b"mine", "x/a.jpg");
        std::fs::create_dir_all(f.output.path().join("x")).unwrap();
        std::fs::write(f.output.path().join("x/a.jpg"), b"theirs").unwrap();

        let report = s.commit(vec![e]).unwrap();

        assert_eq!(
            std::fs::read(f.output.path().join("x/a.jpg")).unwrap(),
            b"theirs"
        );
        assert_eq!(
            std::fs::read(f.output.path().join("x/a_1.jpg")).unwrap(),
            b"mine"
        );
        assert_eq!(
            report.committed[0].final_dest,
            f.output.path().join("x/a_1.jpg")
        );
    }

    #[test]
    fn overwrite_replaces_the_existing_file() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let mut item = f.resolved("a.jpg", b"new", "a.jpg", false);
        item.overwrite = true;
        std::fs::write(&item.final_dest, b"old").unwrap();
        let entries = s.prepare(vec![item]).prepared;

        s.commit(entries).unwrap();
        assert_eq!(
            std::fs::read(f.output.path().join("a.jpg")).unwrap(),
            b"new"
        );
    }

    #[test]
    fn replay_finishes_an_interrupted_batch() {
        let f = Fixture::new();
        let s = f.staging(Some(0o666), Some(0o777));
        let a = f.resolved("a.jpg", b"aaa", "2025/a.jpg", false);
        let b = f.resolved("b.jpg", b"bbb", "2025/b.jpg", false);
        let sources = [a.plan.source.clone(), b.plan.source.clone()];
        let entries = s.prepare(vec![a, b]).prepared;
        s.write_journal(&entries).unwrap();
        // Crash here: phase 4 and the journal are on disk, phase 5 never ran.
        drop(s);

        let s = f.staging(Some(0o666), Some(0o777));
        let report = s.replay().unwrap();

        assert_eq!(report.committed.len(), 2);
        assert_eq!(
            std::fs::read(f.output.path().join("2025/a.jpg")).unwrap(),
            b"aaa"
        );
        assert_eq!(
            std::fs::read(f.output.path().join("2025/b.jpg")).unwrap(),
            b"bbb"
        );
        assert!(sources.iter().all(|p| !p.exists()));
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn replay_drops_entries_whose_staged_file_is_gone() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let e = prepared(&f, &s, "a.jpg", b"x", "a.jpg");
        s.write_journal(std::slice::from_ref(&e)).unwrap();
        std::fs::remove_file(&e.staged).unwrap(); // already published and cleaned

        let report = s.replay().unwrap();
        assert!(report.committed.is_empty() && report.failed.is_empty());
        assert!(!f.output.path().join("a.jpg").exists());
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn replay_purges_orphans_from_a_crash_during_preparation() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        // Phase 4 was interrupted before any journal line existed.
        let e = prepared(&f, &s, "a.jpg", b"x", "a.jpg");

        s.replay().unwrap();
        assert!(!e.staged.exists(), "orphan purged");
        assert!(
            f.input.path().join("a.jpg").exists(),
            "source never touched"
        );
        assert!(!f.output.path().join("a.jpg").exists());
    }

    #[test]
    fn replay_with_nothing_pending_is_a_no_op() {
        let f = Fixture::new();
        let report = f.staging(None, None).replay().unwrap();
        assert!(report.committed.is_empty() && report.failed.is_empty());
    }

    // -- two-step publication ------------------------------------------------

    #[test]
    fn temp_names_are_hidden_and_not_media() {
        let t = temp_path(Path::new("/lib/2025/06/IMG_1.jpg"));
        assert_eq!(t, PathBuf::from("/lib/2025/06/.IMG_1.jpg.mo-part"));
        assert_eq!(t.extension().unwrap(), "mo-part");
    }

    #[test]
    fn a_commit_leaves_no_temporary_file_in_the_library() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let e = prepared(&f, &s, "a.jpg", b"x", "2025/a.jpg");
        s.commit(vec![e]).unwrap();
        let hidden: Vec<_> = tree(f.output.path())
            .into_iter()
            .filter(|p| p.to_string_lossy().ends_with(".mo-part"))
            .collect();
        assert!(hidden.is_empty(), "{hidden:?}");
    }

    #[test]
    fn nothing_is_visible_under_its_final_name_before_the_publish_step() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let e = prepared(&f, &s, "a.jpg", b"payload", "2025/a.jpg");

        s.place_temp(&e).unwrap(); // step 5a only

        assert!(!e.final_dest.exists(), "not published yet");
        assert_eq!(
            std::fs::read(temp_path(&e.final_dest)).unwrap(),
            b"payload",
            "complete under its temporary name"
        );
        assert!(
            f.input.path().join("a.jpg").exists(),
            "source untouched until the end"
        );
    }

    #[test]
    fn a_crash_between_place_and_publish_is_finished_by_replay() {
        let f = Fixture::new();
        let s = f.staging(Some(0o666), Some(0o777));
        let e = prepared(&f, &s, "a.jpg", b"payload", "2025/a.jpg");
        s.write_journal(std::slice::from_ref(&e)).unwrap();
        s.place_temp(&e).unwrap(); // ...then the process dies
        assert!(!e.staged.exists() || temp_path(&e.final_dest).exists());
        drop(s);

        let s = f.staging(Some(0o666), Some(0o777));
        let report = s.replay().unwrap();

        assert_eq!(report.committed.len(), 1);
        assert_eq!(std::fs::read(&e.final_dest).unwrap(), b"payload");
        assert!(!temp_path(&e.final_dest).exists());
        assert!(!f.input.path().join("a.jpg").exists());
        let mode = std::fs::metadata(&e.final_dest)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o666);
    }

    #[test]
    fn a_failed_placement_cleans_its_partial_temporary_file() {
        let f = Fixture::new();
        let s = f.staging(None, None);
        let mut e = prepared(&f, &s, "a.jpg", b"x", "2025/a.jpg");
        e.final_dest = f.output.path().join("2025/a.jpg");
        std::fs::remove_file(&e.staged).unwrap();
        let report = s.commit(vec![e.clone()]).unwrap();
        assert_eq!(report.failed.len(), 1);
        assert!(!temp_path(&e.final_dest).exists());
    }

    #[test]
    fn a_work_area_on_another_filesystem_is_published_by_copy() {
        // /dev/shm is a different filesystem from a regular temp dir, so the
        // staged file cannot be renamed next to its destination: it is copied.
        let f = Fixture::new();
        let shm = Path::new("/dev/shm");
        let different = std::fs::metadata(shm).map(|m| m.dev()).ok()
            != std::fs::metadata(f.output.path()).map(|m| m.dev()).ok();
        if !different || !shm.is_dir() {
            return; // single-filesystem sandbox: the scenario cannot occur here
        }
        let work = shm.join(format!("mo-test-{}", std::process::id()));
        let s = Staging::new(
            work.clone(),
            f.output.path().to_path_buf(),
            Some(Mode(0o666)),
            None,
        )
        .unwrap();
        let item = f.resolved("a.jpg", b"across mounts", "2025/a.jpg", false);
        let entries = s.prepare(vec![item]).prepared;
        s.write_journal(&entries).unwrap();

        let report = s.commit(entries).unwrap();

        assert_eq!(report.committed.len(), 1, "{:?}", report.failed);
        assert_eq!(
            std::fs::read(f.output.path().join("2025/a.jpg")).unwrap(),
            b"across mounts"
        );
        assert_eq!(
            std::fs::read_dir(&work).unwrap().count(),
            0,
            "work area emptied"
        );
        assert!(!f.input.path().join("a.jpg").exists());
        let _ = std::fs::remove_dir_all(work);
    }
}
