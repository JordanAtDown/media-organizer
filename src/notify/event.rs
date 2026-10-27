//! What a batch changed, and the event that announces it.

use chrono::{DateTime, Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Collects what one folder's pipeline changed, as it goes.
///
/// Directories are kept relative to the input and to the library, so a consumer
/// can rescan just those. Over-reporting is harmless (a scan is idempotent);
/// under-reporting would hide a change, so anything that cannot be expressed
/// relatively falls back to `"."`, the whole tree.
#[derive(Debug)]
pub struct Changes {
    input: PathBuf,
    library: PathBuf,
    touched: Touched,
}

impl Changes {
    pub fn new(input: &Path, library: &Path) -> Self {
        Self {
            input: input.to_path_buf(),
            library: library.to_path_buf(),
            touched: Touched::default(),
        }
    }

    /// A file published into the library. Its source left the input as well, so
    /// both directories are stale for whoever indexes them.
    pub fn committed(&mut self, source: &Path, final_dest: &Path) {
        self.touched.committed += 1;
        self.touched
            .library_dirs
            .insert(relative_dir(&self.library, final_dest));
        self.touched
            .input_dirs
            .insert(relative_dir(&self.input, source));
    }

    /// A duplicate removed from the input: nothing was published for it.
    pub fn deleted_source(&mut self, source: &Path) {
        self.touched.deleted_sources += 1;
        self.touched
            .input_dirs
            .insert(relative_dir(&self.input, source));
    }

    /// A pipeline that changed nothing announces nothing.
    pub fn is_empty(&self) -> bool {
        self.touched.is_empty()
    }

    pub fn into_touched(self) -> Touched {
        self.touched
    }
}

/// What has changed since the last announcement: the unit that is held while the
/// input is busy, merged batch after batch, and finally turned into an event.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Touched {
    pub committed: usize,
    pub deleted_sources: usize,
    pub library_dirs: BTreeSet<String>,
    pub input_dirs: BTreeSet<String>,
}

impl Touched {
    pub fn is_empty(&self) -> bool {
        self.committed == 0 && self.deleted_sources == 0
    }

    pub fn merge(&mut self, other: Touched) {
        self.committed += other.committed;
        self.deleted_sources += other.deleted_sources;
        self.library_dirs.extend(other.library_dirs);
        self.input_dirs.extend(other.input_dirs);
    }

    pub fn into_event(self, folder: &str, seq: u64, finished_at: DateTime<Local>) -> BatchEvent {
        BatchEvent {
            folder: folder.to_string(),
            seq,
            finished_at: finished_at.to_rfc3339_opts(SecondsFormat::Secs, false),
            committed: self.committed,
            deleted_sources: self.deleted_sources,
            library_dirs: self.library_dirs.into_iter().collect(),
            input_dirs: self.input_dirs.into_iter().collect(),
        }
    }
}

/// The directory holding `path`, relative to `root`, `/`-separated; `"."` for the
/// root itself.
fn relative_dir(root: &Path, path: &Path) -> String {
    path.parent()
        .and_then(|dir| dir.strip_prefix(root).ok())
        .and_then(Path::to_str)
        .map_or_else(
            || ".".to_string(),
            |rel| {
                if rel.is_empty() {
                    ".".into()
                } else {
                    rel.into()
                }
            },
        )
}

/// The event as published, in the order a reader expects. `seq` grows with every
/// batch of a folder, so a consumer that sees a gap knows it missed one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchEvent {
    pub folder: String,
    pub seq: u64,
    /// RFC 3339 with the local offset, to the second.
    pub finished_at: String,
    /// Files published into the library since the previous event — every batch the
    /// announcement waited through (including ones recovered from a journal).
    pub committed: usize,
    /// Duplicate sources removed from the input (`dedup.delete_source_duplicates`).
    /// Every committed file also leaves the input, but is counted in `committed`.
    pub deleted_sources: usize,
    pub library_dirs: Vec<String>,
    pub input_dirs: Vec<String>,
}

impl BatchEvent {
    /// Compact JSON and a trailing newline.
    pub fn to_json(&self) -> Vec<u8> {
        let mut json = serde_json::to_vec(self).expect("a plain struct always serializes");
        json.push(b'\n');
        json
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn changes() -> Changes {
        Changes::new(Path::new("/in/jordan"), Path::new("/lib/jordan"))
    }

    fn event(c: Changes, seq: u64, at: DateTime<Local>) -> BatchEvent {
        c.into_touched().into_event("jordan", seq, at)
    }

    #[test]
    fn a_fresh_batch_is_empty() {
        assert!(changes().is_empty());
    }

    #[test]
    fn a_committed_file_marks_both_sides() {
        let mut c = changes();
        c.committed(
            Path::new("/in/jordan/trip/IMG.jpg"),
            Path::new("/lib/jordan/2026/10/IMG.jpg"),
        );
        assert!(!c.is_empty());
        let e = event(c, 1, Local::now());
        assert_eq!((e.committed, e.deleted_sources), (1, 0));
        assert_eq!(e.library_dirs, ["2026/10"]);
        assert_eq!(e.input_dirs, ["trip"]);
    }

    #[test]
    fn a_deleted_source_marks_only_the_input() {
        let mut c = changes();
        c.deleted_source(Path::new("/in/jordan/a/b/dup.jpg"));
        assert!(!c.is_empty());
        let e = event(c, 1, Local::now());
        assert_eq!((e.committed, e.deleted_sources), (0, 1));
        assert!(e.library_dirs.is_empty());
        assert_eq!(e.input_dirs, ["a/b"]);
    }

    #[test]
    fn directories_are_deduplicated_and_sorted() {
        let mut c = changes();
        for (src, dst) in [
            ("/in/jordan/b.jpg", "/lib/jordan/2026/10/b.jpg"),
            ("/in/jordan/a.jpg", "/lib/jordan/2026/09/a.jpg"),
            ("/in/jordan/c.jpg", "/lib/jordan/2026/10/c.jpg"),
        ] {
            c.committed(Path::new(src), Path::new(dst));
        }
        let e = event(c, 1, Local::now());
        assert_eq!(e.committed, 3);
        assert_eq!(e.library_dirs, ["2026/09", "2026/10"]);
        assert_eq!(e.input_dirs, ["."]);
    }

    #[test]
    fn the_roots_themselves_are_a_dot() {
        let mut c = changes();
        c.committed(
            Path::new("/in/jordan/IMG.jpg"),
            Path::new("/lib/jordan/IMG.jpg"),
        );
        let e = event(c, 1, Local::now());
        assert_eq!(
            (e.library_dirs, e.input_dirs),
            (vec![".".into()], vec![".".into()])
        );
    }

    #[test]
    fn a_path_outside_its_root_widens_to_the_whole_tree() {
        let mut c = changes();
        c.committed(
            Path::new("/elsewhere/IMG.jpg"),
            Path::new("/lib/other/IMG.jpg"),
        );
        let e = event(c, 1, Local::now());
        assert_eq!(
            (e.library_dirs, e.input_dirs),
            (vec![".".into()], vec![".".into()])
        );
    }

    #[test]
    fn the_wire_format_is_the_documented_one() {
        let mut c = changes();
        c.committed(
            Path::new("/in/jordan/IMG.jpg"),
            Path::new("/lib/jordan/2026/10/IMG.jpg"),
        );
        c.deleted_source(Path::new("/in/jordan/dup.jpg"));
        let at = Local.with_ymd_and_hms(2026, 10, 27, 23, 7, 11).unwrap();
        let json = String::from_utf8(event(c, 42, at).to_json()).unwrap();

        // The offset is the machine's: compare around it.
        let (head, tail) = json.split_once("\"committed\"").unwrap();
        assert!(
            head.starts_with(r#"{"folder":"jordan","seq":42,"finished_at":"2026-10-27T23:07:11"#),
            "{json}"
        );
        assert_eq!(
            tail,
            ":1,\"deleted_sources\":1,\"library_dirs\":[\"2026/10\"],\"input_dirs\":[\".\"]}\n"
        );
    }

    #[test]
    fn an_event_round_trips() {
        let mut c = changes();
        c.committed(
            Path::new("/in/jordan/a.jpg"),
            Path::new("/lib/jordan/2026/10/a.jpg"),
        );
        let sent = event(c, 7, Local::now());
        let back: BatchEvent = serde_json::from_slice(&sent.to_json()).unwrap();
        assert_eq!(back, sent);
    }

    #[test]
    fn touched_batches_merge_into_one() {
        let mut first = changes();
        first.committed(
            Path::new("/in/jordan/a.jpg"),
            Path::new("/lib/jordan/2026/09/a.jpg"),
        );
        let mut second = changes();
        second.committed(
            Path::new("/in/jordan/trip/b.jpg"),
            Path::new("/lib/jordan/2026/10/b.jpg"),
        );
        second.deleted_source(Path::new("/in/jordan/trip/dup.jpg"));

        let mut held = first.into_touched();
        held.merge(second.into_touched());

        assert_eq!((held.committed, held.deleted_sources), (2, 1));
        let e = held.into_event("jordan", 1, Local::now());
        assert_eq!(e.library_dirs, ["2026/09", "2026/10"]);
        assert_eq!(e.input_dirs, [".", "trip"]);
    }

    #[test]
    fn touched_round_trips_through_json() {
        let mut c = changes();
        c.committed(
            Path::new("/in/jordan/a.jpg"),
            Path::new("/lib/jordan/2026/09/a.jpg"),
        );
        let touched = c.into_touched();
        let back: Touched = serde_json::from_slice(&serde_json::to_vec(&touched).unwrap()).unwrap();
        assert_eq!(back, touched);
    }
}
