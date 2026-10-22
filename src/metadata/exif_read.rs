use crate::error::ExifError;
use chrono::{DateTime, Local, TimeZone};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Longest camera label kept, so a hostile or corrupt tag cannot blow up a path.
const CAMERA_MAX_CHARS: usize = 64;

/// What one EXIF pass yields: the capture date and the camera label.
///
/// Both are optional — a file with no readable EXIF yields `ExifInfo::default()`,
/// which is not an error: the caller falls back to other date sources.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExifInfo {
    /// `DateTimeOriginal` (tag 0x9003).
    pub date: Option<DateTime<Local>>,
    /// Sanitized `Model` (falling back to `Make`), safe to use in a path.
    pub camera: Option<String>,
}

/// Read `DateTimeOriginal` and the camera model in a single pass over the file.
///
/// Only an I/O failure opening the file is an error; unparseable or absent EXIF
/// returns an empty [`ExifInfo`].
pub fn read_exif(path: &Path) -> Result<ExifInfo, ExifError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);

    let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) else {
        return Ok(ExifInfo::default());
    };

    let ascii = |tag: exif::Tag| -> Option<String> {
        let field = exif.get_field(tag, exif::In::PRIMARY)?;
        match field.value {
            exif::Value::Ascii(ref v) => v
                .first()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        }
    };

    let date = ascii(exif::Tag::DateTimeOriginal).and_then(|s| parse_exif_datetime(&s));
    let camera = ascii(exif::Tag::Model)
        .or_else(|| ascii(exif::Tag::Make))
        .and_then(|s| sanitize_camera(&s));

    Ok(ExifInfo { date, camera })
}

/// Make a camera label safe to embed in a destination path: trim, collapse
/// internal whitespace, drop path separators, NUL and control characters,
/// cap the length. Empty after cleaning → `None`.
pub fn sanitize_camera(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !matches!(c, '/' | '\\') && !c.is_control() || c.is_whitespace())
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(CAMERA_MAX_CHARS).collect();
    let truncated = truncated.trim().to_string();
    (!truncated.is_empty()).then_some(truncated)
}

/// Parse an EXIF datetime string `YYYY:MM:DD HH:MM:SS` as local time.
fn parse_exif_datetime(s: &str) -> Option<DateTime<Local>> {
    let s = s.trim().trim_end_matches('\0');
    if s.len() < 19 || !s.is_char_boundary(19) {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b':' || b[7] != b':' || b[10] != b' ' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year: i32 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let min: u32 = s.get(14..16)?.parse().ok()?;
    let sec: u32 = s.get(17..19)?.parse().ok()?;

    Local
        .with_ymd_and_hms(year, month, day, hour, min, sec)
        .single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn parses_valid_datetime() {
        let dt = parse_exif_datetime("2024:03:15 10:30:45").unwrap();
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2024-03-15 10:30:45"
        );
    }

    #[test]
    fn parses_datetime_with_padding() {
        let dt = parse_exif_datetime("  2024:06:01 08:00:00\0 ").unwrap();
        assert_eq!(dt.format("%Y").to_string(), "2024");
    }

    #[rstest]
    #[case("not a date")]
    #[case("2024-03-15 10:30:45")]
    #[case("")]
    #[case("2024:13:45 10:30:45")]
    fn rejects_invalid_datetime(#[case] input: &str) {
        assert!(parse_exif_datetime(input).is_none());
    }

    #[test]
    fn file_without_exif_yields_empty_info() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(read_exif(tmp.path()).unwrap(), ExifInfo::default());
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let err = read_exif(Path::new("/nonexistent/photo.jpg")).unwrap_err();
        assert!(matches!(err, ExifError::Io(_)));
    }

    #[rstest]
    #[case("  Canon  EOS 5D ", Some("Canon EOS 5D"))]
    #[case("iPhone/12\\Pro", Some("iPhone12Pro"))]
    #[case("Pixel\u{0}7\u{7}", Some("Pixel7"))]
    #[case("   ", None)]
    #[case("///", None)]
    fn sanitizes_camera(#[case] raw: &str, #[case] expected: Option<&str>) {
        assert_eq!(sanitize_camera(raw).as_deref(), expected);
    }

    #[test]
    fn camera_is_length_capped() {
        let long = "x".repeat(200);
        assert_eq!(sanitize_camera(&long).unwrap().chars().count(), 64);
    }
}
