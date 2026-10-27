//! What the notifier remembers between scans and restarts: per folder, the last
//! event announced and the changes still waiting to be.
//!
//! Two reasons to persist:
//!
//! * `seq` must never go backwards. A consumer decides "did I miss one?" by
//!   comparing it with the last it handled: a counter that restarted at 1 after a
//!   redeploy would make it ignore every new event. The number is therefore
//!   recorded *before* the event is published; a crash in between leaves a gap,
//!   never a reuse.
//! * An announcement waits for the input to go quiet, possibly for minutes. A
//!   restart in the meantime must not forget what it was holding.

use super::event::Touched;
use crate::fsops;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use tracing::warn;

pub const NOTIFY_FILE: &str = "notify.json";

/// The last batch announced for a folder; also what `/healthz` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastEvent {
    pub seq: u64,
    /// Unix seconds.
    pub finished_at: i64,
    pub committed: usize,
    pub deleted_sources: usize,
}

/// Changes held back until the input is quiet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub touched: Touched,
    /// Unix seconds: when the first change was held (bounds the wait).
    pub first_at: u64,
    /// Unix seconds: when the most recent one was (the quiet period counts from here).
    pub last_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FolderState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<LastEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<Pending>,
}

/// `<state_dir>/notify.json`: folder label → its state.
pub struct NotifyState {
    file: PathBuf,
    folders: BTreeMap<String, FolderState>,
}

impl NotifyState {
    /// A missing file starts empty. So does a corrupt one, with a warning: the
    /// event file already published still carries a `seq` to continue from.
    pub fn load(state_dir: &Path) -> Self {
        let file = state_dir.join(NOTIFY_FILE);
        let folders = match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!(file = %file.display(), error = %e, "ignoring corrupt state file");
                BTreeMap::new()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                warn!(file = %file.display(), error = %e, "cannot read state file");
                BTreeMap::new()
            }
        };
        Self { file, folders }
    }

    /// The last event announced per folder.
    pub fn last_events(&self) -> BTreeMap<String, LastEvent> {
        self.folders
            .iter()
            .filter_map(|(folder, s)| Some((folder.clone(), s.last.clone()?)))
            .collect()
    }

    pub fn pending(&self, folder: &str) -> Option<&Pending> {
        self.folders.get(folder)?.pending.as_ref()
    }

    /// The next number for `folder`: above everything known, including `floor`
    /// (the `seq` of an event file found on disk, in case this file was lost).
    pub fn next_seq(&self, folder: &str, floor: u64) -> u64 {
        let known = self
            .folders
            .get(folder)
            .and_then(|s| s.last.as_ref())
            .map_or(0, |l| l.seq);
        known.max(floor) + 1
    }

    /// Hold `touched` for `folder`, merged with what is already waiting. The wait
    /// is bounded from the first change and the quiet period restarts at `now`.
    ///
    /// The in-memory state is updated even when the write fails: this process
    /// still announces what it holds, only a restart would forget it.
    pub fn hold(&mut self, folder: &str, touched: Touched, now: u64) -> io::Result<()> {
        let state = self.folders.entry(folder.to_string()).or_default();
        match &mut state.pending {
            Some(pending) => {
                pending.touched.merge(touched);
                pending.last_at = now;
            }
            None => {
                state.pending = Some(Pending {
                    touched,
                    first_at: now,
                    last_at: now,
                });
            }
        }
        self.save()
    }

    /// Record `last` as the folder's last event and drop what was waiting: it is
    /// now part of that event. Same in-memory rule as [`NotifyState::hold`].
    pub fn announced(&mut self, folder: &str, last: LastEvent) -> io::Result<()> {
        let state = self.folders.entry(folder.to_string()).or_default();
        state.last = Some(last);
        state.pending = None;
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec(&self.folders).map_err(io::Error::other)?;
        let dir = self.file.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        fsops::write_atomic(&self.file, dir, &bytes, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::Changes;

    fn event(seq: u64) -> LastEvent {
        LastEvent {
            seq,
            finished_at: 1_792_687_600,
            committed: 2,
            deleted_sources: 1,
        }
    }

    fn touched(dest_dir: &str) -> Touched {
        let mut c = Changes::new(Path::new("/in"), Path::new("/lib"));
        c.committed(
            Path::new("/in/a.jpg"),
            &Path::new("/lib").join(dest_dir).join("a.jpg"),
        );
        c.into_touched()
    }

    #[test]
    fn numbers_start_at_one_and_grow_per_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = NotifyState::load(dir.path());
        assert_eq!(state.next_seq("jordan", 0), 1);
        state.announced("jordan", event(1)).unwrap();
        assert_eq!(state.next_seq("jordan", 0), 2);
        assert_eq!(
            state.next_seq("sylvie", 0),
            1,
            "each folder counts on its own"
        );
    }

    #[test]
    fn the_counter_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = NotifyState::load(dir.path());
        state.announced("jordan", event(41)).unwrap();

        let reloaded = NotifyState::load(dir.path());
        assert_eq!(reloaded.next_seq("jordan", 0), 42);
        assert_eq!(reloaded.last_events()["jordan"], event(41));
    }

    #[test]
    fn an_event_file_on_disk_raises_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let state = NotifyState::load(dir.path());
        assert_eq!(state.next_seq("jordan", 99), 100);
    }

    #[test]
    fn held_changes_merge_and_keep_the_first_moment() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = NotifyState::load(dir.path());
        state.hold("jordan", touched("2026/09"), 1_000).unwrap();
        state.hold("jordan", touched("2026/10"), 1_040).unwrap();

        let pending = state.pending("jordan").unwrap();
        assert_eq!((pending.first_at, pending.last_at), (1_000, 1_040));
        assert_eq!(pending.touched.committed, 2);
        assert_eq!(
            pending.touched.library_dirs.iter().collect::<Vec<_>>(),
            ["2026/09", "2026/10"]
        );
    }

    #[test]
    fn held_changes_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        NotifyState::load(dir.path())
            .hold("jordan", touched("2026/09"), 1_000)
            .unwrap();

        let reloaded = NotifyState::load(dir.path());
        assert_eq!(reloaded.pending("jordan").unwrap().last_at, 1_000);
        assert!(reloaded.last_events().is_empty(), "held is not announced");
    }

    #[test]
    fn announcing_clears_what_was_held() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = NotifyState::load(dir.path());
        state.hold("jordan", touched("2026/09"), 1_000).unwrap();
        state.announced("jordan", event(1)).unwrap();

        assert!(state.pending("jordan").is_none());
        let reloaded = NotifyState::load(dir.path());
        assert!(reloaded.pending("jordan").is_none());
        assert_eq!(reloaded.last_events()["jordan"].seq, 1);
    }

    #[test]
    fn a_corrupt_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(NOTIFY_FILE), b"{not json").unwrap();
        let state = NotifyState::load(dir.path());
        assert!(state.last_events().is_empty());
        assert_eq!(state.next_seq("jordan", 0), 1);
    }

    #[test]
    fn a_failed_write_still_updates_this_process() {
        let dir = tempfile::tempdir().unwrap();
        // The state "directory" is a file: nothing can be written under it.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let mut state = NotifyState::load(&blocker);

        assert!(state.hold("jordan", touched("2026/09"), 1_000).is_err());
        assert!(state.pending("jordan").is_some());
        assert!(state.announced("jordan", event(1)).is_err());
        assert_eq!(state.next_seq("jordan", 0), 2);
    }
}
