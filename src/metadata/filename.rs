use chrono::{DateTime, Local, TimeZone};
use std::path::Path;

/// Extract a capture date from the file's name, for files with no metadata.
///
/// Patterns, tried in order:
///   1. WhatsApp — `IMG-YYYYMMDD-WAxxxx`, `VID-…`, `AUD-…` (time forced to noon)
///   2. Android camera — `[IMG_]YYYYMMDD_HHMMSS[…]`
///   3. Facebook — `FB_IMG_<unix_milliseconds>[…]`
///   4. `Resized_` prefix followed by pattern 2
///
/// Returns `None` when nothing matches or the components are not a valid date.
pub fn parse_date_from_filename(path: &Path) -> Option<DateTime<Local>> {
    let stem = path.file_stem()?.to_str()?;
    parse_whatsapp_date(stem)
        .or_else(|| parse_android_date(stem))
        .or_else(|| parse_facebook_date(stem))
        .or_else(|| stem.strip_prefix("Resized_").and_then(parse_android_date))
}

/// WhatsApp only encodes the day, so the time is set to noon local.
fn parse_whatsapp_date(stem: &str) -> Option<DateTime<Local>> {
    let after = stem
        .strip_prefix("IMG-")
        .or_else(|| stem.strip_prefix("VID-"))
        .or_else(|| stem.strip_prefix("AUD-"))?;
    // `get` rather than slicing: a non-ASCII stem must not panic.
    let date = after.get(..8)?;
    if !after.get(8..)?.starts_with("-WA") {
        return None;
    }
    parse_ymd_hms(date, "120000")
}

fn parse_android_date(stem: &str) -> Option<DateTime<Local>> {
    let s = stem.strip_prefix("IMG_").unwrap_or(stem);
    let date = s.get(..8)?;
    if s.as_bytes().get(8) != Some(&b'_') {
        return None;
    }
    parse_ymd_hms(date, s.get(9..15)?)
}

fn parse_facebook_date(stem: &str) -> Option<DateTime<Local>> {
    let digits = stem
        .strip_prefix("FB_IMG_")?
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|s| !s.is_empty())?;
    let ms: i64 = digits.parse().ok()?;
    DateTime::from_timestamp(ms / 1000, 0).map(|dt| dt.with_timezone(&Local))
}

fn parse_ymd_hms(date: &str, time: &str) -> Option<DateTime<Local>> {
    let digits = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(date, 8) || !digits(time, 6) {
        return None;
    }
    let n = |s: &str| s.parse::<u32>().ok();
    Local
        .with_ymd_and_hms(
            date[0..4].parse().ok()?,
            n(&date[4..6])?,
            n(&date[6..8])?,
            n(&time[0..2])?,
            n(&time[2..4])?,
            n(&time[4..6])?,
        )
        .single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn fmt(name: &str) -> Option<String> {
        parse_date_from_filename(Path::new(name)).map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
    }

    #[rstest]
    #[case("IMG-20250626-WA0002.jpg", "2025-06-26 12:00:00")]
    #[case("VID-20250408-WA0022.mp4", "2025-04-08 12:00:00")]
    #[case("AUD-20231115-WA0001.opus", "2023-11-15 12:00:00")]
    // Synology Drive conflict copies append extra characters.
    #[case("IMG-20250626-WA0003 (1).jpg", "2025-06-26 12:00:00")]
    #[case("IMG_20190807_080939.jpg", "2019-08-07 08:09:39")]
    // Some Android files append a 7th digit after HHMMSS.
    #[case("IMG_20190807_0809391.jpg", "2019-08-07 08:09:39")]
    #[case("20220630_211409.jpg", "2022-06-30 21:14:09")]
    #[case("20220630_211409_031.jpg", "2022-06-30 21:14:09")]
    #[case("Resized_20210514_173457.jpg", "2021-05-14 17:34:57")]
    #[case("Resized_20180314_121942_119570534785744.jpg", "2018-03-14 12:19:42")]
    fn recognizes_pattern(#[case] name: &str, #[case] expected: &str) {
        assert_eq!(fmt(name).as_deref(), Some(expected));
    }

    #[test]
    fn facebook_timestamp_is_unix_milliseconds() {
        // 1484685560948 ms → 2017-01-17 20:39:20 UTC
        let dt = parse_date_from_filename(Path::new("FB_IMG_1484685560948.jpg")).unwrap();
        let utc = dt.with_timezone(&chrono::Utc);
        assert_eq!(
            utc.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2017-01-17 20:39:20"
        );
    }

    #[rstest]
    #[case("IMG-20251340-WA0001.jpg")] // month 13, day 40
    #[case("Resized_Screenshot_20210317-095743_Instagram.jpg")]
    #[case("random_file.jpg")]
    #[case("4.jpg")]
    #[case("no_exif.jpg")]
    #[case("clip.mp4")]
    #[case("")]
    fn rejects_non_matching_names(#[case] name: &str) {
        assert_eq!(fmt(name), None);
    }

    #[rstest]
    #[case("IMG-éééééééé-WA0001.jpg")]
    #[case("IMG_é0190807_080939.jpg")]
    #[case("日本語日本語日本語日本語日本語.jpg")]
    fn non_ascii_names_never_panic(#[case] name: &str) {
        assert_eq!(fmt(name), None);
    }
}
