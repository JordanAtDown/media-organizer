//! What a scan reports: the phase it is in, and what happened to each folder.

use crate::date_reader::DateSource;
use crate::notify::Announced;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Inventory,
    Planning,
    Resolution,
    Preparation,
    Commit,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Phase::Idle => "idle",
            Phase::Inventory => "inventory",
            Phase::Planning => "planning",
            Phase::Resolution => "resolution",
            Phase::Preparation => "preparation",
            Phase::Commit => "commit",
        };
        f.write_str(s)
    }
}

/// Receives progress as a scan runs. `progress` is the heartbeat that lets a
/// supervisor tell a long scan from one wedged on a dead NFS mount.
pub trait Observer: Sync {
    fn phase(&self, _folder: &str, _phase: Phase) {}
    fn progress(&self) {}
}

pub struct NoObserver;
impl Observer for NoObserver {}

/// What happened to one folder during a scan.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FolderSummary {
    pub name: String,
    pub input_exists: bool,
    /// Candidates inventoried this scan.
    pub found: usize,
    pub organized: usize,
    pub duplicates: usize,
    /// Duplicate sources removed (`dedup.delete_source_duplicates`); a subset of `duplicates`.
    pub duplicates_removed: usize,
    pub conflicts_skipped: usize,
    pub no_date: usize,
    pub failed: usize,
    pub panics: usize,
    pub exif_injected: usize,
    pub by_date_source: BTreeMap<String, usize>,
    /// Entries left staged because a commit failed.
    pub pending_in_staging: usize,
    pub last_error: Option<String>,
    /// The batch event published for this folder, if this scan changed anything and
    /// an event sink is configured. Not part of `/healthz`'s `folders[]`: see its
    /// `notify` block.
    #[serde(skip)]
    pub announced: Option<Announced>,
}

impl FolderSummary {
    pub(super) fn count_source(&mut self, source: DateSource) {
        let key = match source {
            DateSource::Exif => "exif",
            DateSource::QuickTime => "quicktime",
            DateSource::Filename => "filename",
            DateSource::Mtime => "mtime",
        };
        *self.by_date_source.entry(key.to_string()).or_default() += 1;
    }

    pub(super) fn fail(&mut self, message: String) {
        self.failed += 1;
        self.last_error = Some(message);
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    pub folders: Vec<FolderSummary>,
}
