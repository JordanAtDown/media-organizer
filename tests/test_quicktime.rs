//! QuickTime dates, including the real-device fixture whose Apple-specific
//! boxes (`tapt`, `clef`, `prof`, `enof`) defeat full-tree parsers.

mod common;

use chrono::{TimeZone, Utc};
use common::*;
use media_organizer::metadata::isobmff;

#[test]
fn real_iphone_fixture_yields_its_creation_time() {
    let dt = isobmff::read_quicktime_date(&iphone_fixture()).unwrap();
    assert_eq!(
        dt.with_timezone(&Utc),
        Utc.with_ymd_and_hms(2026, 2, 16, 12, 37, 5).unwrap()
    );
}

#[test]
fn real_iphone_fixture_is_organized_by_its_creation_date() {
    let env = Env::new();
    let bytes = std::fs::read(iphone_fixture()).unwrap();
    env.put("IMG_0001.mov", &bytes);
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 1);
    assert_eq!(sum.by_date_source["quicktime"], 1);
    let expected = Utc
        .with_ymd_and_hms(2026, 2, 16, 12, 37, 5)
        .unwrap()
        .with_timezone(&chrono::Local);
    let rel = format!("{}/IMG_0001.mov", expected.format("%Y/%m"));
    assert_eq!(files_under(env.output.path()), vec![rel]);
}

#[test]
fn zero_creation_time_is_not_a_date_even_with_a_valid_box_tree() {
    // The DSM thumbnail guard: SYNOPHOTO_FILM_M.mp4 carries creation_time == 0.
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("SYNOPHOTO_FILM_M.mp4");
    // mp4_with_creation_time adds the Mac offset, so patch the field to a true zero.
    let mut bytes = mp4_with_creation_time(0);
    let at = bytes.windows(4).position(|w| w == b"mvhd").unwrap() + 4 + 4;
    bytes[at..at + 8].fill(0);
    std::fs::write(&f, bytes).unwrap();

    assert!(isobmff::read_quicktime_date(&f).is_err());
}
