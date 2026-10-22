//! End-to-end: files with no metadata are placed by the date in their name.

mod common;

use common::*;
use rstest::rstest;

#[rstest]
#[case("IMG-20250626-WA0002.jpg", "2025/06")]
#[case("VID-20250408-WA0022.mp4", "2025/04")]
#[case("IMG_20190807_080939.jpg", "2019/08")]
#[case("20220630_211409.jpg", "2022/06")]
#[case("Resized_20210514_173457.jpg", "2021/05")]
fn a_dated_name_decides_the_directory(#[case] name: &str, #[case] dir: &str) {
    let env = Env::new();
    env.put(name, b"no metadata here");
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.organized, 1);
    assert_eq!(sum.by_date_source["filename"], 1);
    assert_eq!(
        files_under(env.output.path()),
        vec![format!("{dir}/{name}")]
    );
}

#[test]
fn facebook_timestamps_are_unix_milliseconds() {
    let env = Env::new();
    env.put("FB_IMG_1484685560948.jpg", b"x");
    let mut scanner = env.scanner("", "");

    env.scan(&mut scanner);

    // 2017-01-17 20:39:20 UTC — the month is January in every timezone.
    let files = files_under(env.output.path());
    assert!(files[0].starts_with("2017/01/"), "{files:?}");
}

#[test]
fn a_name_with_no_date_falls_through_to_the_mtime() {
    let env = Env::new();
    let f = env.put("Resized_Screenshot_20210317-095743_Instagram.jpg", b"x");
    set_mtime(&f, 1_700_000_000); // 2023-11-14 UTC
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.by_date_source["mtime"], 1);
    assert!(files_under(env.output.path())[0].starts_with("2023/11/"));
}

#[test]
fn whatsapp_photos_get_their_date_written_into_the_exif() {
    let env = Env::new();
    env.put("IMG-20250626-WA0002.jpg", &jpeg_without_exif());
    let mut scanner = env.scanner("", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.exif_injected, 1);
    let info = media_organizer::metadata::exif_read::read_exif(
        &env.out("2025/06/IMG-20250626-WA0002.jpg"),
    )
    .unwrap();
    assert_eq!(info.date, Some(local(2025, 6, 26, 12, 0, 0)));
}
