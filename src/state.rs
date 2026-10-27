//! Persistent state kept in `state_dir`: three small path-keyed JSON caches.
//!
//! They are plain `HashMap`s with no locking: only the sequential phases of the
//! pipeline (inventory, resolution, commit) ever touch them.

use crate::fsops;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use tracing::warn;

pub const NO_DATE_FILE: &str = "no_date_cache.json";
pub const DUPLICATES_FILE: &str = "duplicates.json";
pub const HASH_INDEX_FILE: &str = "hash_index.json";

/// A JSON file holding `path → V`, written atomically.
struct Store<V> {
    file: PathBuf,
    entries: HashMap<PathBuf, V>,
    dirty: bool,
}

impl<V: Serialize + DeserializeOwned> Store<V> {
    /// A missing or corrupt file starts empty: these are caches, losing one
    /// only costs a rescan, so it must never prevent the daemon from starting.
    fn load(file: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!(file = %file.display(), error = %e, "ignoring corrupt state file");
                HashMap::new()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!(file = %file.display(), error = %e, "cannot read state file");
                HashMap::new()
            }
        };
        Self {
            file,
            entries,
            dirty: false,
        }
    }

    fn insert(&mut self, path: PathBuf, value: V) {
        self.entries.insert(path, value);
        self.dirty = true;
    }

    fn remove(&mut self, path: &Path) {
        if self.entries.remove(path).is_some() {
            self.dirty = true;
        }
    }

    /// Write via a sibling temp file and `rename`, so a crash never leaves a
    /// half-written cache.
    fn save_if_dirty(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&self.entries).map_err(io::Error::other)?;
        let dir = self.file.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        fsops::write_atomic(&self.file, dir, &bytes, None)?;
        self.dirty = false;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// No-date cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NoDateEntry {
    mtime: u64,
    recorded_at: u64,
}

/// Files that could not be dated, so they are logged once, not every scan.
pub struct NoDateCache {
    store: Store<NoDateEntry>,
    enabled: bool,
    ttl_days: u32,
}

impl NoDateCache {
    pub fn load(state_dir: &Path, enabled: bool, ttl_days: u32) -> Self {
        Self {
            store: Store::load(state_dir.join(NO_DATE_FILE)),
            enabled,
            ttl_days,
        }
    }

    /// Skip when remembered, the mtime is unchanged, and the TTL (if any) is unexpired.
    pub fn should_skip(&self, path: &Path, mtime: u64, now: u64) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(e) = self.store.entries.get(path) else {
            return false;
        };
        let expired = self.ttl_days > 0
            && now.saturating_sub(e.recorded_at) > u64::from(self.ttl_days) * 86_400;
        e.mtime == mtime && !expired
    }

    pub fn insert(&mut self, path: PathBuf, mtime: u64, now: u64) {
        if self.enabled {
            self.store.insert(
                path,
                NoDateEntry {
                    mtime,
                    recorded_at: now,
                },
            );
        }
    }

    pub fn remove(&mut self, path: &Path) {
        self.store.remove(path);
    }

    pub fn len(&self) -> usize {
        self.store.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.entries.is_empty()
    }

    pub fn save_if_dirty(&mut self) -> io::Result<()> {
        self.store.save_if_dirty()
    }
}

// ---------------------------------------------------------------------------
// Duplicate cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DuplicateEntry {
    mtime: u64,
    /// The already-organized file this source duplicates.
    of: PathBuf,
}

/// Source files known to duplicate an organized file. They stay in `input`
/// (nothing is ever deleted automatically) and are not re-hashed each scan.
pub struct DuplicateCache {
    store: Store<DuplicateEntry>,
}

impl DuplicateCache {
    pub fn load(state_dir: &Path) -> Self {
        Self {
            store: Store::load(state_dir.join(DUPLICATES_FILE)),
        }
    }

    /// `Some(existing)` while the source is unchanged since it was recorded.
    pub fn lookup(&self, path: &Path, mtime: u64) -> Option<&Path> {
        let e = self.store.entries.get(path)?;
        (e.mtime == mtime).then_some(e.of.as_path())
    }

    pub fn insert(&mut self, path: PathBuf, mtime: u64, of: PathBuf) {
        self.store.insert(path, DuplicateEntry { mtime, of });
    }

    pub fn remove(&mut self, path: &Path) {
        self.store.remove(path);
    }

    pub fn len(&self) -> usize {
        self.store.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.entries.is_empty()
    }

    pub fn save_if_dirty(&mut self) -> io::Result<()> {
        self.store.save_if_dirty()
    }
}

// ---------------------------------------------------------------------------
// Hash index
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HashEntry {
    size: u64,
    mtime: u64,
    hash: String,
}

/// Digests of files already hashed, so a ~49 000-file library is not re-read
/// on every scan. An entry is valid only while size and mtime are unchanged.
pub struct HashIndex {
    store: Store<HashEntry>,
}

impl HashIndex {
    pub fn load(state_dir: &Path) -> Self {
        Self {
            store: Store::load(state_dir.join(HASH_INDEX_FILE)),
        }
    }

    pub fn get(&self, path: &Path, size: u64, mtime: u64) -> Option<&str> {
        let e = self.store.entries.get(path)?;
        (e.size == size && e.mtime == mtime).then_some(e.hash.as_str())
    }

    pub fn insert(&mut self, path: PathBuf, size: u64, mtime: u64, hash: String) {
        self.store.insert(path, HashEntry { size, mtime, hash });
    }

    pub fn len(&self) -> usize {
        self.store.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.entries.is_empty()
    }

    pub fn save_if_dirty(&mut self) -> io::Result<()> {
        self.store.save_if_dirty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn no_date_skips_until_mtime_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = NoDateCache::load(dir.path(), true, 0);
        c.insert(p("/a.jpg"), 100, 1_000);
        assert!(c.should_skip(Path::new("/a.jpg"), 100, 999_999_999));
        assert!(!c.should_skip(Path::new("/a.jpg"), 101, 1_000));
        assert!(!c.should_skip(Path::new("/other.jpg"), 100, 1_000));
    }

    #[test]
    fn no_date_ttl_expires_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = NoDateCache::load(dir.path(), true, 30);
        c.insert(p("/a.jpg"), 100, 0);
        assert!(c.should_skip(Path::new("/a.jpg"), 100, 29 * 86_400));
        assert!(!c.should_skip(Path::new("/a.jpg"), 100, 31 * 86_400));
    }

    #[test]
    fn disabled_no_date_cache_never_skips_nor_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = NoDateCache::load(dir.path(), false, 0);
        c.insert(p("/a.jpg"), 100, 0);
        assert!(c.is_empty());
        assert!(!c.should_skip(Path::new("/a.jpg"), 100, 0));
    }

    #[test]
    fn no_date_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = NoDateCache::load(dir.path(), true, 0);
        c.insert(p("/a.jpg"), 100, 5);
        c.save_if_dirty().unwrap();

        let again = NoDateCache::load(dir.path(), true, 0);
        assert_eq!(again.len(), 1);
        assert!(again.should_skip(Path::new("/a.jpg"), 100, 5));
    }

    #[test]
    fn remove_forgets_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = NoDateCache::load(dir.path(), true, 0);
        c.insert(p("/a.jpg"), 100, 5);
        c.remove(Path::new("/a.jpg"));
        assert!(c.is_empty());
    }

    #[test]
    fn duplicates_are_valid_until_the_source_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = DuplicateCache::load(dir.path());
        c.insert(p("/in/a.jpg"), 100, p("/out/2025/a.jpg"));
        assert_eq!(
            c.lookup(Path::new("/in/a.jpg"), 100),
            Some(Path::new("/out/2025/a.jpg"))
        );
        assert_eq!(c.lookup(Path::new("/in/a.jpg"), 101), None);
    }

    #[test]
    fn hash_index_requires_matching_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = HashIndex::load(dir.path());
        idx.insert(p("/a"), 10, 100, "abc".into());
        assert_eq!(idx.get(Path::new("/a"), 10, 100), Some("abc"));
        assert_eq!(idx.get(Path::new("/a"), 11, 100), None);
        assert_eq!(idx.get(Path::new("/a"), 10, 101), None);
    }

    #[test]
    fn corrupt_file_starts_empty_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(HASH_INDEX_FILE), b"{not json").unwrap();
        assert!(HashIndex::load(dir.path()).is_empty());
    }

    #[test]
    fn clean_cache_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = DuplicateCache::load(dir.path());
        c.save_if_dirty().unwrap();
        assert!(!dir.path().join(DUPLICATES_FILE).exists());
    }

    #[test]
    fn save_creates_the_state_dir_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("nested").join("state");
        let mut c = DuplicateCache::load(&state);
        c.insert(p("/a"), 1, p("/b"));
        c.save_if_dirty().unwrap();
        let names: Vec<_> = std::fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(DUPLICATES_FILE)]);
    }
}
