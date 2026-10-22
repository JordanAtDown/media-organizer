//! PHASE 3 — resolution. Sequential and in memory.
//!
//! Decides, for the whole batch at once, which files are duplicates and which
//! final name every other file gets. Because the batch and the existing tree are
//! both known here, names are allocated from a single in-memory table: there is
//! no `exists()`-then-`rename()` window, so the old suffix race cannot happen.
//! The only filesystem access is `stat` on destination candidates and the reads
//! needed to hash same-size files.

use crate::config::{DedupConfig, OnConflict};
use crate::dedup::{Digester, FileKey};
use crate::naming::with_suffix;
use crate::plan::FilePlan;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Upper bound on `_N` suffixes tried for one destination.
const MAX_SUFFIX: u32 = 9999;

/// A file that will be organized.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub plan: FilePlan,
    /// Absolute final path under `output`.
    pub final_dest: PathBuf,
    /// `on_conflict = overwrite` replaces an existing file at `final_dest`.
    pub overwrite: bool,
}

/// A source whose content is already present at `of`. It stays in place.
#[derive(Debug, Clone)]
pub struct Duplicate {
    pub source: PathBuf,
    pub mtime: u64,
    pub of: PathBuf,
}

#[derive(Debug, Default)]
pub struct Resolution {
    pub resolved: Vec<Resolved>,
    pub duplicates: Vec<Duplicate>,
    /// `on_conflict = skip`: `(source, existing destination)`.
    pub skipped: Vec<(PathBuf, PathBuf)>,
    /// Files left alone because a read failed: `(source, message)`.
    pub failed: Vec<(PathBuf, String)>,
}

/// Who holds a destination name: another file of this batch, or the disk.
struct Occupant {
    key: FileKey,
    in_batch: bool,
}

pub fn resolve(
    mut plans: Vec<FilePlan>,
    output: &Path,
    on_conflict: OnConflict,
    dedup: &crate::config::DedupConfig,
    digester: &mut Digester,
) -> Resolution {
    // Sorted by source path, so the same batch always yields the same names.
    plans.sort_by(|a, b| a.source.cmp(&b.source));

    if dedup.enabled {
        prefetch_digests(&plans, output, dedup, digester);
    }

    let mut claimed: HashMap<PathBuf, FileKey> = HashMap::new();
    let mut out = Resolution::default();

    'files: for plan in plans {
        let desired = output.join(&plan.rel_dest);
        let source_key = FileKey {
            path: plan.source.clone(),
            size: plan.size,
            mtime: plan.mtime,
        };

        for attempt in 0..=MAX_SUFFIX {
            let name = if attempt == 0 {
                desired.clone()
            } else {
                with_suffix(&desired, attempt)
            };

            let Some(occupant) = occupant_of(&name, &claimed) else {
                claimed.insert(name.clone(), source_key);
                out.resolved.push(Resolved {
                    plan,
                    final_dest: name,
                    overwrite: false,
                });
                continue 'files;
            };

            if dedup.enabled && (occupant.in_batch || dedup.check_existing) {
                match same_content(&source_key, &occupant.key, digester) {
                    Ok(true) => {
                        out.duplicates.push(Duplicate {
                            source: plan.source,
                            mtime: plan.mtime,
                            of: name,
                        });
                        continue 'files;
                    }
                    Ok(false) => {}
                    Err(msg) => {
                        out.failed.push((plan.source, msg));
                        continue 'files;
                    }
                }
            }

            // Different content (or dedup off) wants the same name.
            match on_conflict {
                OnConflict::Rename => {}
                OnConflict::Skip => {
                    out.skipped.push((plan.source, name));
                    continue 'files;
                }
                // Replacing a file of this very batch would silently lose it:
                // overwrite only applies to what is already on disk.
                OnConflict::Overwrite if !occupant.in_batch => {
                    claimed.insert(name.clone(), source_key);
                    out.resolved.push(Resolved {
                        plan,
                        final_dest: name,
                        overwrite: true,
                    });
                    continue 'files;
                }
                OnConflict::Overwrite => {}
            }
        }

        out.failed.push((
            plan.source,
            format!(
                "no free name under {} after {MAX_SUFFIX} attempts",
                desired.display()
            ),
        ));
    }
    out
}

fn occupant_of(name: &Path, claimed: &HashMap<PathBuf, FileKey>) -> Option<Occupant> {
    if let Some(key) = claimed.get(name) {
        return Some(Occupant {
            key: key.clone(),
            in_batch: true,
        });
    }
    // A file on disk always occupies its name; `check_existing` only decides
    // whether its content is compared.
    disk_key(name).map(|key| Occupant {
        key,
        in_batch: false,
    })
}

fn disk_key(path: &Path) -> Option<FileKey> {
    let meta = std::fs::metadata(path).ok()?;
    meta.is_file().then(|| FileKey {
        path: path.to_path_buf(),
        size: meta.len(),
        mtime: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs()),
    })
}

/// Size first: different sizes can never be identical, and no hash is computed.
fn same_content(a: &FileKey, b: &FileKey, digester: &mut Digester) -> Result<bool, String> {
    if a.size != b.size {
        return Ok(false);
    }
    let da = digester.digest(a).map_err(|e| e.to_string())?;
    let db = digester.digest(b).map_err(|e| e.to_string())?;
    Ok(da == db)
}

/// Warm the digest index in parallel for the likely collisions: files whose
/// *first-choice* destination is occupied by a same-size file. Later `_N` probes
/// are rare and hash on demand. This only reorders reads; decisions are unchanged.
fn prefetch_digests(plans: &[FilePlan], output: &Path, dedup: &DedupConfig, d: &mut Digester) {
    let mut by_dest: HashMap<PathBuf, Vec<FileKey>> = HashMap::new();
    let mut wanted = Vec::new();

    for plan in plans {
        let dest = output.join(&plan.rel_dest);
        let key = FileKey {
            path: plan.source.clone(),
            size: plan.size,
            mtime: plan.mtime,
        };
        by_dest.entry(dest).or_default().push(key);
    }
    for (dest, keys) in &by_dest {
        let on_disk = dedup.check_existing.then(|| disk_key(dest)).flatten();
        let mut group: Vec<&FileKey> = keys.iter().collect();
        if let Some(disk) = &on_disk {
            wanted.extend(
                keys.iter()
                    .filter(|k| k.size == disk.size)
                    .cloned()
                    .chain(std::iter::once(disk.clone())),
            );
            group.clear();
        }
        // Intra-batch: only files sharing both destination and size can collide.
        for (i, a) in group.iter().enumerate() {
            if group
                .iter()
                .enumerate()
                .any(|(j, b)| i != j && a.size == b.size)
            {
                wanted.push((*a).clone());
            }
        }
    }
    d.digest_many(&wanted);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DedupConfig;
    use crate::date_reader::{CaptureInfo, DateSource};
    use crate::state::HashIndex;
    use chrono::Local;

    struct Fixture {
        input: tempfile::TempDir,
        output: tempfile::TempDir,
        state: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                input: tempfile::tempdir().unwrap(),
                output: tempfile::tempdir().unwrap(),
                state: tempfile::tempdir().unwrap(),
            }
        }

        fn digester(&self) -> Digester {
            Digester::new(HashIndex::load(self.state.path()))
        }

        fn plan(&self, name: &str, content: &[u8], rel_dest: &str) -> FilePlan {
            let source = self.input.path().join(name);
            std::fs::write(&source, content).unwrap();
            FilePlan {
                source,
                size: content.len() as u64,
                mtime: 1,
                capture: CaptureInfo {
                    date: Local::now(),
                    source: DateSource::Filename,
                    camera: None,
                },
                rel_dest: PathBuf::from(rel_dest),
                inject_exif: false,
            }
        }

        fn existing(&self, rel: &str, content: &[u8]) -> PathBuf {
            let p = self.output.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }

        fn run(&self, plans: Vec<FilePlan>, on_conflict: OnConflict) -> (Resolution, Digester) {
            let mut d = self.digester();
            let r = resolve(
                plans,
                self.output.path(),
                on_conflict,
                &DedupConfig::default(),
                &mut d,
            );
            (r, d)
        }

        fn names(&self, r: &Resolution) -> Vec<String> {
            r.resolved
                .iter()
                .map(|x| {
                    x.final_dest
                        .strip_prefix(self.output.path())
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }
    }

    #[test]
    fn distinct_destinations_pass_through_without_hashing() {
        let f = Fixture::new();
        let plans = vec![
            f.plan("a.jpg", b"aaa", "2025/a.jpg"),
            f.plan("b.jpg", b"bbbb", "2025/b.jpg"),
        ];
        let (r, d) = f.run(plans, OnConflict::Rename);
        assert_eq!(r.resolved.len(), 2);
        assert!(r.duplicates.is_empty());
        assert_eq!(d.hashes_computed(), 0);
    }

    #[test]
    fn different_sizes_on_one_name_get_a_suffix_without_hashing() {
        let f = Fixture::new();
        let plans = vec![
            f.plan("a.jpg", b"short", "x/p.jpg"),
            f.plan("b.jpg", b"much longer", "x/p.jpg"),
        ];
        let (r, d) = f.run(plans, OnConflict::Rename);
        assert_eq!(f.names(&r), vec!["x/p.jpg", "x/p_1.jpg"]);
        assert_eq!(d.hashes_computed(), 0);
    }

    #[test]
    fn same_size_different_content_is_not_a_duplicate() {
        let f = Fixture::new();
        let plans = vec![
            f.plan("a.jpg", b"aaaa", "x/p.jpg"),
            f.plan("b.jpg", b"aaab", "x/p.jpg"),
        ];
        let (r, _) = f.run(plans, OnConflict::Rename);
        assert!(r.duplicates.is_empty());
        assert_eq!(f.names(&r), vec!["x/p.jpg", "x/p_1.jpg"]);
    }

    #[test]
    fn intra_batch_duplicate_is_detected_and_not_suffixed() {
        let f = Fixture::new();
        let plans = vec![
            f.plan("a.jpg", b"same", "x/p.jpg"),
            f.plan("b.jpg", b"same", "x/p.jpg"),
        ];
        let (r, _) = f.run(plans, OnConflict::Rename);
        assert_eq!(f.names(&r), vec!["x/p.jpg"]);
        assert_eq!(r.duplicates.len(), 1);
        assert!(r.duplicates[0].source.ends_with("b.jpg"));
        assert!(r.duplicates[0].of.ends_with("x/p.jpg"));
    }

    #[test]
    fn duplicate_of_an_existing_file_is_left_alone() {
        let f = Fixture::new();
        let existing = f.existing("x/p.jpg", b"same");
        let (r, _) = f.run(
            vec![f.plan("a.jpg", b"same", "x/p.jpg")],
            OnConflict::Rename,
        );
        assert!(r.resolved.is_empty());
        assert_eq!(r.duplicates[0].of, existing);
    }

    #[test]
    fn duplicate_of_an_existing_suffixed_variant_is_found() {
        let f = Fixture::new();
        f.existing("x/p.jpg", b"first");
        let variant = f.existing("x/p_1.jpg", b"second");
        let (r, _) = f.run(
            vec![f.plan("a.jpg", b"second", "x/p.jpg")],
            OnConflict::Rename,
        );
        assert!(r.resolved.is_empty());
        assert_eq!(r.duplicates[0].of, variant);
    }

    #[test]
    fn new_content_skips_taken_suffixes_on_disk() {
        let f = Fixture::new();
        f.existing("x/p.jpg", b"one");
        f.existing("x/p_1.jpg", b"two");
        let (r, _) = f.run(
            vec![f.plan("a.jpg", b"three!", "x/p.jpg")],
            OnConflict::Rename,
        );
        assert_eq!(f.names(&r), vec!["x/p_2.jpg"]);
    }

    #[test]
    fn skip_leaves_the_newcomer_in_place() {
        let f = Fixture::new();
        f.existing("x/p.jpg", b"one");
        let (r, _) = f.run(vec![f.plan("a.jpg", b"other", "x/p.jpg")], OnConflict::Skip);
        assert!(r.resolved.is_empty());
        assert_eq!(r.skipped.len(), 1);
    }

    #[test]
    fn overwrite_replaces_what_is_on_disk() {
        let f = Fixture::new();
        f.existing("x/p.jpg", b"one");
        let (r, _) = f.run(
            vec![f.plan("a.jpg", b"other", "x/p.jpg")],
            OnConflict::Overwrite,
        );
        assert_eq!(f.names(&r), vec!["x/p.jpg"]);
        assert!(r.resolved[0].overwrite);
    }

    #[test]
    fn overwrite_never_discards_a_file_of_the_same_batch() {
        let f = Fixture::new();
        let plans = vec![
            f.plan("a.jpg", b"first", "x/p.jpg"),
            f.plan("b.jpg", b"second!", "x/p.jpg"),
        ];
        let (r, _) = f.run(plans, OnConflict::Overwrite);
        assert_eq!(f.names(&r), vec!["x/p.jpg", "x/p_1.jpg"]);
    }

    #[test]
    fn allocation_is_deterministic_whatever_the_input_order() {
        let f = Fixture::new();
        let a = f.plan("a.jpg", b"1", "x/p.jpg");
        let b = f.plan("b.jpg", b"22", "x/p.jpg");
        let c = f.plan("c.jpg", b"333", "x/p.jpg");

        let (r1, _) = f.run(vec![a.clone(), b.clone(), c.clone()], OnConflict::Rename);
        let (r2, _) = f.run(vec![c, a, b], OnConflict::Rename);
        let pairs = |r: &Resolution| -> Vec<(PathBuf, PathBuf)> {
            r.resolved
                .iter()
                .map(|x| (x.plan.source.clone(), x.final_dest.clone()))
                .collect()
        };
        assert_eq!(pairs(&r1), pairs(&r2));
    }

    #[test]
    fn check_existing_off_renames_instead_of_comparing_with_disk() {
        let f = Fixture::new();
        f.existing("x/p.jpg", b"same");
        let mut d = f.digester();
        let cfg = DedupConfig {
            enabled: true,
            check_existing: false,
        };
        let plans = vec![f.plan("a.jpg", b"same", "x/p.jpg")];
        let r = resolve(plans, f.output.path(), OnConflict::Rename, &cfg, &mut d);
        assert_eq!(f.names(&r), vec!["x/p_1.jpg"]);
        assert_eq!(d.hashes_computed(), 0);
    }

    #[test]
    fn dedup_off_never_hashes_and_renames_identical_files() {
        let f = Fixture::new();
        let plans = vec![
            f.plan("a.jpg", b"same", "x/p.jpg"),
            f.plan("b.jpg", b"same", "x/p.jpg"),
        ];
        let mut d = f.digester();
        let off = DedupConfig {
            enabled: false,
            check_existing: true,
        };
        let r = resolve(plans, f.output.path(), OnConflict::Rename, &off, &mut d);
        assert_eq!(f.names(&r), vec!["x/p.jpg", "x/p_1.jpg"]);
        assert_eq!(d.hashes_computed(), 0);
    }

    #[test]
    fn unreadable_same_size_file_is_reported_not_moved() {
        let f = Fixture::new();
        f.existing("x/p.jpg", b"same");
        let mut plan = f.plan("a.jpg", b"same", "x/p.jpg");
        std::fs::remove_file(&plan.source).unwrap();
        plan.mtime = 1;
        let (r, _) = f.run(vec![plan], OnConflict::Rename);
        assert!(r.resolved.is_empty() && r.duplicates.is_empty());
        assert_eq!(r.failed.len(), 1);
    }

    #[test]
    fn resolve_writes_nothing_to_disk() {
        let f = Fixture::new();
        f.existing("x/seed.jpg", b"seed");
        let before = walk_count(f.output.path());
        f.run(vec![f.plan("a.jpg", b"new", "y/p.jpg")], OnConflict::Rename);
        assert_eq!(walk_count(f.output.path()), before);
    }

    fn walk_count(p: &Path) -> usize {
        walkdir::WalkDir::new(p).into_iter().count()
    }
}
