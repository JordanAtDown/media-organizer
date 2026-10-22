//! PHASE 2 — planning. Read-only: nothing is written anywhere.
//!
//! Turns an inventoried [`Candidate`] into a [`FilePlan`]: the capture date and
//! its provenance, the destination the pattern asks for, and whether the date
//! should be written back into the file's EXIF.

use crate::config::FolderConfig;
use crate::date_reader::{self, CaptureInfo, DateSource};
use crate::error::ExifError;
use crate::metadata::is_video;
use crate::naming::{self, PatternInput};
use std::path::{Path, PathBuf};

/// Extensions `little_exif` can write `DateTimeOriginal` into.
const EXIF_WRITABLE: &[&str] = &["jpg", "jpeg", "png", "heic"];

/// A file found by the inventory, with the `stat` data it already paid for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: u64,
}

/// What phase 2 decided for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlan {
    pub source: PathBuf,
    pub size: u64,
    pub mtime: u64,
    pub capture: CaptureInfo,
    /// Destination relative to the folder's `output`, as the pattern renders it.
    pub rel_dest: PathBuf,
    /// Write `DateTimeOriginal` into the staged copy (never into the source).
    pub inject_exif: bool,
}

#[derive(Debug)]
pub enum PlanOutcome {
    Planned(FilePlan),
    /// No date from any enabled source.
    NoDate,
    /// The file could not be read.
    Unreadable(String),
}

/// Plan one file. Pure with respect to the filesystem: it only reads.
pub fn plan_file(candidate: &Candidate, folder: &FolderConfig) -> PlanOutcome {
    let ext_lower = extension_lower(&candidate.path);
    let video = is_video(&ext_lower);

    let capture = match date_reader::read_capture(&candidate.path, video, folder.use_mtime_fallback)
    {
        Ok(c) => c,
        Err(ExifError::NoDate) => return PlanOutcome::NoDate,
        Err(e) => return PlanOutcome::Unreadable(e.to_string()),
    };

    let inject_exif = !video
        && EXIF_WRITABLE.contains(&ext_lower.as_str())
        && match capture.source {
            DateSource::Filename => folder.write_exif_from_filename,
            DateSource::Mtime => folder.write_exif_from_mtime,
            DateSource::Exif | DateSource::QuickTime => false,
        };

    let stem = candidate
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = candidate
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let prefix = if video {
        &folder.video_prefix
    } else {
        &folder.photo_prefix
    };

    let rel_dest = PathBuf::from(naming::apply_pattern(
        &folder.pattern,
        &PatternInput {
            date: &capture.date,
            stem,
            ext: &ext,
            camera: capture.camera.as_deref(),
            prefix,
        },
    ));

    PlanOutcome::Planned(FilePlan {
        source: candidate.path.clone(),
        size: candidate.size,
        mtime: candidate.mtime,
        capture,
        rel_dest,
        inject_exif,
    })
}

pub fn extension_lower(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn folder(extra: &str) -> FolderConfig {
        config::parse(&format!(
            "[[folders]]\ninput = \"/in\"\noutput = \"/out\"\n{extra}"
        ))
        .unwrap()
        .folders
        .remove(0)
    }

    fn candidate(dir: &Path, name: &str) -> Candidate {
        let path = dir.join(name);
        std::fs::write(&path, b"not a real image").unwrap();
        Candidate {
            path,
            size: 16,
            mtime: 0,
        }
    }

    fn planned(outcome: PlanOutcome) -> FilePlan {
        match outcome {
            PlanOutcome::Planned(p) => p,
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn destination_follows_the_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let c = candidate(dir.path(), "IMG-20250626-WA0002.jpg");
        let f = folder("pattern = \"{year}/{month}/{stem}{ext}\"");
        let plan = planned(plan_file(&c, &f));
        assert_eq!(
            plan.rel_dest,
            PathBuf::from("2025/06/IMG-20250626-WA0002.jpg")
        );
    }

    #[test]
    fn video_uses_the_video_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let c = candidate(dir.path(), "VID-20250408-WA0022.mp4");
        let f = folder(
            "pattern = \"{prefix}{stem}{ext}\"\nvideo_prefix = \"V_\"\nphoto_prefix = \"P_\"",
        );
        let plan = planned(plan_file(&c, &f));
        assert_eq!(plan.rel_dest, PathBuf::from("V_VID-20250408-WA0022.mp4"));
    }

    #[test]
    fn filename_date_injects_exif_by_default_for_photos() {
        let dir = tempfile::tempdir().unwrap();
        let c = candidate(dir.path(), "IMG-20250626-WA0002.jpg");
        assert!(planned(plan_file(&c, &folder(""))).inject_exif);
    }

    #[test]
    fn filename_date_never_injects_into_a_video() {
        let dir = tempfile::tempdir().unwrap();
        let c = candidate(dir.path(), "VID-20250408-WA0022.mp4");
        assert!(!planned(plan_file(&c, &folder(""))).inject_exif);
    }

    #[test]
    fn filename_injection_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let c = candidate(dir.path(), "IMG-20250626-WA0002.jpg");
        let f = folder("write_exif_from_filename = false");
        assert!(!planned(plan_file(&c, &f)).inject_exif);
    }

    #[test]
    fn mtime_date_does_not_inject_unless_asked() {
        let dir = tempfile::tempdir().unwrap();
        let c = candidate(dir.path(), "sans_date.jpg");
        let plan = planned(plan_file(&c, &folder("")));
        assert_eq!(plan.capture.source, DateSource::Mtime);
        assert!(!plan.inject_exif);

        let plan = planned(plan_file(&c, &folder("write_exif_from_mtime = true")));
        assert!(plan.inject_exif);
    }

    #[test]
    fn without_mtime_fallback_an_undatable_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let c = candidate(dir.path(), "sans_date.jpg");
        let outcome = plan_file(&c, &folder("use_mtime_fallback = false"));
        assert!(matches!(outcome, PlanOutcome::NoDate));
    }

    #[test]
    fn unreadable_file_is_reported_not_panicked() {
        let c = Candidate {
            path: PathBuf::from("/nonexistent/a.jpg"),
            size: 0,
            mtime: 0,
        };
        assert!(matches!(
            plan_file(&c, &folder("")),
            PlanOutcome::Unreadable(_)
        ));
    }

    #[test]
    fn planning_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let c = candidate(dir.path(), "IMG-20250626-WA0002.jpg");
        let before = std::fs::read_dir(dir.path()).unwrap().count();
        planned(plan_file(&c, &folder("")));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), before);
    }
}
