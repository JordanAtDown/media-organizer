//! Capture-date resolution.
//!
//! The chain, in this exact order, ends at the first source that yields a date:
//!   1. EXIF `DateTimeOriginal` (photos)
//!   2. QuickTime `mvhd.creation_time` (videos)
//!   3. date encoded in the file name
//!   4. the file's modification time — a last resort and only an approximation

use crate::error::ExifError;
use crate::metadata::{exif_read, filename, isobmff};
use chrono::{DateTime, Local};
use serde::Serialize;
use std::path::Path;

/// Where a capture date came from, most to least trustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DateSource {
    Exif,
    QuickTime,
    Filename,
    /// Approximation: the file's mtime. Logged at `warn`, counted apart.
    Mtime,
}

/// A resolved capture date, with its provenance and camera label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureInfo {
    pub date: DateTime<Local>,
    pub source: DateSource,
    /// Sanitized camera model, photos only; `None` when unknown.
    pub camera: Option<String>,
}

/// A fallback source of capture dates, tried after the primary metadata reader.
///
/// Adding a source is one implementor plus one entry in [`fallback_readers`].
pub trait DateReader: Send + Sync {
    fn source(&self) -> DateSource;
    fn read_date(&self, path: &Path) -> Option<DateTime<Local>>;
}

struct FilenameDateReader;

impl DateReader for FilenameDateReader {
    fn source(&self) -> DateSource {
        DateSource::Filename
    }
    fn read_date(&self, path: &Path) -> Option<DateTime<Local>> {
        filename::parse_date_from_filename(path)
    }
}

struct MtimeDateReader;

impl DateReader for MtimeDateReader {
    fn source(&self) -> DateSource {
        DateSource::Mtime
    }
    fn read_date(&self, path: &Path) -> Option<DateTime<Local>> {
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(DateTime::<Local>::from(modified))
    }
}

/// Fallback readers in priority order. The file name always precedes the mtime:
/// a date encoded in the name is far more reliable than the mtime of a file that
/// may have been copied, synced or restored since.
fn fallback_readers(use_mtime: bool) -> Vec<Box<dyn DateReader>> {
    let mut readers: Vec<Box<dyn DateReader>> = vec![Box::new(FilenameDateReader)];
    if use_mtime {
        readers.push(Box::new(MtimeDateReader));
    }
    readers
}

/// Resolve the capture date of `path`.
///
/// `is_video` selects the primary metadata reader. EXIF is read once, so the
/// camera label costs no extra I/O. `use_mtime` enables the last-resort mtime
/// source; without it a file with no other date yields `ExifError::NoDate`.
pub fn read_capture(
    path: &Path,
    is_video: bool,
    use_mtime: bool,
) -> Result<CaptureInfo, ExifError> {
    let mut camera = None;

    if is_video {
        if let Ok(date) = isobmff::read_quicktime_date(path) {
            return Ok(CaptureInfo {
                date,
                source: DateSource::QuickTime,
                camera: None,
            });
        }
    } else {
        let info = exif_read::read_exif(path)?;
        if let Some(date) = info.date {
            return Ok(CaptureInfo {
                date,
                source: DateSource::Exif,
                camera: info.camera,
            });
        }
        camera = info.camera;
    }

    for reader in fallback_readers(use_mtime) {
        if let Some(date) = reader.read_date(path) {
            return Ok(CaptureInfo {
                date,
                source: reader.source(),
                camera,
            });
        }
    }
    Err(ExifError::NoDate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_is_used_when_there_is_no_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("IMG-20250626-WA0002.jpg");
        std::fs::write(&file, b"not a jpeg").unwrap();

        let info = read_capture(&file, false, false).unwrap();
        assert_eq!(info.source, DateSource::Filename);
        assert_eq!(info.date.format("%Y-%m-%d").to_string(), "2025-06-26");
    }

    #[test]
    fn video_falls_back_to_filename() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("VID-20250408-WA0022.mp4");
        std::fs::write(&file, b"mp4 stub").unwrap();

        let info = read_capture(&file, true, false).unwrap();
        assert_eq!(info.source, DateSource::Filename);
        assert_eq!(info.camera, None);
    }

    #[test]
    fn no_source_means_no_date() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("random.jpg");
        std::fs::write(&file, b"x").unwrap();

        assert!(matches!(
            read_capture(&file, false, false),
            Err(ExifError::NoDate)
        ));
    }

    #[test]
    fn unreadable_photo_is_an_io_error() {
        let err = read_capture(Path::new("/nonexistent/a.jpg"), false, true).unwrap_err();
        assert!(matches!(err, ExifError::Io(_)));
    }

    fn set_mtime(path: &Path, unix_secs: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn mtime_is_the_last_resort() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sans_date.jpg");
        std::fs::write(&file, b"x").unwrap();
        set_mtime(&file, 1_700_000_000);

        let info = read_capture(&file, false, true).unwrap();
        assert_eq!(info.source, DateSource::Mtime);
        assert_eq!(info.date.timestamp(), 1_700_000_000);
    }

    #[test]
    fn filename_beats_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("IMG-20250626-WA0002.jpg");
        std::fs::write(&file, b"x").unwrap();
        set_mtime(&file, 1_700_000_000);

        let info = read_capture(&file, false, true).unwrap();
        assert_eq!(info.source, DateSource::Filename);
    }

    #[test]
    fn mtime_fallback_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sans_date.mp4");
        std::fs::write(&file, b"x").unwrap();

        assert!(matches!(
            read_capture(&file, true, false),
            Err(ExifError::NoDate)
        ));
    }
}
