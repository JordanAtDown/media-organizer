//! The event file: `<dir>/<folder>.json`, overwritten at each batch.
//!
//! A reader (typically a systemd `.path` unit watching `dir`) must never see a
//! truncated JSON, nor be woken before the content is complete. The event is
//! written to `<dir>/.tmp/` and renamed into place: a directory watch sees one
//! event, at the rename. A temporary file created in `dir` itself would wake it
//! at creation.

use crate::config::Mode;
use crate::fsops;
use serde::Deserialize;
use std::io;
use std::path::Path;

/// Subdirectory holding the temporary files, on the same filesystem as `dir`.
pub const TMP_DIR: &str = ".tmp";

/// Readable by the consumer whatever the process umask, which may run as another
/// uid than the unit that reads the event.
const FILE_MODE: Mode = Mode(0o644);

fn event_path(dir: &Path, slug: &str) -> std::path::PathBuf {
    dir.join(format!("{slug}.json"))
}

/// Publish `json` as `<dir>/<slug>.json`, creating `dir` when missing.
pub fn publish(dir: &Path, slug: &str, json: &[u8]) -> io::Result<()> {
    let tmp_dir = dir.join(TMP_DIR);
    std::fs::create_dir_all(&tmp_dir)?;
    fsops::write_atomic(&event_path(dir, slug), &tmp_dir, json, Some(FILE_MODE))
}

/// Check that `dir` can receive events, so a bad setting shows at startup rather
/// than at the first batch.
pub fn check_writable(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir.join(TMP_DIR))
}

/// The `seq` of the event last published for `slug`, when there is a readable one.
pub fn last_seq(dir: &Path, slug: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct Seq {
        seq: u64,
    }
    let text = std::fs::read_to_string(event_path(dir, slug)).ok()?;
    serde_json::from_str::<Seq>(&text).ok().map(|s| s.seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn publishes_the_json_and_leaves_only_it_in_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), "jordan", b"{\"seq\":1}\n").unwrap();

        assert_eq!(
            std::fs::read(dir.path().join("jordan.json")).unwrap(),
            b"{\"seq\":1}\n"
        );
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [".tmp", "jordan.json"]);
        assert_eq!(
            std::fs::read_dir(dir.path().join(TMP_DIR)).unwrap().count(),
            0
        );
    }

    #[test]
    fn overwrites_the_previous_event() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), "jordan", b"{\"seq\":1}\n").unwrap();
        publish(dir.path(), "jordan", b"{\"seq\":2}\n").unwrap();
        assert_eq!(last_seq(dir.path(), "jordan"), Some(2));
    }

    #[test]
    fn creates_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("state").join("events");
        publish(&events, "jordan", b"{\"seq\":1}\n").unwrap();
        assert!(events.join("jordan.json").is_file());
    }

    #[test]
    fn the_file_is_world_readable_whatever_the_umask() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), "jordan", b"{}\n").unwrap();
        let mode = std::fs::metadata(dir.path().join("jordan.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn a_directory_that_is_a_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("events");
        std::fs::write(&blocker, b"x").unwrap();
        assert!(publish(&blocker, "jordan", b"{}\n").is_err());
        assert!(check_writable(&blocker).is_err());
    }

    #[test]
    fn last_seq_ignores_a_missing_or_garbled_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(last_seq(dir.path(), "jordan"), None);
        std::fs::write(dir.path().join("jordan.json"), b"{truncated").unwrap();
        assert_eq!(last_seq(dir.path(), "jordan"), None);
    }
}
