//! PHASE 4 — preparation, then PHASE 5 — commit.
//!
//! Phase 4 materializes every resolved file in the staging area: a hard link
//! when possible (free on one NFS export), a copy when the EXIF must be
//! rewritten. **Nothing outside the staging area is touched.** Phase 5, in the
//! same module, publishes the prepared files with `rename()`.
//!
//! The staging area must share `output`'s filesystem, so a commit is a
//! metadata-only operation and the window in which a library scanner can see a
//! half-organized tree shrinks to one quick burst.

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
use std::os::unix::fs::MetadataExt;
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

pub struct Staging {
    dir: PathBuf,
    output: PathBuf,
    file_mode: Option<Mode>,
    dir_mode: Option<Mode>,
}

impl Staging {
    /// Create the staging directory and verify it shares `output`'s filesystem.
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

        let staging_dev = std::fs::metadata(&dir).map_err(io(&dir))?.dev();
        let output_dev = std::fs::metadata(&output).map_err(io(&output))?.dev();
        if staging_dev != output_dev {
            return Err(StagingError::CrossDevice {
                staging: dir,
                output,
            });
        }
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

    /// Publish the entries with `rename()`. Sequential on purpose: it is one
    /// quick burst of metadata operations, and ordering stays deterministic.
    ///
    /// A failure is reported and the batch **continues** — there is no global
    /// rollback, so one bad file can never stall the others. Failed entries are
    /// left staged and the journal is rewritten to hold exactly those.
    pub fn commit(&self, entries: Vec<Entry>) -> Result<CommitReport, StagingError> {
        let mut report = CommitReport::default();
        for entry in entries {
            match self.commit_one(&entry) {
                Ok(done) => report.committed.push(done),
                Err(message) => report.failed.push((entry, message)),
            }
        }
        let remaining: Vec<Entry> = report.failed.iter().map(|(e, _)| e.clone()).collect();
        self.write_journal(&remaining)?;
        Ok(report)
    }

    fn commit_one(&self, entry: &Entry) -> Result<Committed, String> {
        let dest = &entry.final_dest;
        let parent = dest.parent().unwrap_or(&self.output);
        fsops::ensure_dir_chain(&self.output, parent, entry.dir_mode.map(Mode))
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;

        // Reserve the name against *outside* writers (Nextcloud dropping a file
        // mid-scan). Inside the batch names are already unique by construction.
        let (target, placeholder) = if entry.overwrite {
            (dest.clone(), false)
        } else {
            (self.claim_name(dest)?, true)
        };

        if let Err(e) = std::fs::rename(&entry.staged, &target) {
            if placeholder {
                let _ = std::fs::remove_file(&target);
            }
            return Err(format!("cannot publish {}: {e}", target.display()));
        }

        if let Err(e) = fsops::apply_mode(&target, entry.file_mode.map(Mode)) {
            warn!(path = %target.display(), error = %e, "published but chmod failed");
        }
        // The source is removed only now, after the destination is in place.
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

    /// Atomically create an empty file at the first free name (`create_new`).
    /// The following `rename` replaces that placeholder.
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
    /// Each journaled entry whose staged file still exists is committed; an
    /// entry whose staged file is gone was already published and is dropped.
    /// Staged files that no journal line refers to (a crash during phase 4)
    /// are purged — their sources were never touched.
    pub fn replay(&self) -> Result<CommitReport, StagingError> {
        let (live, gone): (Vec<_>, Vec<_>) = self
            .read_journal()
            .into_iter()
            .partition(|e| e.staged.exists());
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
    use std::os::unix::fs::PermissionsExt;

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
    fn staging_on_another_filesystem_is_refused() {
        let output = tempfile::tempdir().unwrap();
        let err = Staging::new(
            PathBuf::from("/dev/shm/mo-test-staging"),
            output.path().to_path_buf(),
            None,
            None,
        );
        // /dev/shm is tmpfs on Linux; if the temp dir shares it the check cannot fail.
        if std::fs::metadata("/dev/shm").map(|m| m.dev()).ok()
            != std::fs::metadata(output.path()).map(|m| m.dev()).ok()
        {
            assert!(matches!(err, Err(StagingError::CrossDevice { .. })));
        }
        let _ = std::fs::remove_dir("/dev/shm/mo-test-staging");
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
}
