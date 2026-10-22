use crate::error::ExifError;
use chrono::{DateTime, Local};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Write `DateTimeOriginal` into a JPEG/PNG/HEIC file, atomically.
///
/// 1. a temp file is created in the **same directory** (same filesystem);
/// 2. the original is copied into it;
/// 3. the EXIF tag is written to the copy;
/// 4. `rename(temp → path)` replaces the original.
///
/// If any step fails before the rename the temp file is removed and the
/// original is untouched. Callers pass the **staged copy**, never the source.
///
/// `mode` is applied to the result; `None` keeps the original's permission bits
/// (a temp file is created at 0600, which would otherwise leak through the rename).
pub fn write_exif_date(
    path: &Path,
    date: &DateTime<Local>,
    mode: Option<u32>,
) -> Result<(), ExifError> {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let date_str = date.format("%Y:%m:%d %H:%M:%S").to_string();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
    let parse = |e: &dyn std::fmt::Display| ExifError::Parse(e.to_string());

    // NamedTempFile deletes itself on drop until `keep()` is called.
    let tmp = tempfile::Builder::new()
        .prefix(TMP_PREFIX)
        .suffix(&format!(".{ext}"))
        .tempfile_in(parent)
        .map_err(|e| parse(&e))?;

    std::fs::copy(path, tmp.path()).map_err(|e| parse(&e))?;

    let mut metadata = Metadata::new_from_path(tmp.path()).unwrap_or_else(|_| Metadata::new());
    metadata.set_tag(ExifTag::DateTimeOriginal(date_str));
    metadata.write_to_file(tmp.path()).map_err(|e| parse(&e))?;

    let (_, tmp_path) = tmp.keep().map_err(|e| parse(&e))?;
    let cleanup = |e: std::io::Error| {
        let _ = std::fs::remove_file(&tmp_path);
        parse(&e)
    };

    let permissions = match mode {
        Some(m) => std::fs::Permissions::from_mode(m),
        None => std::fs::metadata(path).map_err(cleanup)?.permissions(),
    };
    std::fs::set_permissions(&tmp_path, permissions).map_err(cleanup)?;
    std::fs::rename(&tmp_path, path).map_err(cleanup)
}

/// Prefix of the temporary files this module creates; the scanner skips it.
pub const TMP_PREFIX: &str = ".mo_exif_tmp_";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::exif_read::read_exif;
    use chrono::{TimeZone, Timelike};

    // Smallest JPEG that little_exif accepts: SOI + EOI.
    const MIN_JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xD9];

    fn date() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2025, 6, 26, 12, 0, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn injected_date_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.jpg");
        std::fs::write(&file, MIN_JPEG).unwrap();

        write_exif_date(&file, &date(), None).unwrap();

        let info = read_exif(&file).unwrap();
        assert_eq!(info.date.unwrap().hour(), 12);
        assert_eq!(info.date, Some(date()));
    }

    #[test]
    fn explicit_mode_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.jpg");
        std::fs::write(&file, MIN_JPEG).unwrap();

        write_exif_date(&file, &date(), Some(0o666)).unwrap();

        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o666);
    }

    #[test]
    fn no_mode_keeps_original_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.jpg");
        std::fs::write(&file, MIN_JPEG).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_exif_date(&file, &date(), None).unwrap();

        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.jpg");
        std::fs::write(&file, MIN_JPEG).unwrap();

        write_exif_date(&file, &date(), None).unwrap();

        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "unexpected leftovers: {names:?}");
    }

    #[test]
    fn missing_file_fails_without_creating_anything() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("missing.jpg");

        assert!(write_exif_date(&file, &date(), None).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
