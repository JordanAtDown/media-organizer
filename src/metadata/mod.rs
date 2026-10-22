//! Capture-metadata readers and writers.

pub mod exif_read;
pub mod exif_write;
pub mod filename;
pub mod isobmff;

/// Known video extensions, used to pick a metadata reader and `video_prefix`.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "avi", "mkv", "3gp", "m4v", "wmv", "flv", "webm", "ts", "mts", "m2ts",
];

/// True if the lowercase extension (without dot) is a known video format.
pub fn is_video(ext_lower: &str) -> bool {
    VIDEO_EXTENSIONS.contains(&ext_lower)
}
