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
use crate::resolve::Resolved;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

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
}
