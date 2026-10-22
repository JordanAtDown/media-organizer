//! Shared test utilities. Media files are generated at runtime; the only binary
//! fixture is the metadata-only extract documented in `tests/fixtures/README.md`.
#![allow(dead_code)]

use chrono::{DateTime, Local, TimeZone};
use media_organizer::config::{self, Config};
use media_organizer::scanner::{FolderSummary, NoObserver, Scanner};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, UNIX_EPOCH};

/// Seconds between the Mac epoch (1904) and the Unix epoch.
pub const MAC_EPOCH_OFFSET: u64 = 2_082_844_800;

/// An input folder, an output folder and a state dir, all temporary.
pub struct Env {
    pub input: tempfile::TempDir,
    pub output: tempfile::TempDir,
    pub state: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Self {
        Self {
            input: tempfile::tempdir().unwrap(),
            output: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
        }
    }

    /// `top` is appended before `[[folders]]` (global keys and tables),
    /// `folder` inside the single folder entry.
    pub fn config(&self, top: &str, folder: &str) -> Config {
        // A default pattern, unless the test brings its own.
        let pattern = if folder.contains("pattern") {
            ""
        } else {
            "pattern = \"{year}/{month}/{stem}{ext}\"\n"
        };
        config::parse(&format!(
            "stability_secs = 0\n{top}\n[[folders]]\ninput = \"{}\"\noutput = \"{}\"\n{pattern}{folder}\n",
            self.input.path().display(),
            self.output.path().display(),
        ))
        .unwrap()
    }

    pub fn scanner(&self, top: &str, folder: &str) -> Scanner {
        self.scanner_with(self.config(top, folder), false)
    }

    pub fn scanner_with(&self, cfg: Config, dry_run: bool) -> Scanner {
        Scanner::new(cfg, self.state.path().to_path_buf(), dry_run).unwrap()
    }

    pub fn scan(&self, scanner: &mut Scanner) -> FolderSummary {
        scanner
            .scan_once(&NoObserver, &AtomicBool::new(false))
            .folders
            .remove(0)
    }

    /// Write raw bytes under `input` (creating directories).
    pub fn put(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = self.input.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    pub fn out(&self, rel: &str) -> PathBuf {
        self.output.path().join(rel)
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

/// All regular files under `dir`, relative and sorted, skipping the work dir.
pub fn files_under(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            e.path()
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|p| !p.starts_with(".media-organizer"))
        .collect();
    v.sort();
    v
}

pub fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

pub fn set_mtime(path: &Path, unix_secs: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(unix_secs))
        .unwrap();
}

pub fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Local> {
    Local.with_ymd_and_hms(y, mo, d, h, mi, s).single().unwrap()
}

/// A minimal JPEG (SOI + JFIF APP0 + EOI) with no EXIF.
pub fn jpeg_without_exif() -> Vec<u8> {
    let mut d = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    d.extend_from_slice(b"JFIF\x00");
    d.extend_from_slice(&[0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);
    d.extend_from_slice(&[0xFF, 0xD9]);
    d
}

/// A minimal JPEG carrying `DateTimeOriginal` and, optionally, the camera model.
pub fn jpeg_with_exif(date: DateTime<Local>, model: Option<&str>) -> Vec<u8> {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.jpg");
    std::fs::write(&path, jpeg_without_exif()).unwrap();
    let mut meta = Metadata::new();
    meta.set_tag(ExifTag::DateTimeOriginal(
        date.format("%Y:%m:%d %H:%M:%S").to_string(),
    ));
    if let Some(m) = model {
        meta.set_tag(ExifTag::Model(m.to_string()));
    }
    meta.write_to_file(&path).unwrap();
    std::fs::read(&path).unwrap()
}

/// A header-only MP4 (`ftyp` + `moov/mvhd` v0) with the given Unix creation time.
pub fn mp4_with_creation_time(unix_secs: u64) -> Vec<u8> {
    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }
    let mac = (unix_secs + MAC_EPOCH_OFFSET) as u32;
    let mut mvhd = vec![0u8; 4];
    mvhd.extend_from_slice(&mac.to_be_bytes());
    mvhd.extend_from_slice(&mac.to_be_bytes());
    mvhd.extend_from_slice(&[0u8; 92]);

    let mut out = boxed(b"ftyp", b"isom\x00\x00\x02\x00isom");
    out.extend(boxed(b"moov", &boxed(b"mvhd", &mvhd)));
    out
}

/// Path of the committed real-device fixture.
pub fn iphone_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_iphone.mov")
}
