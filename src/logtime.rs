//! Log timestamps in the process's local zone (`TZ`), written for people.
//!
//! `tracing-subscriber` stamps events in UTC by default, which disagrees with the
//! zone the capture dates (and so the destination folders) are computed in. Using
//! `Local` for both keeps logs and library directories on the same clock.

use chrono::{DateTime, Local, TimeZone};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// `2026-10-22 20:25:42`: local wall clock, to the second. The offset is logged
/// once at startup (`timezone_offset`) rather than on every line.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalTime;

fn format<Tz: TimeZone>(t: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    t.format("%Y-%m-%d %H:%M:%S").to_string()
}

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        w.write_str(&format(Local::now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};

    #[test]
    fn renders_local_wall_clock_to_the_second() {
        let paris = FixedOffset::east_opt(2 * 3600).unwrap();
        let t = Utc
            .with_ymd_and_hms(2026, 10, 22, 18, 25, 42)
            .unwrap()
            .with_timezone(&paris);
        assert_eq!(format(t), "2026-10-22 20:25:42");
    }

    #[test]
    fn local_now_has_the_expected_shape() {
        let s = format(Local::now());
        assert!(
            chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S").is_ok(),
            "{s}"
        );
    }
}
