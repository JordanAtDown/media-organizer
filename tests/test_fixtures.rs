//! Checks that the shared generators produce what the other tests rely on.

mod common;

use common::*;
use media_organizer::metadata::{exif_read, isobmff};

#[test]
fn generated_exif_jpeg_reads_back_with_date_and_camera() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.jpg");
    std::fs::write(
        &f,
        jpeg_with_exif(local(2024, 3, 15, 10, 30, 45), Some("Pixel 7")),
    )
    .unwrap();

    let info = exif_read::read_exif(&f).unwrap();
    assert_eq!(info.date, Some(local(2024, 3, 15, 10, 30, 45)));
    assert_eq!(info.camera.as_deref(), Some("Pixel 7"));
}

#[test]
fn generated_plain_jpeg_has_no_exif() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.jpg");
    std::fs::write(&f, jpeg_without_exif()).unwrap();
    assert!(exif_read::read_exif(&f).unwrap().date.is_none());
}

#[test]
fn generated_mp4_reads_back_its_creation_time() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.mp4");
    std::fs::write(&f, mp4_with_creation_time(1_767_354_117)).unwrap();
    assert_eq!(
        isobmff::read_quicktime_date(&f).unwrap().timestamp(),
        1_767_354_117
    );
}

#[test]
fn env_puts_files_and_lists_them() {
    let env = Env::new();
    env.put("a/b.jpg", b"x");
    assert_eq!(files_under(env.input.path()), vec!["a/b.jpg"]);
}
