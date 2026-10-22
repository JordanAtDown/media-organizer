//! Content digests for duplicate detection.
//!
//! Hashing costs a full read over NFS, the one real price of the transactional
//! design, so it is contained three ways: callers hash only files whose **size
//! collides** (a `stat` is enough to know), digests are remembered in the
//! persistent [`HashIndex`], and BLAKE3 is fast enough that the Pi's cores
//! outrun the network.

use crate::error::DedupError;
use crate::state::HashIndex;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A file to digest, with the `stat` data that keys the persistent index.
#[derive(Debug, Clone)]
pub struct FileKey {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: u64,
}

/// BLAKE3 digest of a file's content, hex-encoded.
pub fn hash_file(path: &Path) -> Result<String, DedupError> {
    let hash_err = |source| DedupError::Hash {
        path: path.to_path_buf(),
        source,
    };
    let file = File::open(path).map_err(hash_err)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(file).map_err(hash_err)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Hashes files through the persistent index and counts the reads it really does.
pub struct Digester {
    index: HashIndex,
    computed: AtomicUsize,
}

impl Digester {
    pub fn new(index: HashIndex) -> Self {
        Self {
            index,
            computed: AtomicUsize::new(0),
        }
    }

    /// Files actually read and hashed (index hits excluded).
    pub fn hashes_computed(&self) -> usize {
        self.computed.load(Ordering::Relaxed)
    }

    pub fn index_len(&self) -> usize {
        self.index.len()
    }

    fn compute(&self, path: &Path) -> Result<String, DedupError> {
        self.computed.fetch_add(1, Ordering::Relaxed);
        hash_file(path)
    }

    /// Digest of one file, from the index when still valid.
    pub fn digest(&mut self, key: &FileKey) -> Result<String, DedupError> {
        if let Some(hit) = self.index.get(&key.path, key.size, key.mtime) {
            return Ok(hit.to_string());
        }
        let hash = self.compute(&key.path)?;
        self.index
            .insert(key.path.clone(), key.size, key.mtime, hash.clone());
        Ok(hash)
    }

    /// Digest many files, hashing the cache misses in parallel on the current
    /// rayon pool. Each file reports its own result, so one unreadable file
    /// never fails the batch.
    pub fn digest_many(
        &mut self,
        keys: &[FileKey],
    ) -> HashMap<PathBuf, Result<String, DedupError>> {
        let mut out = HashMap::with_capacity(keys.len());
        let mut misses = Vec::new();
        for key in keys {
            match self.index.get(&key.path, key.size, key.mtime) {
                Some(hit) => {
                    out.insert(key.path.clone(), Ok(hit.to_string()));
                }
                None => misses.push(key),
            }
        }

        let this = &*self;
        let hashed: Vec<_> = misses
            .par_iter()
            .map(|key| (*key, this.compute(&key.path)))
            .collect();

        for (key, result) in hashed {
            if let Ok(hash) = &result {
                self.index
                    .insert(key.path.clone(), key.size, key.mtime, hash.clone());
            }
            out.insert(key.path.clone(), result);
        }
        out
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        self.index.save_if_dirty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &[u8]) -> FileKey {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        FileKey {
            path,
            size: content.len() as u64,
            mtime: 1,
        }
    }

    fn digester(dir: &Path) -> Digester {
        Digester::new(HashIndex::load(dir))
    }

    #[test]
    fn identical_content_hashes_identically() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a", b"same bytes");
        let b = write(dir.path(), "b", b"same bytes");
        assert_eq!(hash_file(&a.path).unwrap(), hash_file(&b.path).unwrap());
    }

    #[test]
    fn same_size_different_content_differs() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a", b"aaaa");
        let b = write(dir.path(), "b", b"aaab");
        assert_ne!(hash_file(&a.path).unwrap(), hash_file(&b.path).unwrap());
    }

    #[test]
    fn missing_file_is_a_hash_error() {
        let err = hash_file(Path::new("/nonexistent/x")).unwrap_err();
        assert!(matches!(err, DedupError::Hash { .. }));
    }

    #[test]
    fn nothing_is_hashed_until_asked() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(digester(dir.path()).hashes_computed(), 0);
    }

    #[test]
    fn second_digest_of_an_unchanged_file_is_an_index_hit() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let key = write(dir.path(), "a", b"content");
        let mut d = digester(state.path());

        let first = d.digest(&key).unwrap();
        let second = d.digest(&key).unwrap();
        assert_eq!(first, second);
        assert_eq!(d.hashes_computed(), 1);
    }

    #[test]
    fn changed_mtime_invalidates_the_index_entry() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut key = write(dir.path(), "a", b"content");
        let mut d = digester(state.path());
        d.digest(&key).unwrap();

        key.mtime = 2;
        d.digest(&key).unwrap();
        assert_eq!(d.hashes_computed(), 2);
    }

    #[test]
    fn digest_many_hashes_each_miss_once_and_reports_failures_apart() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a", b"one");
        let b = write(dir.path(), "b", b"two");
        let ghost = FileKey {
            path: dir.path().join("ghost"),
            size: 1,
            mtime: 1,
        };
        let mut d = digester(state.path());

        let out = d.digest_many(&[a.clone(), b.clone(), ghost.clone()]);
        assert!(out[&a.path].is_ok() && out[&b.path].is_ok());
        assert!(out[&ghost.path].is_err());

        // The two good digests are now indexed: a second pass reads nothing new.
        let before = d.hashes_computed();
        d.digest_many(&[a, b]);
        assert_eq!(d.hashes_computed(), before);
    }

    #[test]
    fn index_persists_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let key = write(dir.path(), "a", b"content");

        let mut d = digester(state.path());
        d.digest(&key).unwrap();
        d.save().unwrap();

        let mut again = digester(state.path());
        again.digest(&key).unwrap();
        assert_eq!(again.hashes_computed(), 0);
    }
}
